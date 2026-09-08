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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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
///
/// **No hardlinks, and this is a boundary requirement rather than a
/// preference.** A local clone's default is to hardlink the whole of
/// `.git/objects` — libgit2 does it as `git clone --local` does, and it is
/// normally free. Here it is not free, because every environment's clone is
/// bind-mounted into a container with `:Z`, and `:Z` means *relabel this
/// tree with a private SELinux MCS category*. A label is a property of the
/// inode, so relabelling a hardlink relabels the file at the other end of
/// it: one container starting rewrote the security label on the object
/// store of every other clone AND on the user's own checkout under their
/// home directory. Every git command in every other environment then failed
/// with `fatal: bad object HEAD`, because those containers hold a different
/// category pair and SELinux denied them the read.
///
/// Two lines of CLAUDE.md meet in that sentence. "Nothing an agent or a
/// container runs reaches the user's home" — a container's mount option was
/// rewriting metadata on files in `~`, through inodes nobody meant to
/// share. And "the boundary is the host, not the agent": the fix is to stop
/// the sharing, not to drop `:Z`, because the private label is what keeps
/// one environment's checkout out of another container's reach.
///
/// So: `CloneLocal::NoLinks`, which still bypasses the git-aware transport
/// (no negotiation, no smart-protocol round trips over a path on the same
/// disk) and copies the objects instead of linking them. The cost is one
/// object store per environment on disk, which is the honest price of an
/// environment being a separate world; the cost of the alternative was
/// every environment but the newest being unable to run git at all.
///
/// [`unshare_inodes`] then holds the postcondition whatever libgit2 did,
/// and is what repairs the clones that were made before this was known.
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
        .clone_local(git2::build::CloneLocal::NoLinks)
        .clone(&source.to_string_lossy(), dest)
        .with_context(|| format!("cloning {} into {}", source.display(), dest.display()))?;
    // Belt and braces, and cheap: a walk that finds nothing to do costs one
    // `stat` per file in `.git`. The guarantee this function makes is
    // "shares no inode with anything", and a guarantee that rests on one
    // flag of one library version being honoured is not one.
    unshare_inodes(dest).with_context(|| format!("unsharing {}", dest.display()))?;
    Ok(())
}

/// Give a repository its own copy of every file in `.git` that some other
/// directory entry also points at. Returns how many it had to break.
///
/// The reason is [`clone_local`]'s: a hardlinked object store means one
/// inode, one SELinux label, and a `:Z` bind mount of any one of the
/// sharers relabels it for all of them. This is the repair for the clones
/// that already exist — a fixed `clone_local` does nothing for the five
/// environments already on disk when it ships — and it is idempotent, so it
/// can simply run at startup: after the first pass every `st_nlink` is 1
/// and the walk copies nothing.
///
/// Only `.git`. The working tree is written by the checkout, file by file,
/// and shares nothing; the object store is the only part a local clone
/// links. Git itself never hardlinks within one repository, so `st_nlink >
/// 1` under here means exactly one thing: another repository is holding the
/// same file.
///
/// The copy goes to a temporary name beside the original and is then
/// renamed over it, so the file is never absent and never half-written, and
/// a container that has the old inode mmapped keeps reading identical
/// bytes. Permissions come with it, which matters: a pack file is 0444, and
/// git checks.
pub fn unshare_inodes(repo: &Path) -> Result<usize> {
    let git_dir = repo.join(".git");
    // A bare repository, or a worktree whose `.git` is a file pointing
    // elsewhere: neither is what an environment clone is, and guessing is
    // worse than doing nothing.
    if !git_dir.is_dir() {
        return Ok(0);
    }
    let mut broken = 0;
    let mut stack = vec![git_dir];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished under us — git maintenance moving
            // its own temporaries — is not this pass's business.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`: a symlink's own inode is never shared in
            // the way that matters, and following one could walk out of the
            // repository entirely.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && meta.nlink() > 1 {
                unshare_one(&path).with_context(|| format!("unsharing {}", path.display()))?;
                broken += 1;
            }
        }
    }
    Ok(broken)
}

