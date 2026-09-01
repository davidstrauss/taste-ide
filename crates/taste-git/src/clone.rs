//! Local clone plumbing for environments — libgit2 only, host-side.
//!
//! Two operations, both between repositories only the IDE can see as a
//! pair: making an environment's clone of the main checkout, and asking
//! what work in a clone has *not* made it back.
//!
//! libgit2 is the point, not an implementation detail. It runs no hooks and
//! shells out to nothing, so cloning an untrusted repository — which is
//! what every repository is here — executes none of its code. The same
//! property is why phase 3's mediated publish will run through here rather
//! than through `git`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// How far back to walk a branch before giving up on precision. A clone
/// with more than this many commits ahead of the main checkout is
/// pathological; the report says "at least N" rather than walking forever
/// while a user waits on a confirmation dialog.
const WALK_CAP: usize = 1000;

/// Commits reachable from the main checkout's refs, capped. Large enough
/// for any repository whose history a person is working in, bounded so a
/// monorepo cannot turn "delete this environment" into a minute of CPU.
const PUBLISHED_CAP: usize = 200_000;

/// One branch of an environment's clone holding commits the main checkout
/// has never seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpublishedBranch {
    pub branch: String,
    /// Short id of the branch tip.
    pub tip: String,
    /// How many commits are unreachable from the main checkout's refs.
    /// Saturates at [`WALK_CAP`] (reported honestly by `truncated`).
    pub commits: usize,
    pub truncated: bool,
    /// Subject line of the newest unpublished commit — enough for a user to
    /// recognise the work in a confirmation dialog.
    pub summary: String,
}

/// Clone a local repository into `dest`.
///
/// `source` is a path, not a URL: the two repositories live on the same
/// host and the clone's `origin` deliberately points at a host path that
/// no container has mounted, so fetch and push from inside an environment
/// simply fail. The IDE is the only thing that can move refs between them.
pub fn clone_local(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;
    git2::build::RepoBuilder::new()
        .clone(&source.to_string_lossy(), dest)
        .with_context(|| format!("cloning {} into {}", source.display(), dest.display()))?;
    Ok(())
}

/// Which branches of `clone` hold commits not reachable from any ref of
/// `main`.
///
/// This is the question that has to be answered before an environment is
/// destroyed: the clone may hold the only copy of an agent's unreviewed
/// work, and `env_remove` deleting it silently would be data loss dressed
/// up as cleanup.
///
/// Reachability is computed against *every* ref of the main checkout —
/// branches, tags, remote-tracking refs, and the `agents/*` branches
/// publish creates — so anything the user could still get at counts as
/// published.
pub fn unpublished_work(clone: &Path, main: &Path) -> Result<Vec<UnpublishedBranch>> {
    let clone_repo = git2::Repository::open(clone)
        .with_context(|| format!("opening clone {}", clone.display()))?;
    let main_repo = git2::Repository::open(main)
        .with_context(|| format!("opening main checkout {}", main.display()))?;

    let published = reachable_from_refs(&main_repo)?;

    let mut out = Vec::new();
    for branch in clone_repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch?;
        let Some(name) = branch.name()?.map(str::to_string) else {
            continue;
        };
        let Ok(tip) = branch.get().peel_to_commit() else {
            continue;
        };
        if published.contains(&tip.id()) {
            continue; // fully published — the common case
        }

        let mut walk = clone_repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
        walk.push(tip.id())?;
        let mut commits = 0usize;
        let mut truncated = false;
        for oid in walk {
            let oid = oid?;
            if published.contains(&oid) {
                // Topological order: reaching a published commit means the
                // rest of this line is published too.
                break;
            }
            commits += 1;
            if commits >= WALK_CAP {
                truncated = true;
                break;
            }
        }
        if commits == 0 {
            continue;
        }
        out.push(UnpublishedBranch {
            branch: name,
            tip: tip.id().to_string().chars().take(8).collect(),
            commits,
            truncated,
            summary: tip.summary().unwrap_or("").to_string(),
        });
    }
    out.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(out)
}

/// Every commit reachable from any reference of `repo`, capped.
fn reachable_from_refs(repo: &git2::Repository) -> Result<HashSet<git2::Oid>> {
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
    for reference in repo.references()? {
        let reference = reference?;
        if let Ok(commit) = reference.peel_to_commit() {
            walk.push(commit.id())?;
        }
    }
    let mut set = HashSet::new();
    for oid in walk {
        set.insert(oid?);
        if set.len() >= PUBLISHED_CAP {
            break;
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &git2::Repository, name: &str) -> git2::Oid {
        let root = repo.workdir().unwrap().to_path_buf();
        std::fs::write(root.join(name), name).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, name, &tree, &parent_refs)
            .unwrap()
    }

    fn main_repo(dir: &Path) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        commit(&repo, "base");
        repo
    }

    #[test]
    fn clone_tracks_the_main_checkout() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        main_repo(src.path());
        let dest = dst.path().join("env/repo");

        clone_local(src.path(), &dest).unwrap();
        let clone = git2::Repository::open(&dest).unwrap();
        assert!(dest.join("base").is_file(), "worktree is checked out");
        let origin = clone.find_remote("origin").unwrap();
        assert!(
            origin.url().unwrap().contains(
                src.path()
                    .canonicalize()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            ),
            "origin points at the main checkout's path: {:?}",
            origin.url()
        );
        // Nothing unpublished in a fresh clone.
        assert!(unpublished_work(&dest, src.path()).unwrap().is_empty());
    }

    #[test]
    fn cloning_over_an_existing_directory_is_refused() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        main_repo(src.path());
        assert!(clone_local(src.path(), dst.path()).is_err());
    }

    #[test]
    fn commits_only_in_the_clone_are_reported_as_unpublished() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        main_repo(src.path());
        let dest = dst.path().join("env/repo");
        clone_local(src.path(), &dest).unwrap();

        let clone = git2::Repository::open(&dest).unwrap();
        commit(&clone, "agent-work-one");
        commit(&clone, "agent-work-two");

        let found = unpublished_work(&dest, src.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].commits, 2);
        assert_eq!(found[0].summary, "agent-work-two");
        assert!(!found[0].truncated);
    }

    /// The whole point of the check: once the work is in the main checkout
    /// — however it got there — destroying the environment loses nothing.
    #[test]
    fn work_fetched_into_the_main_checkout_stops_counting() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let main = main_repo(src.path());
        let dest = dst.path().join("env/repo");
        clone_local(src.path(), &dest).unwrap();

        let clone = git2::Repository::open(&dest).unwrap();
        let tip = commit(&clone, "agent-work");
        assert_eq!(unpublished_work(&dest, src.path()).unwrap().len(), 1);

        // Simulate publish: fetch the clone's branch into main under the
        // ref name the review inbox will use.
        let mut remote = main
            .remote_anonymous(&dest.canonicalize().unwrap().to_string_lossy())
            .unwrap();
        remote
            .fetch(&["+refs/heads/*:refs/heads/agents/env/*"], None, None)
            .unwrap();
        assert!(main.find_commit(tip).is_ok(), "object arrived");

        assert!(
            unpublished_work(&dest, src.path()).unwrap().is_empty(),
            "reachable from a main ref = published"
        );
    }
}
