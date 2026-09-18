//! The working copy as a ref: what `git status` shows, put somewhere it
//! can travel.
//!
//! Refs cross a network; a dirty working tree does not. That matters
//! because an agent's output is uncommitted for most of the time it is
//! interesting, and because docs/ENVIRONMENTS.md → "Isolation" commits the
//! IDE to a topology where an environment's files live where its containers
//! are — on another machine, and possibly on a spot instance that can be
//! reclaimed with thirty seconds' notice. A snapshot is how work in that
//! environment becomes durable without being committed to a branch.
//!
//! Three callers want exactly this one thing, which is why it is one
//! mechanism rather than three:
//!
//! - **Remote visibility.** The IDE shows what an environment has done
//!   without reading its filesystem.
//! - **Backups.** "The backup should basically be the working copy with
//!   gitignored things ignored. If git status shows it, I want it"
//!   (David, 2026-09-17).
//! - **Restore**, which is the same path whether the host was replaced, the
//!   VM went stale, the workspace is moving provisioner, or a spot instance
//!   was taken back.
//!
//! **The definition is `git status`, literally.** The tree is built from
//! [`GitWorkspace::status`] — which already asks libgit2 for untracked
//! files, recursing untracked directories, with ignored files excluded — so
//! there is no second opinion about what belongs in a snapshot to disagree
//! with the list the user is shown. Tracked modifications, new files git
//! would offer to add, and deletions all land; anything `.gitignore`
//! excludes never appears.
//!
//! **Nothing is staged and nothing is touched.** No index is written, no
//! HEAD moves, no file in the working tree changes: blobs are read from
//! disk straight into the object database and assembled with a tree
//! builder, exactly as `refs/taste/*` writes have always worked
//! ([`crate::refs`]). A snapshot of the checkout the user is typing in has
//! to be invisible to them.
//!
//! Snapshots chain. The parent is the previous snapshot, so the ref is a
//! walkable history of what the working copy looked like over time, and the
//! first one parents itself on HEAD so the chain joins the branch it came
//! from. Trees are content-addressed, so a snapshot of an unchanged working
//! copy writes nothing at all and says so.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use git2::Oid;

use crate::GitWorkspace;

/// Where snapshots live. One ref per environment, under the substrate every
/// other piece of IDE bookkeeping already uses.
pub const SNAPSHOT_REF_PREFIX: &str = "refs/taste/snapshot/";

/// The snapshot ref for one environment.
pub fn snapshot_ref(env: &str) -> String {
    format!("{SNAPSHOT_REF_PREFIX}{env}")
}

/// What a snapshot did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot commit: the one just written, or the one already there
    /// when the working copy had not changed.
    pub commit: Oid,
    /// False when the working copy was byte-identical to the last snapshot,
    /// so nothing was written. Callers that snapshot on a timer use this to
    /// avoid reporting work that did not happen.
    pub wrote: bool,
}

