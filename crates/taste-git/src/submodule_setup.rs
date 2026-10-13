//! The folder's submodules that are not yet the commit the parent records,
//! and the git that makes each one so, run in the folder with the user's
//! keys as a fetch is.
//!
//! A repository cloned without `--recurse-submodules` — `gh repo clone`'s
//! default — has each submodule as an empty directory. The sync gives
//! Personal only a submodule the folder has cloned, because nothing in the
//! VM can fetch, so until the folder has it neither side does, and the file
//! tree said "not checked out" with nothing to do about it (David,
//! 2026-10-13: "I'd prefer a banner with a one-click fix if any submodules
//! need initialization or fix-up").
//!
//! Only what can be done without a decision: cloned when it is not, fetched
//! when the recorded commit is not in the clone, and moved forward to that
//! commit when it is behind it. A submodule past the recorded commit is
//! worked in, and one diverged from it is the user's to settle; neither is
//! touched here.

use std::path::{Path, PathBuf};

use git2::Oid;

use crate::GitWorkspace;

/// What a submodule needs before it holds the commit its parent records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleNeed {
    /// Not cloned in the folder.
    Clone,
    /// Cloned, without the recorded commit: fetched, then decided again.
    Fetch,
    /// Behind the recorded commit, or never checked out: moved to it.
    Advance,
}

/// One submodule to set up: where it is, what it needs, and the commit the
/// parent records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleSetup {
    pub path: PathBuf,
    pub need: SubmoduleNeed,
    pub pin: Oid,
    /// Where `.gitmodules` says it is fetched from: the remote whose keys
    /// the setup asks for.
    pub url: Option<String>,
}

impl GitWorkspace {
    /// Each submodule of this folder that is not yet the commit its parent
    /// records and can be made so without a decision, in `.gitmodules`
    /// order. The recorded commit is the index's, as `git submodule status`
    /// reads it; a path the index does not record as a submodule, or holds
    /// in conflict, is left out.
    pub fn submodules_to_set_up(&self) -> Vec<SubmoduleSetup> {
        let Ok(mut index) = self.repo.index() else {
            return Vec::new();
        };
        // As it is on disk now, whatever this handle read before.
        let _ = index.read(false);
        self.submodules()
            .into_iter()
            .filter_map(|(name, path)| {
                let entry = index.get_path(&path, 0)?;
                if entry.mode != 0o160000 {
                    return None;
                }
                let need = need_of(&self.workdir.join(&path), entry.id)?;
                let url = self
                    .repo
                    .find_submodule(&name)
                    .ok()
                    .and_then(|sub| sub.url().map(str::to_owned));
                Some(SubmoduleSetup {
                    path,
                    need,
                    pin: entry.id,
                    url,
                })
            })
            .collect()
    }

    /// The git that answers `setup`'s need, run as [`Self::fetch_all_command`]
    /// is. A clone is the parent's `submodule update --init`, recursive so a
    /// submodule's own come with it; a fetch is the submodule's own, of every
    /// remote; a move forward fast-forwards the branch it is on, or checks
    /// the commit out where it is on none.
    pub fn submodule_setup_command(&self, setup: &SubmoduleSetup) -> Option<(String, Vec<String>)> {
        let path = setup.path.to_str()?;
        match setup.need {
            SubmoduleNeed::Clone => Some(self.git_command(&[
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--",
                path,
            ])),
            SubmoduleNeed::Fetch => {
                Some(GitWorkspace::open_at(&self.workdir.join(path))?.fetch_all_command())
            }
            SubmoduleNeed::Advance => {
                let inner = GitWorkspace::open_at(&self.workdir.join(path))?;
                let pin = setup.pin.to_string();
                let on_branch = inner.repo.head().is_ok_and(|h| h.is_branch());
                Some(if on_branch {
                    inner.git_command(&["merge", "--ff-only", "--quiet", &pin])
                } else {
                    inner.git_command(&["checkout", "--quiet", "--detach", &pin])
                })
            }
        }
    }

    /// The repository whose working tree is exactly `dir` — never one
    /// above it, which is what discovery from an uncloned submodule's empty
    /// directory finds.
    fn open_at(dir: &Path) -> Option<GitWorkspace> {
        let repo = git2::Repository::open(dir).ok()?;
        let workdir = repo.workdir()?.to_path_buf();
        Some(GitWorkspace { repo, workdir })
    }
}

