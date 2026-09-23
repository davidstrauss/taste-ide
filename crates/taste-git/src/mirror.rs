//! The user's folder as a mirror of the working copy of record.
//!
//! The primary environment's checkout lives in a VM, and the folder the
//! user opened is its peer. Until 2026-09-23 the folder only ever moved by
//! fast-forward — its own branch, when it was clean — so opening the IDE,
//! editing a file, and closing it left the folder exactly as it was unless
//! a commit happened in between (David: "I should be able to just open up
//! Taste, edit and save some files, and close it"; "I specifically want my
//! local checkout to switch branches with Personal and get working copy
//! changes (other than what's ignored) from the VM, too").
//!
//! So the folder follows the checkout, whole: its HEAD onto the checkout's
//! branch at the checkout's commit, its index to that commit's tree, and
//! its working tree to the checkout's latest snapshot (`crate::snapshot`)
//! — which is `git status`'s definition of the working copy, so what
//! `.gitignore` excludes is never written and never removed.
//!
//! **Both ways, and never over the user's own edits.** The mirror
//! remembers what it last wrote ([`MIRROR_REF`]), the base both sides are
//! measured from. A path changed only in the folder since — a file
//! dropped in on this machine — is the user's, and goes to the checkout
//! ([`Mirror::Outgoing`]) BEFORE anything is written here, so a failure
//! halfway can never delete it (David: "If there isn't a conflict, I want
//! the sync to be two-way"). A path changed on both sides, to different
//! content, is a conflict: [`Mirror::Drift`], nothing written, the caller
//! asks (David: "Stop and ask") and either sends the folder's side
//! ([`GitWorkspace::folder_changes`]) or forces the checkout's.
//!
//! **Only a snapshot of the tip it is on.** A snapshot names the HEAD it
//! was taken against; one taken before the checkout switched branch or
//! committed describes a working copy that no longer exists, and is
//! [`Mirror::Stale`]: nothing is written until a fresh one arrives. A
//! snapshot whose tree IS the tip's (a clean checkout just after a commit,
//! which writes no new snapshot) is current whatever it names.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use git2::{Delta, Oid};

use crate::GitWorkspace;

/// What the mirror last wrote into the folder: a commit whose tree is the
/// folder's working copy as the mirror left it.
pub const MIRROR_REF: &str = "refs/taste/mirror/primary";

/// What mirroring did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mirror {
    /// The folder now matches the checkout; `changed` paths were written or
    /// removed, and `switched` says whether HEAD moved to another branch.
    Applied { changed: usize, switched: bool },
    /// The folder already matched.
    Unchanged,
    /// The snapshot is of another commit than the branch's tip: wait for a
    /// fresh one.
    Stale,
    /// The folder has changes the checkout does not, and none of them
    /// conflicts: the caller writes these into the checkout, snapshots it,
    /// and mirrors again — which then finds them on both sides. Nothing
    /// was written here.
    Outgoing { changes: Vec<Change> },
    /// Paths changed on both sides, to different content, since the last
    /// mirror: relative to the folder. Nothing was written.
    Drift { paths: Vec<PathBuf> },
}

/// One path's new state in the folder, for the checkout to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    /// The file's bytes, and whether it is executable; `None` when the
    /// folder deleted it. (A link is carried as its target's bytes with
    /// `link` set.)
    pub content: Option<Vec<u8>>,
    pub executable: bool,
    pub link: bool,
}

/// The commit a snapshot names in its message ("taste snapshot against
/// <oid>"), if it names one.
fn snapshot_base(message: &str) -> Option<Oid> {
    let rest = message.trim().strip_prefix("taste snapshot against ")?;
    Oid::from_str(rest.split_whitespace().next()?).ok()
}

