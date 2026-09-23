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

use anyhow::{bail, Context, Result};
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

        let head = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let tree_id = self.worktree_tree()?;

        let expected = self.read_ref(name)?;
        let previous = match expected {
            Some(oid) => Some(
                self.repo
                    .find_commit(oid)
                    .with_context(|| format!("{name} does not point at a commit"))?,
            ),
            None => None,
        };
        // The message names the commit the working copy was sitting on, so
        // a restore can tell what this is a snapshot OF without walking the
        // chain to find a parent that is on a branch.
        let message = match &head {
            Some(commit) => format!("taste snapshot against {}", commit.id()),
            None => "taste snapshot against an unborn branch".to_string(),
        };
        // Unchanged only when the tree AND the commit under it are: a
        // commit that leaves the working copy as it was (part of the
        // changes committed, an amended message) still moves what the
        // snapshot is of, and a snapshot naming the old commit held the
        // folder's mirror at Stale until the next edit (review,
        // 2026-09-23).
        if let Some(previous) = &previous {
            if previous.tree_id() == tree_id
                && previous.message().map(str::trim) == Some(message.as_str())
            {
                return Ok(Snapshot {
                    commit: previous.id(),
                    wrote: false,
                });
            }
        }

        let tree = self.repo.find_tree(tree_id)?;
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

    /// The working copy as a tree object — the tree a snapshot would
    /// record, HEAD's with every change `git status` shows laid over it —
    /// written to the object database and nowhere else: no ref, no index,
    /// no file touched. What a snapshot commits, and what the mirror
    /// (`crate::mirror`) compares a folder against.
    pub fn worktree_tree(&self) -> Result<Oid> {
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

        // A directory the working copy has made a file, or a link: libgit2
        // will not put a blob where its base has a tree ("cannot replace
        // 'tree' with 'blob'"), so every snapshot failed until the change
        // was committed. Such a directory is cleared from the base first.
        let replaced: Vec<PathBuf> = paths
            .iter()
            .filter(|rel| {
                base_tree
                    .get_path(rel)
                    .is_ok_and(|entry| entry.kind() == Some(git2::ObjectType::Tree))
                    && std::fs::symlink_metadata(self.workdir.join(rel)).is_ok_and(|m| !m.is_dir())
            })
            .cloned()
            .collect();
        let base_tree = if replaced.is_empty() {
            base_tree
        } else {
            let mut clear = git2::build::TreeUpdateBuilder::new();
            for rel in &replaced {
                clear.remove(rel);
            }
            let cleared = clear
                .create_updated(&self.repo, &base_tree)
                .context("clearing the directories the working copy replaced")?;
            self.repo.find_tree(cleared)?
        };
        // What was inside them is gone with them.
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|rel| {
                !replaced
                    .iter()
                    .any(|dir| rel != dir && rel.starts_with(dir))
            })
            .collect();

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
        Ok(tree_id)
    }
}