/// Replace one file with a private copy of itself, atomically.
fn unshare_one(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // In the same directory, so the rename is on one filesystem, and named
    // so a crash leaves something recognisable rather than a mystery.
    let temp: PathBuf = dir.join(format!(".taste-unshare.{name}"));
    std::fs::copy(path, &temp)?;
    std::fs::rename(&temp, path)?;
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

    /// Every file in the clone's `.git` is the clone's own.
    ///
    /// The regression this guards is not subtle once it is stated: a
    /// hardlinked object store is one inode with one SELinux label, and a
    /// `:Z` bind mount of any sharer relabels it for all of them — which
    /// left every environment but the most recently started one unable to
    /// read its own objects, and rewrote the label on the user's checkout
    /// in their home directory on the way (`clone_local`).
    #[test]
    fn a_clone_shares_no_inode_with_the_checkout_it_came_from() {
        use std::os::unix::fs::MetadataExt;

        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        main_repo(src.path());
        let dest = dst.path().join("env/repo");

        clone_local(src.path(), &dest).unwrap();

        // Asked both ways round, because either alone can pass while the
        // bug is present: link counts catch sharing with anything at all,
        // and the inode set catches sharing with the source specifically.
        let mut source_inodes = HashSet::new();
        for path in walk(&src.path().join(".git")) {
            source_inodes.insert(std::fs::metadata(&path).unwrap().ino());
        }
        let mut checked = 0;
        for path in walk(&dest.join(".git")) {
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(
                meta.nlink(),
                1,
                "{} has {} links: the clone shares it",
                path.display(),
                meta.nlink()
            );
            assert!(
                !source_inodes.contains(&meta.ino()),
                "{} is the same inode as a file in the source",
                path.display()
            );
            checked += 1;
        }
        // The assertions above are all vacuously true over an empty walk.
        assert!(checked > 5, "only {checked} files walked");
    }

    #[test]
    fn unsharing_breaks_a_link_and_keeps_the_bytes_and_the_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/objects/pack")).unwrap();
        let pack = repo.join(".git/objects/pack/pack-1.pack");
        std::fs::write(&pack, b"PACK-objects").unwrap();
        // A pack is read-only on disk, which is exactly the mode a
        // copy-and-rename has to carry over.
        std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o444)).unwrap();
        // Somebody else's directory entry for the same inode: what a
        // hardlinking local clone leaves behind.
        let elsewhere = dir.path().join("other-pack");
        std::fs::hard_link(&pack, &elsewhere).unwrap();
        assert_eq!(std::fs::metadata(&pack).unwrap().nlink(), 2);

        assert_eq!(unshare_inodes(&repo).unwrap(), 1);

        let meta = std::fs::metadata(&pack).unwrap();
        assert_eq!(meta.nlink(), 1, "the link is broken");
        assert_ne!(
            meta.ino(),
            std::fs::metadata(&elsewhere).unwrap().ino(),
            "and it is a different inode now"
        );
        assert_eq!(std::fs::read(&pack).unwrap(), b"PACK-objects");
        assert_eq!(meta.permissions().mode() & 0o777, 0o444);
        // No temporary left beside it.
        assert!(!repo
            .join(".git/objects/pack/.taste-unshare.pack-1.pack")
            .exists());
    }

    /// Idempotent, which is what lets it run at every startup: the second
    /// pass over a repaired clone copies nothing.
    #[test]
    fn unsharing_twice_copies_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        let object = repo.join(".git/objects/one");
        std::fs::write(&object, b"one").unwrap();
        std::fs::hard_link(&object, dir.path().join("link")).unwrap();

        assert_eq!(unshare_inodes(&repo).unwrap(), 1);
        assert_eq!(unshare_inodes(&repo).unwrap(), 0);
    }

    /// A path with no `.git` directory — a bare repository, or nothing at
    /// all — is left alone rather than guessed at.
    #[test]
    fn unsharing_a_path_with_no_git_dir_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unshare_inodes(dir.path()).unwrap(), 0);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => stack.push(path),
                    Ok(meta) if meta.is_file() => out.push(path),
                    _ => {}
                }
            }
        }
        out
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