impl GitWorkspace {
    /// Snapshot the working copy onto `name`.
    ///
    /// See the module docs for what is included and what is left alone.
    /// Idempotent: snapshotting an unchanged working copy returns the
    /// existing commit with `wrote: false`.
    pub fn snapshot_worktree(&self, name: &str) -> Result<Snapshot> {
        self.check_ref_writable(name)?;

        // HEAD is the base the changes are overlaid on, so a snapshot's tree
        // is the whole working copy rather than only the diff — a restore
        // wants a checkout, not a patch. An unborn branch has no tree, and
        // the empty one is the honest base for a repository whose first
        // commit has not happened.
        let head = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let base_tree = match &head {
            Some(commit) => commit.tree().context("reading HEAD's tree")?,
            None => {
                let empty = self.repo.treebuilder(None)?.write()?;
                self.repo.find_tree(empty)?
            }
        };

        let status = self.status().context("reading status for a snapshot")?;
        // Sorted for a stable walk. The tree is content-addressed either
        // way, but a deterministic order makes a failure reproducible.
        let mut paths: Vec<PathBuf> = status.into_keys().collect();
        paths.sort();

        let mut builder = git2::build::TreeUpdateBuilder::new();
        for rel in &paths {
            let absolute = self.workdir.join(rel);
            // `symlink_metadata`, not `metadata`: a symlink is content to
            // record, not a thing to follow. Following one would copy a file
            // from outside the checkout into the snapshot, and a repo can
            // link anywhere.
            match std::fs::symlink_metadata(&absolute) {
                // Gone. Only remove what the base actually has, since
                // removing an absent path is an error in libgit2 — and an
                // untracked file deleted between the status and this loop
                // is a path the base never had.
                Err(_) => {
                    if base_tree.get_path(rel).is_ok() {
                        builder.remove(rel);
                    }
                }
                Ok(meta) if meta.is_dir() => {
                    // `status` recurses untracked directories, so entries
                    // are files. A directory here means the tree changed
                    // under the walk; skipping it is right, and the next
                    // snapshot will catch up.
                }
                Ok(meta) if meta.file_type().is_symlink() => {
                    let target = std::fs::read_link(&absolute)
                        .with_context(|| format!("reading the link {}", absolute.display()))?;
                    let blob = self
                        .repo
                        .blob(target.as_os_str().as_bytes())
                        .context("writing a symlink into the object database")?;
                    builder.upsert(rel, blob, git2::FileMode::Link);
                }
                Ok(meta) => {
                    let blob = self
                        .repo
                        .blob_path(&absolute)
                        .with_context(|| format!("reading {}", absolute.display()))?;
                    // The executable bit is the only permission git keeps,
                    // and losing it would make a restored checkout fail to
                    // run its own scripts.
                    let mode = if meta.permissions().mode() & 0o111 != 0 {
                        git2::FileMode::BlobExecutable
                    } else {
                        git2::FileMode::Blob
                    };
                    builder.upsert(rel, blob, mode);
                }
            }
        }

        let tree_id = builder
            .create_updated(&self.repo, &base_tree)
            .context("building the snapshot tree")?;

        let expected = self.read_ref(name)?;
        let previous = match expected {
            Some(oid) => Some(
                self.repo
                    .find_commit(oid)
                    .with_context(|| format!("{name} does not point at a commit"))?,
            ),
            None => None,
        };
        if let Some(previous) = &previous {
            if previous.tree_id() == tree_id {
                return Ok(Snapshot {
                    commit: previous.id(),
                    wrote: false,
                });
            }
        }

        let tree = self.repo.find_tree(tree_id)?;
        // The message names the commit the working copy was sitting on, so
        // a restore can tell what this is a snapshot OF without walking the
        // chain to find a parent that is on a branch.
        let message = match &head {
            Some(commit) => format!("taste snapshot against {}", commit.id()),
            None => "taste snapshot against an unborn branch".to_string(),
        };
        // Parent: the previous snapshot when there is one, so the ref is a
        // history; HEAD for the first, so the chain is rooted in the branch
        // rather than floating free.
        let parent = previous.as_ref().or(head.as_ref());
        let commit = self.commit_tree_to_ref(name, expected, parent, &tree, &message)?;
        Ok(Snapshot {
            commit,
            wrote: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    const REF: &str = "refs/taste/snapshot/env-1";

    fn temp_repo() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config
            .set_str("user.email", "test@example.invalid")
            .unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        fs::write(dir.path().join("tracked.txt"), "one\n").unwrap();
        ws.stage(Path::new("tracked.txt")).unwrap();
        ws.commit("base").unwrap();
        (dir, ws)
    }

    /// The paths in a snapshot's tree, sorted.
    fn listing(ws: &GitWorkspace, commit: Oid) -> Vec<String> {
        let tree = ws.repo.find_commit(commit).unwrap().tree().unwrap();
        let mut out = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                out.push(format!("{root}{}", entry.name().unwrap_or_default()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        out.sort();
        out
    }

    fn content(ws: &GitWorkspace, commit: Oid, path: &str) -> String {
        let tree = ws.repo.find_commit(commit).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new(path)).unwrap();
        let blob = ws.repo.find_blob(entry.id()).unwrap();
        String::from_utf8(blob.content().to_vec()).unwrap()
    }

    /// The definition is `git status`: modifications and new files in,
    /// ignored files out, and the whole checkout present rather than a diff.
    #[test]
    fn a_snapshot_is_what_git_status_shows() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join(".gitignore"), "ignored/\n*.log\n").unwrap();
        fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();
        fs::create_dir(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/secret.txt"), "no\n").unwrap();
        fs::write(dir.path().join("noisy.log"), "no\n").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/deep.txt"), "deep\n").unwrap();

        let snap = ws.snapshot_worktree(REF).unwrap();
        assert!(snap.wrote);
        assert_eq!(
            listing(&ws, snap.commit),
            vec![
                ".gitignore",
                "nested/deep.txt",
                "tracked.txt",
                "untracked.txt"
            ],
            "ignored paths must not appear, and the tree is the whole checkout"
        );
        assert_eq!(content(&ws, snap.commit, "tracked.txt"), "modified\n");
    }

    /// Deletions are part of what status shows, so they are part of the
    /// snapshot: a restore has to be able to not-have a file.
    #[test]
    fn a_deleted_file_is_absent_from_the_snapshot() {
        let (dir, ws) = temp_repo();
        fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        let snap = ws.snapshot_worktree(REF).unwrap();
        assert!(listing(&ws, snap.commit).is_empty(), "the file was deleted");
    }

    /// Nothing is staged, nothing moves, and the user's own view is
    /// untouched — a snapshot of the checkout someone is typing in has to be
    /// invisible to them.
    #[test]
    fn snapshotting_touches_neither_head_nor_the_index_nor_the_worktree() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let head_before = ws.read_ref("HEAD").unwrap();
        let status_before = ws.status().unwrap();

        ws.snapshot_worktree(REF).unwrap();

        assert_eq!(ws.read_ref("HEAD").unwrap(), head_before, "HEAD moved");
        assert_eq!(
            ws.status().unwrap(),
            status_before,
            "the snapshot staged something"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "modified\n",
            "the working tree was rewritten"
        );
        // A fresh handle, so the answer cannot come from a cached index.
        let reopened = GitWorkspace::discover(dir.path()).unwrap();
        assert_eq!(reopened.status().unwrap(), status_before);
    }

    /// An unchanged working copy writes nothing, and says so. Callers
    /// snapshot on a timer and on a preemption signal; neither should mint
    /// commits for work that did not happen.
    #[test]
    fn an_unchanged_working_copy_writes_no_second_commit() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let first = ws.snapshot_worktree(REF).unwrap();
        assert!(first.wrote);
        let again = ws.snapshot_worktree(REF).unwrap();
        assert!(
            !again.wrote,
            "an identical working copy was committed twice"
        );
        assert_eq!(again.commit, first.commit);

        // A real change does write, and chains onto the first.
        fs::write(dir.path().join("untracked.txt"), "changed\n").unwrap();
        let third = ws.snapshot_worktree(REF).unwrap();
        assert!(third.wrote);
        assert_ne!(third.commit, first.commit);
        let commit = ws.repo.find_commit(third.commit).unwrap();
        assert_eq!(
            commit.parent_id(0).unwrap(),
            first.commit,
            "snapshots chain, so the ref is a history"
        );
    }

