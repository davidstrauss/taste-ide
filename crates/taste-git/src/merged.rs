//! Whether the branch checked out is already in the default branch, so the
//! file tree can say so and offer the way back (David, 2026-10-15: "I'd
//! like a banner if the branch I'm on has already been merged into the
//! default branch. Nudge me to switch back to the default").
//!
//! "Merged" three ways, as a forge leaves it. A merge commit or a
//! fast-forward leaves the branch's tip in the default branch's history. A
//! squash or a rebase merge leaves its changes there under other commits,
//! which a merge of the branch into the default branch shows as changing
//! nothing. And a forge that deletes a merged pull request's branch leaves
//! the branch's upstream gone, which is said as just that, since a branch
//! is also deleted unmerged.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use git2::{BranchType, Oid};

use crate::GitWorkspace;

/// How the branch is in the default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merged {
    /// Its tip is in the default branch's history, which has moved past it.
    Contained,
    /// It has commits of its own, and merging them into the default branch
    /// would change nothing: a squash or a rebase merge.
    Equivalent,
    /// Its upstream is gone from the remote, as a merged pull request's
    /// branch usually is.
    Gone,
}

/// The branch checked out, already in the default branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedInto {
    /// The branch checked out.
    pub branch: String,
    /// Its tip, which a dismissal of the banner is good for.
    pub tip: Oid,
    /// The default branch's name, as switching to it names it (`main`).
    pub default_branch: String,
    /// The remote's copy of it, as a person reads it (`origin/main`).
    pub tracking: String,
    pub how: Merged,
}

impl GitWorkspace {
    /// The remote's default branch: what `refs/remotes/<remote>/HEAD`
    /// names, else its `main` or its `master`. Its name and its tracking
    /// ref, in full.
    pub fn default_branch(&self) -> Option<(String, String)> {
        let remote = self.default_remote();
        let prefix = format!("refs/remotes/{remote}/");
        let named = self
            .repo
            .find_reference(&format!("{prefix}HEAD"))
            .ok()
            .and_then(|head| head.symbolic_target().map(str::to_string))
            .filter(|target| self.repo.find_reference(target).is_ok());
        let tracking = named.or_else(|| {
            ["main", "master"]
                .iter()
                .map(|name| format!("{prefix}{name}"))
                .find(|name| self.repo.find_reference(name).is_ok())
        })?;
        let name = tracking.strip_prefix(&prefix)?.to_string();
        Some((name, tracking))
    }

    /// `branch`, if it is already in the default branch
    /// ([`Self::default_branch`]) — and is not that branch. A branch with
    /// nothing of its own at the default branch's tip, as one just made
    /// from it is, is not.
    pub fn merged_into_default(&self, branch: &str) -> Option<MergedInto> {
        let (default_branch, tracking_ref) = self.default_branch()?;
        if branch == default_branch {
            return None;
        }
        let tip = self
            .repo
            .find_branch(branch, BranchType::Local)
            .ok()?
            .get()
            .target()?;
        let theirs = self.repo.find_reference(&tracking_ref).ok()?.target()?;
        let (ahead, behind) = self.repo.graph_ahead_behind(tip, theirs).ok()?;
        let gone = self.upstream_gone(branch);
        let how = if ahead == 0 && (behind > 0 || gone) {
            Merged::Contained
        } else if ahead > 0 && self.merges_to_nothing(theirs, tip) {
            Merged::Equivalent
        } else if gone {
            Merged::Gone
        } else {
            return None;
        };
        Some(MergedInto {
            branch: branch.to_string(),
            tip,
            default_branch,
            tracking: tracking_ref
                .strip_prefix("refs/remotes/")
                .unwrap_or(&tracking_ref)
                .to_string(),
            how,
        })
    }

    /// Whether `branch` was pushed to a remote that no longer has it: an
    /// upstream configured on a named remote, whose tracking ref the last
    /// fetch pruned. A branch never pushed has no upstream, and is not.
    fn upstream_gone(&self, branch: &str) -> bool {
        let Some(remote) = self.config_string(&format!("branch.{branch}.remote")) else {
            return false;
        };
        let Some(merge) = self.config_string(&format!("branch.{branch}.merge")) else {
            return false;
        };
        let Some(name) = merge.strip_prefix("refs/heads/") else {
            return false;
        };
        self.is_named_remote(&remote)
            && self
                .repo
                .find_reference(&format!("refs/remotes/{remote}/{name}"))
                .is_err()
    }