impl GitWorkspace {
    /// Make this folder the checkout's mirror: HEAD on `branch` at `tip`,
    /// the index at `tip`'s tree, the working tree at `snapshot`'s. With
    /// `force`, a folder that drifted is written over; without, it is
    /// reported and left alone.
    pub fn mirror_from(
        &self,
        branch: &str,
        tip: Oid,
        snapshot: Oid,
        force: bool,
    ) -> Result<Mirror> {
        let local = format!("refs/heads/{branch}");
        if !git2::Reference::is_valid_name(&local) {
            bail!("{branch} is not a branch name");
        }
        let tip_commit = self
            .repo
            .find_commit(tip)
            .with_context(|| format!("finding the checkout's tip {tip}"))?;
        let snap = self
            .repo
            .find_commit(snapshot)
            .with_context(|| format!("finding the snapshot {snapshot}"))?;
        let target = snap.tree()?;
        let based_on_tip = snapshot_base(snap.message().unwrap_or("")) == Some(tip);
        if !based_on_tip && target.id() != tip_commit.tree_id() {
            return Ok(Mirror::Stale);
        }

        // What the folder should still look like: what the mirror last
        // wrote, or — the first time — its own HEAD, which a folder with no
        // uncommitted work matches and one with some does not.
        let baseline = match self.read_ref(MIRROR_REF)? {
            Some(oid) => self.repo.find_commit(oid)?.tree_id(),
            None => match self.repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
                Some(tree) => tree.id(),
                None => self.repo.treebuilder(None)?.write()?,
            },
        };
        let current = self.worktree_tree()?;
        if !force && current != baseline {
            // Each side's changes since the base, path by path, as the
            // entry each now has (`None`: deleted).
            let here = self.entries_between(baseline, current)?;
            let there = self.entries_between(baseline, target.id())?;
            let conflicts: Vec<PathBuf> = here
                .iter()
                .filter(|(path, entry)| there.get(*path).is_some_and(|theirs| theirs != *entry))
                .map(|(path, _)| path.clone())
                .collect();
            if !conflicts.is_empty() {
                return Ok(Mirror::Drift { paths: conflicts });
            }
            // The folder's own changes the checkout does not yet have go
            // there first; the ones it already has are agreement.
            let outgoing: Vec<&PathBuf> = here
                .keys()
                .filter(|path| !there.contains_key(*path))
                .collect();
            if !outgoing.is_empty() {
                let current_tree = self.repo.find_tree(current)?;
                let changes = outgoing
                    .into_iter()
                    .map(|path| self.change_at(&current_tree, path))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(Mirror::Outgoing { changes });
            }
        }

        let on_branch = self.head_ref_name().as_deref() == Some(local.as_str());
        let at_tip = self.repo.head().ok().and_then(|h| h.target()) == Some(tip);
        if current == target.id() && on_branch && at_tip {
            self.record_mirror(target.id())?;
            return Ok(Mirror::Unchanged);
        }