    /// The first snapshot is rooted in the branch it came from, and says
    /// which commit it was taken against.
    #[test]
    fn the_first_snapshot_parents_itself_on_head() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();
        let head = ws.read_ref("HEAD").unwrap().unwrap();

        let snap = ws.snapshot_worktree(REF).unwrap();
        let commit = ws.repo.find_commit(snap.commit).unwrap();
        assert_eq!(commit.parent_id(0).unwrap(), head);
        assert!(
            commit.message().unwrap().contains(&head.to_string()),
            "the message names the commit the working copy sat on"
        );
    }

    /// The executable bit is the only permission git keeps, and a restored
    /// checkout whose scripts lost it does not run.
    #[test]
    fn the_executable_bit_survives() {
        let (dir, ws) = temp_repo();
        let script = dir.path().join("run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let snap = ws.snapshot_worktree(REF).unwrap();
        let tree = ws.repo.find_commit(snap.commit).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new("run.sh")).unwrap();
        assert_eq!(entry.filemode(), i32::from(git2::FileMode::BlobExecutable));
    }

    /// A symlink is recorded, never followed: a repository can link
    /// anywhere, and following one would pull a file from outside the
    /// checkout into a snapshot that then travels off the machine.
    #[test]
    fn a_symlink_is_recorded_rather_than_followed() {
        let (dir, ws) = temp_repo();
        let outside = dir.path().join("outside-the-checkout.txt");
        fs::write(&outside, "host secret\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("link")).unwrap();

        let snap = ws.snapshot_worktree(REF).unwrap();
        let tree = ws.repo.find_commit(snap.commit).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new("link")).unwrap();
        assert_eq!(entry.filemode(), i32::from(git2::FileMode::Link));
        let blob = ws.repo.find_blob(entry.id()).unwrap();
        assert_eq!(
            String::from_utf8(blob.content().to_vec()).unwrap(),
            outside.display().to_string(),
            "the link's target is the content, not the file it points at"
        );
    }

    /// The ref rules are the ref rules: a snapshot goes through the same
    /// gate every `refs/taste/*` write does.
    #[test]
    fn a_snapshot_will_not_write_a_branch_or_a_bad_name() {
        let (_dir, ws) = temp_repo();
        for name in ["taste/snapshot/env-1", "refs/taste/bad name", "HEAD"] {
            assert!(
                ws.snapshot_worktree(name).is_err(),
                "{name} should be refused"
            );
        }
        let branch = ws.branch_name().unwrap();
        assert!(
            ws.snapshot_worktree(&format!("refs/heads/{branch}"))
                .is_err(),
            "the checked-out branch is not a snapshot target"
        );
    }

    #[test]
    fn the_ref_name_is_per_environment() {
        assert_eq!(snapshot_ref("i-0001"), "refs/taste/snapshot/i-0001");
    }
}