    /// Whether merging `tip` into `base` changes nothing: no conflict, and
    /// the merge's result is `base`'s tree. Done in memory, writing no
    /// object, and remembered per pair, since the file tree asks on every
    /// status refresh and the answer for two commits never changes.
    fn merges_to_nothing(&self, base: Oid, tip: Oid) -> bool {
        static SEEN: OnceLock<Mutex<HashMap<(Oid, Oid), bool>>> = OnceLock::new();
        let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(known) = seen.lock().unwrap().get(&(base, tip)) {
            return *known;
        }
        let answer = (|| -> Option<bool> {
            let ours = self.repo.find_commit(base).ok()?;
            let theirs = self.repo.find_commit(tip).ok()?;
            let merged = self.repo.merge_commits(&ours, &theirs, None).ok()?;
            if merged.has_conflicts() {
                return Some(false);
            }
            let mut expected = git2::Index::new().ok()?;
            expected.read_tree(&ours.tree().ok()?).ok()?;
            let entries = |index: &git2::Index| {
                index
                    .iter()
                    .map(|entry| (entry.path, entry.id, entry.mode))
                    .collect::<Vec<_>>()
            };
            Some(entries(&merged) == entries(&expected))
        })()
        .unwrap_or(false);
        seen.lock().unwrap().insert((base, tip), answer);
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(cwd)
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
    }

    /// A remote with main, and a clone of it at `mine`, `origin/HEAD` set
    /// as a clone sets it.
    fn cloned(root: &Path) -> std::path::PathBuf {
        git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        git(root, &["clone", "-q", "remote.git", "theirs"]);
        let theirs = root.join("theirs");
        std::fs::write(theirs.join("a.txt"), "one\n").unwrap();
        git(&theirs, &["add", "-A"]);
        git(&theirs, &["commit", "-q", "-m", "first"]);
        git(&theirs, &["push", "-q", "origin", "HEAD:main"]);
        git(root, &["clone", "-q", "remote.git", "mine"]);
        root.join("mine")
    }

    fn merged(mine: &Path, branch: &str) -> Option<Merged> {
        GitWorkspace::discover(mine)
            .unwrap()
            .merged_into_default(branch)
            .map(|m| m.how)
    }

    #[test]
    fn a_branch_just_made_is_not_merged_and_main_never_is() {
        let dir = tempfile::tempdir().unwrap();
        let mine = cloned(dir.path());
        git(&mine, &["switch", "-q", "-c", "topic"]);
        assert_eq!(merged(&mine, "topic"), None);
        assert_eq!(merged(&mine, "main"), None);
        let ws = GitWorkspace::discover(&mine).unwrap();
        assert_eq!(
            ws.default_branch(),
            Some(("main".into(), "refs/remotes/origin/main".into()))
        );
    }

    /// Merged with a merge commit; squash-merged, its own commits never in
    /// main; and deleted on the remote unmerged, which is said as gone.
    #[test]
    fn a_branch_merged_squashed_or_deleted_on_the_remote_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mine = cloned(root);
        let theirs = root.join("theirs");

        // A merge commit on the remote.
        git(&mine, &["switch", "-q", "-c", "merged"]);
        std::fs::write(mine.join("b.txt"), "b\n").unwrap();
        git(&mine, &["add", "-A"]);
        git(&mine, &["commit", "-q", "-m", "b"]);
        git(&mine, &["push", "-q", "-u", "origin", "merged"]);
        git(&theirs, &["fetch", "-q"]);
        git(
            &theirs,
            &["merge", "-q", "--no-ff", "-m", "merge", "origin/merged"],
        );
        git(&theirs, &["push", "-q", "origin", "HEAD:main"]);
        git(&mine, &["fetch", "-q"]);
        assert_eq!(merged(&mine, "merged"), Some(Merged::Contained));

        // A squash merge, main moving on in another file after it.
        git(&mine, &["switch", "-q", "-c", "squashed", "origin/main"]);
        std::fs::write(mine.join("c.txt"), "c\n").unwrap();
        git(&mine, &["add", "-A"]);
        git(&mine, &["commit", "-q", "-m", "c one"]);
        std::fs::write(mine.join("c.txt"), "c\nc\n").unwrap();
        git(&mine, &["commit", "-qam", "c two"]);
        git(&mine, &["push", "-q", "-u", "origin", "squashed"]);
        git(&theirs, &["fetch", "-q"]);
        git(&theirs, &["merge", "-q", "--ff-only", "origin/main"]);
        git(&theirs, &["merge", "-q", "--squash", "origin/squashed"]);
        git(&theirs, &["commit", "-q", "-m", "c (squashed)"]);
        std::fs::write(theirs.join("d.txt"), "d\n").unwrap();
        git(&theirs, &["add", "-A"]);
        git(&theirs, &["commit", "-q", "-m", "d"]);
        git(&theirs, &["push", "-q", "origin", "HEAD:main"]);
        git(&mine, &["fetch", "-q"]);
        assert_eq!(merged(&mine, "squashed"), Some(Merged::Equivalent));

        // Unmerged, and still on the remote: nothing. Deleted there: gone.
        git(&mine, &["switch", "-q", "-c", "open", "origin/main"]);
        std::fs::write(mine.join("e.txt"), "e\n").unwrap();
        git(&mine, &["add", "-A"]);
        git(&mine, &["commit", "-q", "-m", "e"]);
        git(&mine, &["push", "-q", "-u", "origin", "open"]);
        assert_eq!(merged(&mine, "open"), None);
        git(&theirs, &["push", "-q", "origin", "--delete", "open"]);
        git(&mine, &["fetch", "-q", "--prune"]);
        assert_eq!(merged(&mine, "open"), Some(Merged::Gone));
    }
}