/// The snapshot, as a shell script for a working copy this process cannot
/// open — one in a VM, run there by the files service beside the files.
///
/// The same definition as [`GitWorkspace::snapshot_worktree`], spelled in
/// git's own plumbing: a temporary index seeded from HEAD's tree (so the
/// snapshot is the whole working copy and deletions land), `add -A` (which
/// honours `.gitignore` for free and is exactly what `git status` would
/// show), `write-tree`, `commit-tree` parented on the previous snapshot or
/// on HEAD, `update-ref`. Neither HEAD nor the real index is touched; the
/// scratch index is the script's own and is removed. An unchanged working
/// copy writes nothing and says so. The last line is what
/// [`parse_script_output`] reads: the commit, and `wrote` or `unchanged`.
///
/// `name` is the ref, which is `refs/taste/snapshot/<env>` and therefore
/// safe to single-quote; anything else is refused here rather than
/// interpolated into a shell.
pub fn script(name: &str) -> Result<String> {
    if !name.starts_with(SNAPSHOT_REF_PREFIX)
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        bail!("{name} is not a snapshot ref name this script will take");
    }
    Ok(format!(
        r#"set -eu
export GIT_AUTHOR_NAME=taste-ide GIT_AUTHOR_EMAIL=taste-ide@localhost
export GIT_COMMITTER_NAME=taste-ide GIT_COMMITTER_EMAIL=taste-ide@localhost
name='{name}'
gitdir=$(git rev-parse --git-dir)
export GIT_INDEX_FILE="$gitdir/taste-snapshot-index"
rm -f "$GIT_INDEX_FILE"
head=$(git rev-parse --verify -q HEAD || true)
if [ -n "$head" ]; then git read-tree "$head"; fi
git add -A -- .
tree=$(git write-tree)
rm -f "$GIT_INDEX_FILE"
prev=$(git rev-parse --verify -q "$name" || true)
if [ -n "$head" ]; then msg="taste snapshot against $head"; else msg="taste snapshot against an unborn branch"; fi
if [ -n "$prev" ] && [ "$(git rev-parse "$prev^{{tree}}")" = "$tree" ] \
   && [ "$(git log -1 --format=%B "$prev")" = "$msg" ]; then
  printf '%s unchanged
' "$prev"
  exit 0
fi
parent="${{prev:-$head}}"
if [ -n "$parent" ]; then commit=$(git commit-tree "$tree" -p "$parent" -m "$msg"); else commit=$(git commit-tree "$tree" -m "$msg"); fi
if [ -n "$prev" ]; then git update-ref "$name" "$commit" "$prev"; else git update-ref "$name" "$commit"; fi
printf '%s wrote
' "$commit"
"#
    ))
}

/// [`GitWorkspace::restore_snapshot`] as a shell script, for a working
/// copy this process cannot open: the snapshot's tree unpacked over the
/// working tree with `git archive`, the paths the snapshot removed
/// relative to HEAD removed, and HEAD and the index left exactly where
/// they were — so what comes back is uncommitted work, as `git status`
/// will show it. With `only_if_clean`, a working tree with changes of its
/// own is refused (exit 4) rather than written over; without a snapshot
/// to restore, exit 3.
pub fn restore_script(name: &str, only_if_clean: bool) -> Result<String> {
    if !name.starts_with(SNAPSHOT_REF_PREFIX)
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        bail!("{name} is not a snapshot ref name this script will take");
    }
    let clean_check = if only_if_clean {
        r#"if [ -n "$(git status --porcelain --untracked-files=all)" ]; then echo "the working tree has changes of its own" >&2; exit 4; fi"#
    } else {
        ""
    };
    Ok(format!(
        r#"set -eu
name='{name}'
snap=$(git rev-parse --verify -q "$name^{{tree}}") || {{ echo "$name has no snapshot to restore" >&2; exit 3; }}
{clean_check}
head=$(git rev-parse --verify -q 'HEAD^{{tree}}' || true)
if [ -n "$head" ]; then
  git diff-tree -r --name-only --diff-filter=D -z "$head" "$snap" | xargs -0 -r rm -f --
fi
git archive --format=tar "$snap" | tar -xf -
printf '%s restored
' "$(git rev-parse "$name")"
"#
    ))
}

/// What [`script`] printed, as a [`Snapshot`].
pub fn parse_script_output(stdout: &str) -> Result<Snapshot> {
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .context("the snapshot script printed nothing")?;
    let mut words = last.split_whitespace();
    let commit = words
        .next()
        .and_then(|w| Oid::from_str(w).ok())
        .with_context(|| format!("the snapshot script's last line is not a commit: {last:?}"))?;
    let wrote = match words.next() {
        Some("wrote") => true,
        Some("unchanged") => false,
        other => bail!("the snapshot script's last line is not understood: {other:?}"),
    };
    Ok(Snapshot { commit, wrote })
}