        let changed = self.write_tree_over(current, target.id())?;
        self.set_ref(&local, tip)?;
        if !on_branch {
            self.repo
                .set_head(&local)
                .with_context(|| format!("switching this folder to {branch}"))?;
        }
        // The index is the tip's tree: the checkout's changes read as
        // changes here, as they do there. What it has STAGED is not
        // carried — a snapshot holds a working copy, not an index.
        let mut index = self.repo.index()?;
        index.read_tree(&tip_commit.tree()?)?;
        index.write()?;
        self.record_mirror(target.id())?;
        Ok(Mirror::Applied {
            changed,
            switched: !on_branch,
        })
    }

    /// Every path the folder changed since the last mirror, as the
    /// checkout should take it: what a conflict's "keep the folder's"
    /// sends, conflicting paths included. Empty when nothing changed.
    pub fn folder_changes(&self) -> Result<Vec<Change>> {
        let baseline = match self.read_ref(MIRROR_REF)? {
            Some(oid) => self.repo.find_commit(oid)?.tree_id(),
            None => match self.repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
                Some(tree) => tree.id(),
                None => return Ok(Vec::new()),
            },
        };
        let current = self.worktree_tree()?;
        let current_tree = self.repo.find_tree(current)?;
        self.entries_between(baseline, current)?
            .keys()
            .map(|path| self.change_at(&current_tree, path))
            .collect()
    }

    fn change_at(&self, tree: &git2::Tree, path: &Path) -> Result<Change> {
        Ok(match tree.get_path(path) {
            Ok(entry) => {
                let blob = self.repo.find_blob(entry.id())?;
                Change {
                    path: path.to_path_buf(),
                    content: Some(blob.content().to_vec()),
                    executable: entry.filemode() == i32::from(git2::FileMode::BlobExecutable),
                    link: entry.filemode() == i32::from(git2::FileMode::Link),
                }
            }
            Err(_) => Change {
                path: path.to_path_buf(),
                content: None,
                executable: false,
                link: false,
            },
        })
    }

    /// The paths that differ between two trees, each with the entry it has
    /// in `to` — its object and mode — or `None` where `to` removed it.
    fn entries_between(
        &self,
        from: Oid,
        to: Oid,
    ) -> Result<std::collections::BTreeMap<PathBuf, Option<(Oid, i32)>>> {
        let from_tree = self.repo.find_tree(from)?;
        let to_tree = self.repo.find_tree(to)?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?;
        let mut out = std::collections::BTreeMap::new();
        for delta in diff.deltas() {
            if matches!(delta.status(), Delta::Deleted) {
                if let Some(path) = delta.old_file().path() {
                    out.insert(path.to_path_buf(), None);
                }
            } else if let Some(path) = delta.new_file().path() {
                let file = delta.new_file();
                out.insert(
                    path.to_path_buf(),
                    Some((file.id(), i32::from(file.mode()))),
                );
            }
        }
        Ok(out)
    }

    fn record_mirror(&self, tree: Oid) -> Result<()> {
        if let Some(oid) = self.read_ref(MIRROR_REF)? {
            if self.repo.find_commit(oid)?.tree_id() == tree {
                return Ok(());
            }
        }
        let tree = self.repo.find_tree(tree)?;
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost")?;
        let commit = self.repo.commit(
            None,
            &signature,
            &signature,
            "taste mirror: the folder as the IDE last wrote it",
            &tree,
            &[],
        )?;
        self.set_ref(MIRROR_REF, commit)
    }

    /// Write `to`'s files over a working tree that holds `from`'s: each
    /// path that differs written or removed, nothing else touched — so what
    /// neither tree has (ignored files, `.git`) is left exactly as it is.
    fn write_tree_over(&self, from: Oid, to: Oid) -> Result<usize> {
        let from_tree = self.repo.find_tree(from)?;
        let to_tree = self.repo.find_tree(to)?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?;
        let mut changed = 0;
        for delta in diff.deltas() {
            let removed = matches!(delta.status(), Delta::Deleted | Delta::Typechange);
            let written = !matches!(delta.status(), Delta::Deleted);
            if removed {
                if let Some(rel) = delta.old_file().path() {
                    self.remove_in_worktree(rel)?;
                }
            }
            if written {
                let Some(rel) = delta.new_file().path() else {
                    continue;
                };
                let file = delta.new_file();
                if file.mode() == git2::FileMode::Commit {
                    // A submodule's pointer: nothing to write here.
                    continue;
                }
                let absolute = self.checked_path(rel)?;
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("making {}", parent.display()))?;
                }
                let blob = self.repo.find_blob(file.id())?;
                // A path that was a file becoming one, a link, or a
                // directory the old tree never had: cleared first, so the
                // write below cannot land through a link.
                if std::fs::symlink_metadata(&absolute).is_ok_and(|m| m.file_type().is_symlink()) {
                    std::fs::remove_file(&absolute)?;
                }
                match file.mode() {
                    git2::FileMode::Link => {
                        let _ = std::fs::remove_file(&absolute);
                        let target = std::ffi::OsStr::from_bytes(blob.content());
                        std::os::unix::fs::symlink(target, &absolute)
                            .with_context(|| format!("linking {}", absolute.display()))?;
                    }
                    mode => {
                        std::fs::write(&absolute, blob.content())
                            .with_context(|| format!("writing {}", absolute.display()))?;
                        let executable = mode == git2::FileMode::BlobExecutable;
                        let mut permissions = std::fs::metadata(&absolute)?.permissions();
                        let bits = permissions.mode();
                        permissions.set_mode(if executable {
                            bits | 0o111
                        } else {
                            bits & !0o111
                        });
                        std::fs::set_permissions(&absolute, permissions)?;
                    }
                }
            }
            changed += 1;
        }
        Ok(changed)
    }

    /// A path inside the working tree, refused when it would reach outside
    /// it or into `.git` — a tree is data, and data does not get to choose
    /// where on this host it is written.
    fn checked_path(&self, rel: &Path) -> Result<PathBuf> {
        use std::path::Component;
        let ok = rel.components().all(|c| matches!(c, Component::Normal(_)))
            && rel
                .components()
                .next()
                .is_some_and(|first| first.as_os_str() != ".git");
        if !ok {
            bail!(
                "refusing to write {} outside the working tree",
                rel.display()
            );
        }
        // A directory on the way that is a link would carry the write out
        // of the folder.
        let mut at = self.workdir.clone();
        for part in rel.parent().into_iter().flat_map(Path::components) {
            at.push(part);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!(
                    "refusing to write {} through the link {}",
                    rel.display(),
                    at.display()
                );
            }
        }
        Ok(self.workdir.join(rel))
    }

    fn remove_in_worktree(&self, rel: &Path) -> Result<()> {
        let absolute = self.checked_path(rel)?;
        match std::fs::remove_file(&absolute) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", absolute.display())),
        }
        // The folders it leaves empty go too, up to the working tree.
        let mut dir = absolute.parent();
        while let Some(at) = dir {
            if at == self.workdir || std::fs::remove_dir(at).is_err() {
                break;
            }
            dir = at.parent();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        (dir, ws)
    }

    /// A second repository standing in for the checkout in the VM, its
    /// objects fetched into the folder the way the sync does.
    fn checkout_state(
        folder: &GitWorkspace,
        folder_dir: &Path,
        edit: impl FnOnce(&Path, &GitWorkspace),
    ) -> (Oid, Oid) {
        let vm = tempfile::tempdir().unwrap();
        let repo = git2::Repository::clone(folder_dir.to_str().unwrap(), vm.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(vm.path()).unwrap();
        edit(vm.path(), &ws);
        let tip = ws.repo.head().unwrap().target().unwrap();
        let snap = ws
            .snapshot_worktree("refs/taste/snapshot/primary")
            .unwrap()
            .commit;
        // Bring the objects over, as the fetch would.
        let folder_repo = git2::Repository::open(folder_dir).unwrap();
        let mut remote = folder_repo
            .remote_anonymous(vm.path().to_str().unwrap())
            .unwrap();
        remote
            .fetch(
                &[
                    "+refs/heads/*:refs/taste/vm/*",
                    "+refs/taste/snapshot/*:refs/taste/snapshot/*",
                ],
                None,
                None,
            )
            .unwrap();
        let _ = folder;
        (tip, snap)
    }

    #[test]
    fn the_folder_follows_branch_commit_and_working_copy() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "secret\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.stage(Path::new(".gitignore")).unwrap();
        ws.commit("first").unwrap();
        fs::write(dir.path().join("secret"), "mine\n").unwrap();

        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, vm_ws| {
            vm_ws.create_branch("feature").unwrap();
            fs::write(vm.join("b.txt"), "committed\n").unwrap();
            vm_ws.stage(Path::new("b.txt")).unwrap();
            vm_ws.commit("second").unwrap();
            fs::write(vm.join("a.txt"), "edited, uncommitted\n").unwrap();
            fs::create_dir_all(vm.join("new/dir")).unwrap();
            fs::write(vm.join("new/dir/c.txt"), "untracked\n").unwrap();
        });

        let outcome = ws.mirror_from("feature", tip, snap, false).unwrap();
        assert!(
            matches!(outcome, Mirror::Applied { switched: true, .. }),
            "{outcome:?}"
        );
        assert_eq!(ws.branch_name().as_deref(), Some("feature"));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited, uncommitted\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "committed\n"
        );
        assert!(dir.path().join("new/dir/c.txt").exists());
        // Ignored, so never written or removed.
        assert_eq!(
            fs::read_to_string(dir.path().join("secret")).unwrap(),
            "mine\n"
        );
        // The changes read as changes, as they do in the checkout.
        let status = ws.status().unwrap();
        assert!(status.contains_key(Path::new("a.txt")));
        assert!(!status.contains_key(Path::new("b.txt")));
        // Again: nothing to do.
        assert_eq!(
            ws.mirror_from("feature", tip, snap, false).unwrap(),
            Mirror::Unchanged
        );
    }

    #[test]
    fn a_file_dropped_in_here_goes_out_first_and_is_never_lost() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "from the vm\n").unwrap();
        });
        ws.mirror_from(&main, tip, snap, false).unwrap();

        // A file dropped into the folder, and the checkout moving on
        // elsewhere: no conflict.
        fs::write(dir.path().join("dropped.txt"), "mine\n").unwrap();
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "the vm moved on\n").unwrap();
        });
        match ws.mirror_from(&main, tip2, snap2, false).unwrap() {
            Mirror::Outgoing { changes } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].path, PathBuf::from("dropped.txt"));
                assert_eq!(changes[0].content.as_deref(), Some(&b"mine\n"[..]));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "from the vm\n",
            "nothing is written here until the checkout has the folder's change"
        );
        // The checkout took it: the next pass carries the checkout's change
        // in and keeps the dropped file.
        let (tip3, snap3) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "the vm moved on\n").unwrap();
            fs::write(vm.join("dropped.txt"), "mine\n").unwrap();
        });
        assert!(matches!(
            ws.mirror_from(&main, tip3, snap3, false).unwrap(),
            Mirror::Applied { .. }
        ));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "the vm moved on\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("dropped.txt")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn the_same_path_changed_on_both_sides_is_drift() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        ws.mirror_from(&main, tip, snap, false).unwrap();

        fs::write(dir.path().join("a.txt"), "edited in the folder\n").unwrap();
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "edited in the vm\n").unwrap();
        });
        match ws.mirror_from(&main, tip2, snap2, false).unwrap() {
            Mirror::Drift { paths } => assert_eq!(paths, vec![PathBuf::from("a.txt")]),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited in the folder\n",
            "never written over on a guess"
        );
        let mine = ws.folder_changes().unwrap();
        assert_eq!(
            mine[0].content.as_deref(),
            Some(&b"edited in the folder\n"[..])
        );
        // Forced: the checkout's wins.
        ws.mirror_from(&main, tip2, snap2, true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited in the vm\n"
        );
    }

    #[test]
    fn a_snapshot_of_another_commit_is_stale() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let (_, old_snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "dirty\n").unwrap();
        });
        let (new_tip, _) = checkout_state(&ws, dir.path(), |vm, vm_ws| {
            fs::write(vm.join("b.txt"), "two\n").unwrap();
            vm_ws.stage(Path::new("b.txt")).unwrap();
            vm_ws.commit("second").unwrap();
        });
        let main = ws.branch_name().unwrap();
        assert_eq!(
            ws.mirror_from(&main, new_tip, old_snap, false).unwrap(),
            Mirror::Stale
        );
    }

    #[test]
    fn a_tree_path_outside_the_folder_is_refused() {
        let (_dir, ws) = repo();
        assert!(ws.checked_path(Path::new("../escape")).is_err());
        assert!(ws.checked_path(Path::new(".git/config")).is_err());
        assert!(ws.checked_path(Path::new("ok/file")).is_ok());
    }

    #[test]
    fn the_base_a_snapshot_names_is_read() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            snapshot_base(&format!("taste snapshot against {oid}")),
            Some(Oid::from_str(oid).unwrap())
        );
        assert_eq!(
            snapshot_base("taste snapshot against an unborn branch"),
            None
        );
        let _ = OsStrExt::as_bytes(std::ffi::OsStr::new(""));
    }
}