/// What the submodule cloned (or not) at `dir` needs to hold `pin`, if
/// anything it can be given without a decision.
fn need_of(dir: &Path, pin: Oid) -> Option<SubmoduleNeed> {
    if !dir.join(".git").exists() {
        return Some(SubmoduleNeed::Clone);
    }
    let repo = git2::Repository::open(dir).ok()?;
    if repo.find_commit(pin).is_err() {
        return Some(SubmoduleNeed::Fetch);
    }
    let Some(head) = repo.head().ok().and_then(|h| h.target()) else {
        // Cloned and never checked out.
        return Some(SubmoduleNeed::Advance);
    };
    (head != pin && repo.graph_descendant_of(pin, head).unwrap_or(false))
        .then_some(SubmoduleNeed::Advance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn run(command: (String, Vec<String>)) {
        let (program, args) = command;
        let out = Command::new(program)
            .args(&args)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The template's own repository at `dir/upstream`, two commits on
    /// main, and a parent at `dir/parent` recording the second; the
    /// parent's clone at `dir/folder` as `gh repo clone` leaves it, with
    /// the submodule never initialised. The first commit's id comes back.
    fn cloned_without_submodules(dir: &Path) -> String {
        let upstream = dir.join("upstream");
        git(dir, &["init", "-q", "-b", "main", "upstream"]);
        std::fs::write(upstream.join("lib.txt"), "one\n").unwrap();
        git(&upstream, &["add", "-A"]);
        git(&upstream, &["commit", "-q", "-m", "one"]);
        let first = git(&upstream, &["rev-parse", "HEAD"]);
        std::fs::write(upstream.join("lib.txt"), "two\n").unwrap();
        git(&upstream, &["commit", "-q", "-am", "two"]);
        git(dir, &["init", "-q", "-b", "main", "parent"]);
        let parent = dir.join("parent");
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                upstream.to_str().unwrap(),
                "tpl",
            ],
        );
        git(&parent, &["commit", "-q", "-m", "add the template"]);
        git(dir, &["clone", "-q", parent.to_str().unwrap(), "folder"]);
        first
    }

    #[test]
    fn a_submodule_never_initialised_is_cloned_and_then_needs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        cloned_without_submodules(dir.path());
        let folder = GitWorkspace::discover(&dir.path().join("folder")).unwrap();
        let needs = folder.submodules_to_set_up();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].need, SubmoduleNeed::Clone);
        assert_eq!(needs[0].path, PathBuf::from("tpl"));
        run(folder.submodule_setup_command(&needs[0]).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("folder/tpl/lib.txt")).unwrap(),
            "two\n"
        );
        assert!(folder.submodules_to_set_up().is_empty());
    }

    #[test]
    fn a_submodule_behind_moves_forward_on_its_branch_or_detached() {
        let dir = tempfile::tempdir().unwrap();
        let first = cloned_without_submodules(dir.path());
        let folder_dir = dir.path().join("folder");
        let folder = GitWorkspace::discover(&folder_dir).unwrap();
        run(folder
            .submodule_setup_command(&folder.submodules_to_set_up()[0])
            .unwrap());
        let tpl = folder_dir.join("tpl");
        // On a branch, behind: the branch is fast-forwarded and stays.
        git(&tpl, &["checkout", "-q", "-B", "main", &first]);
        let needs = folder.submodules_to_set_up();
        assert_eq!(needs[0].need, SubmoduleNeed::Advance);
        run(folder.submodule_setup_command(&needs[0]).unwrap());
        assert_eq!(git(&tpl, &["branch", "--show-current"]), "main");
        assert_eq!(
            std::fs::read_to_string(tpl.join("lib.txt")).unwrap(),
            "two\n"
        );
        // Detached, behind: checked out at the recorded commit.
        git(&tpl, &["checkout", "-q", "--detach", &first]);
        let needs = folder.submodules_to_set_up();
        run(folder.submodule_setup_command(&needs[0]).unwrap());
        assert_eq!(git(&tpl, &["rev-parse", "HEAD"]), needs[0].pin.to_string());
        assert!(folder.submodules_to_set_up().is_empty());
    }

    #[test]
    fn a_submodule_past_or_beside_the_recorded_commit_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let first = cloned_without_submodules(dir.path());
        let folder_dir = dir.path().join("folder");
        let folder = GitWorkspace::discover(&folder_dir).unwrap();
        run(folder
            .submodule_setup_command(&folder.submodules_to_set_up()[0])
            .unwrap());
        let tpl = folder_dir.join("tpl");
        // Past it: work in the submodule.
        git(&tpl, &["checkout", "-q", "-B", "work"]);
        std::fs::write(tpl.join("lib.txt"), "three\n").unwrap();
        git(&tpl, &["commit", "-q", "-am", "three"]);
        assert!(folder.submodules_to_set_up().is_empty());
        // Beside it: a decision, not a fix.
        git(&tpl, &["checkout", "-q", "-B", "other", &first]);
        std::fs::write(tpl.join("lib.txt"), "elsewhere\n").unwrap();
        git(&tpl, &["commit", "-q", "-am", "elsewhere"]);
        assert!(folder.submodules_to_set_up().is_empty());
    }

    #[test]
    fn a_clone_without_the_recorded_commit_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        cloned_without_submodules(dir.path());
        let folder_dir = dir.path().join("folder");
        let folder = GitWorkspace::discover(&folder_dir).unwrap();
        run(folder
            .submodule_setup_command(&folder.submodules_to_set_up()[0])
            .unwrap());
        // The upstream moves on, and the parent records its new commit
        // while the folder's clone of the submodule has not fetched it.
        let upstream = dir.path().join("upstream");
        std::fs::write(upstream.join("lib.txt"), "three\n").unwrap();
        git(&upstream, &["commit", "-q", "-am", "three"]);
        let three = git(&upstream, &["rev-parse", "HEAD"]);
        git(
            &folder_dir,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{three},tpl"),
            ],
        );
        let needs = folder.submodules_to_set_up();
        assert_eq!(needs[0].need, SubmoduleNeed::Fetch);
        run(folder.submodule_setup_command(&needs[0]).unwrap());
        let needs = folder.submodules_to_set_up();
        assert_eq!(needs[0].need, SubmoduleNeed::Advance, "decided again");
        run(folder.submodule_setup_command(&needs[0]).unwrap());
        assert!(folder.submodules_to_set_up().is_empty());
    }
}