/// Whether a restore may write over a working tree that has changes of its
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// Refuse unless the working tree is clean. The default, and the right
    /// one for the case restore exists for — a checkout just created from
    /// an archive, which has nothing of its own to lose.
    OnlyIfClean,
    /// Write anyway. For a user who has been told what is there and said
    /// to go ahead.
    Overwrite,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The snapshot commit that was materialised.
    pub commit: Oid,
    /// What the working tree now differs from HEAD by — the uncommitted
    /// work that came back, as `git status` will report it.
    pub uncommitted: usize,
}

impl GitWorkspace {
    /// Materialise a snapshot into the working tree.
    ///
    /// The other half of [`GitWorkspace::snapshot_worktree`], and the
    /// reason that one exists: a snapshot nobody can put back is a write-only
    /// mechanism.
    ///
    /// **HEAD does not move and the index is not touched.** That is what
    /// makes this a restore of *uncommitted work* rather than a commit of
    /// it: the snapshot's tree goes into the working tree, HEAD stays on the
    /// branch, and so everything the snapshot held that the branch does not
    /// reappears as exactly what it was — unstaged changes and untracked
    /// files, in `git status`, for the user to commit or discard as they
    /// would have.
    ///
    /// Ignored files are left alone. A snapshot never contained them (see
    /// the module docs), so a restore has no opinion about them, and
    /// deleting somebody's `target/` because it is not in a tree that was
    /// never going to hold it would be indefensible.
    pub fn restore_snapshot(&self, name: &str, mode: RestoreMode) -> Result<Restored> {
        let Some(oid) = self.read_ref(name)? else {
            bail!("{name} has no snapshot to restore");
        };
        let commit = self
            .repo
            .find_commit(oid)
            .with_context(|| format!("{name} does not point at a commit"))?;
        let tree = commit.tree().context("reading the snapshot's tree")?;

        if mode == RestoreMode::OnlyIfClean {
            let dirty = self.status().context("checking the working tree first")?;
            if !dirty.is_empty() {
                bail!(
                    "the working tree has {} change(s) of its own; restoring would \
                     overwrite them",
                    dirty.len()
                );
            }
        }

        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout
            // The working tree is being made to match the snapshot, so
            // what is there loses — that is the whole request.
            .force()
            // Files the snapshot does not have go, which is how a deletion
            // is restored. Untracked-and-not-ignored is exactly the set a
            // snapshot DOES carry, so anything in that set and absent from
            // the tree was absent when the snapshot was taken.
            .remove_untracked(true)
            // ...but never the ignored ones: `target/` was not in the
            // snapshot because it was never eligible, not because it was
            // deleted.
            .remove_ignored(false)
            // The index stays at HEAD, which is what makes the restored
            // work read as uncommitted rather than as staged.
            .update_index(false);
        self.repo
            .checkout_tree(tree.as_object(), Some(&mut checkout))
            .with_context(|| format!("writing {name}'s tree into the working tree"))?;

        let uncommitted = self.status().map(|s| s.len()).unwrap_or_default();
        Ok(Restored {
            commit: oid,
            uncommitted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_that_leaves_the_working_copy_as_it_was_is_a_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        std::fs::write(dir.path().join("a"), "a\n").unwrap();
        ws.stage(Path::new("a")).unwrap();
        ws.commit("first").unwrap();
        std::fs::write(dir.path().join("a"), "edited\n").unwrap();
        std::fs::write(dir.path().join("b"), "b\n").unwrap();
        let before = ws.snapshot_worktree(REF).unwrap();
        assert!(before.wrote);
        // Part of it committed: the working copy is what it was, the
        // commit under it is not.
        ws.stage(Path::new("b")).unwrap();
        ws.commit("b").unwrap();
        let after = ws.snapshot_worktree(REF).unwrap();
        assert!(
            after.wrote,
            "a snapshot naming the old commit would be stale"
        );
        assert!(
            !ws.snapshot_worktree(REF).unwrap().wrote,
            "and then it is unchanged"
        );
    }

    /// The script and the library agree byte for byte: run on the same
    /// dirty working copy, both produce the same tree. The script needs
    /// `git`, which the devcontainer has; a machine without it skips.
    #[test]
    fn the_script_snapshots_exactly_what_the_library_does() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no git on this machine");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join("kept.txt"), "kept\n").unwrap();
        std::fs::write(root.join("gone.txt"), "gone\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("t", "t@t").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
                .unwrap();
        }
        // Dirty it every way a snapshot has to notice.
        std::fs::write(root.join("kept.txt"), "changed\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::create_dir_all(root.join("new/deep")).unwrap();
        std::fs::write(root.join("new/deep/file.rs"), "fn f() {}\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/ignored.o"), "x").unwrap();
        std::os::unix::fs::symlink("kept.txt", root.join("link")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let ws = GitWorkspace::discover(root).unwrap();
        let library = ws.snapshot_worktree("refs/taste/snapshot/lib").unwrap();
        assert!(library.wrote);

        let text = script("refs/taste/snapshot/script").unwrap();
        let run = |script: &str| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(root)
                .output()
                .unwrap()
        };
        let out = run(&text);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let scripted = parse_script_output(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert!(scripted.wrote);

        let tree_of = |oid: Oid| repo.find_commit(oid).unwrap().tree_id();
        assert_eq!(
            tree_of(scripted.commit),
            tree_of(library.commit),
            "the script and the library must snapshot the same tree"
        );
        // Both chain on HEAD for the first snapshot...
        let head = repo.head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(
            repo.find_commit(scripted.commit)
                .unwrap()
                .parent_id(0)
                .unwrap(),
            head
        );
        // ...neither touched HEAD or the index...
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), head);
        assert!(!root.join(".git/taste-snapshot-index").exists());
        // ...and an unchanged working copy writes nothing.
        let again = run(&text);
        let unchanged = parse_script_output(&String::from_utf8_lossy(&again.stdout)).unwrap();
        assert!(!unchanged.wrote);
        assert_eq!(unchanged.commit, scripted.commit);

        // The restore script puts that snapshot back as uncommitted work,
        // on a clean checkout of the same base — and refuses a dirty one.
        let other = tempfile::tempdir().unwrap();
        let other_root = other.path();
        std::process::Command::new("git")
            .args([
                "clone",
                "-q",
                &root.display().to_string(),
                &other_root.display().to_string(),
            ])
            .output()
            .unwrap();
        // The snapshot ref travels like any ref.
        std::process::Command::new("git")
            .args([
                "-C",
                &other_root.display().to_string(),
                "fetch",
                "-q",
                &root.display().to_string(),
                "refs/taste/snapshot/script:refs/taste/snapshot/script",
            ])
            .output()
            .unwrap();
        let restore = restore_script("refs/taste/snapshot/script", true).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&restore)
            .current_dir(other_root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(other_root.join("kept.txt")).unwrap(),
            "changed\n"
        );
        assert!(
            !other_root.join("gone.txt").exists(),
            "the deletion came back"
        );
        assert!(other_root.join("new/deep/file.rs").exists());
        assert!(
            !other_root.join("target/ignored.o").exists(),
            "ignored files were never in it"
        );
        // HEAD did not move: the restored work is uncommitted.
        let restored = git2::Repository::open(other_root).unwrap();
        assert_eq!(
            restored.head().unwrap().peel_to_commit().unwrap().id(),
            head
        );
        assert!(!crate::GitWorkspace::discover(other_root)
            .unwrap()
            .status()
            .unwrap()
            .is_empty());
        // Now dirty, a second only-if-clean restore is refused.
        let refused = std::process::Command::new("sh")
            .arg("-c")
            .arg(&restore)
            .current_dir(other_root)
            .output()
            .unwrap();
        assert_eq!(refused.status.code(), Some(4));
        assert!(restore_script("refs/heads/x", true).is_err());

        assert!(script("refs/heads/main").is_err(), "not a snapshot ref");
        assert!(script("refs/taste/snapshot/x'; rm -rf /").is_err());
        assert!(parse_script_output("").is_err());
        assert!(parse_script_output("not-an-oid wrote").is_err());
    }
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

    /// The point of the whole mechanism: uncommitted work goes away and
    /// comes back as uncommitted work.
    #[test]
    fn a_restore_brings_back_the_working_copy_as_uncommitted() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::write(dir.path().join("tracked.txt"), "half-finished\n").unwrap();
        fs::write(dir.path().join("untracked.txt"), "new thought\n").unwrap();
        fs::create_dir(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/build.o"), "artifact\n").unwrap();
        let before = ws.status().unwrap();

        ws.snapshot_worktree(REF).unwrap();

        // The machine goes away: the checkout comes back from the branch
        // alone, with none of the work in it.
        fs::write(dir.path().join("tracked.txt"), "one\n").unwrap();
        fs::remove_file(dir.path().join("untracked.txt")).unwrap();
        fs::remove_file(dir.path().join(".gitignore")).unwrap();

        let restored = ws
            .restore_snapshot(REF, RestoreMode::Overwrite)
            .expect("restoring onto a checkout with its own changes was asked for");

        assert_eq!(
            fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "half-finished\n",
            "the modification came back"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("untracked.txt")).unwrap(),
            "new thought\n",
            "the untracked file came back"
        );
        // And it is uncommitted, not staged and not committed: the same
        // status the user had before their machine went away.
        assert_eq!(ws.status().unwrap(), before);
        assert!(restored.uncommitted >= 2, "{restored:?}");
        assert_eq!(
            ws.read_ref("HEAD").unwrap(),
            ws.repo.head().unwrap().resolve().unwrap().target(),
            "HEAD did not move"
        );
    }

    /// A deletion is part of the working copy, so restoring one means the
    /// file is absent afterwards.
    #[test]
    fn a_restore_reproduces_a_deletion() {
        let (dir, ws) = temp_repo();
        fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        ws.snapshot_worktree(REF).unwrap();
        // The file is back, the way a fresh checkout would have it.
        fs::write(dir.path().join("tracked.txt"), "one\n").unwrap();

        ws.restore_snapshot(REF, RestoreMode::Overwrite).unwrap();
        assert!(
            !dir.path().join("tracked.txt").exists(),
            "the snapshot did not have this file"
        );
    }

    /// Ignored files are nobody's business here. A snapshot never held
    /// them, so a restore must not read their absence as a deletion.
    #[test]
    fn a_restore_leaves_ignored_files_alone() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        ws.snapshot_worktree(REF).unwrap();
        fs::create_dir(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/build.o"), "expensive\n").unwrap();

        ws.restore_snapshot(REF, RestoreMode::Overwrite).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("ignored/build.o")).unwrap(),
            "expensive\n",
            "a restore deleted a build artifact it was never carrying"
        );
    }

    /// The default refuses rather than overwriting work it was not told
    /// about.
    #[test]
    fn a_restore_refuses_a_working_tree_with_its_own_changes() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("untracked.txt"), "snapshotted\n").unwrap();
        ws.snapshot_worktree(REF).unwrap();
        fs::write(dir.path().join("mine.txt"), "not in the snapshot\n").unwrap();

        let refused = ws.restore_snapshot(REF, RestoreMode::OnlyIfClean);
        assert!(refused.is_err(), "it overwrote a dirty working tree");
        assert_eq!(
            fs::read_to_string(dir.path().join("mine.txt")).unwrap(),
            "not in the snapshot\n"
        );
    }

    #[test]
    fn restoring_a_ref_that_has_no_snapshot_says_so() {
        let (_dir, ws) = temp_repo();
        let missing = ws.restore_snapshot(REF, RestoreMode::OnlyIfClean);
        assert!(missing.is_err());
        assert!(
            format!("{:#}", missing.unwrap_err()).contains("no snapshot"),
            "the reason should name the absence"
        );
    }

    #[test]
    fn the_ref_name_is_per_environment() {
        assert_eq!(snapshot_ref("i-0001"), "refs/taste/snapshot/i-0001");
    }
}
