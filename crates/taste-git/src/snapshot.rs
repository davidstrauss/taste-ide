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
//! **Snapshots do not chain.** Each is parented on HEAD alone, so the ref
//! holds the working copy as it is now and nothing older. They chained once,
//! the parent the previous snapshot, for a walkable history nothing read —
//! and with files that sync though ignored (`crate::sync_ignored`) that
//! history was every version of a project's database and its backups,
//! reachable for ever: 2,311 snapshots, 3.4 GiB in the checkout and
//! 1.3 GiB in the folder, which gc could neither drop nor, in a 4 GiB
//! container, pack (2026-10-12). The snapshot a new one replaces is
//! unreachable, and git's own pruning takes it. Trees are
//! content-addressed, so a snapshot of an unchanged working copy writes
//! nothing at all and says so.

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
        // a restore can tell what this is a snapshot OF without reading
        // its parent.
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
        // Parent: HEAD, never the previous snapshot (see the module's
        // docs: a chain kept every version of every synced file).
        let parent = head.as_ref();
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
        // And what is ignored but syncs anyway (`crate::sync_ignored`).
        paths.extend(self.synced_ignored_files(&self.sync_rules()));
        paths.sort();
        paths.dedup();

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
        // ...and the other way: a file or a link the working copy has made
        // a directory, which libgit2 will not put a tree beneath ("D/F
        // conflict when updating tree"). Cleared from the base too, its new
        // contents kept: the sync's simulation found every snapshot of the
        // folder failing once the checkout made `a` a directory
        // (docs/SYNC.md, P12).
        let became_dirs: Vec<PathBuf> = paths
            .iter()
            .filter(|rel| {
                base_tree
                    .get_path(rel)
                    .is_ok_and(|entry| entry.kind() == Some(git2::ObjectType::Blob))
                    && std::fs::symlink_metadata(self.workdir.join(rel)).is_ok_and(|m| m.is_dir())
            })
            .cloned()
            .collect();
        let base_tree = if replaced.is_empty() && became_dirs.is_empty() {
            base_tree
        } else {
            let mut clear = git2::build::TreeUpdateBuilder::new();
            for rel in replaced.iter().chain(&became_dirs) {
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

        let odb = self.repo.odb().context("opening the object database")?;
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
                    // The file's own bytes, unfiltered — as the checkout's
                    // snapshot in the VM takes them, where the repository's
                    // attributes are not read (the files service's
                    // GIT_ATTR_SOURCE). `blob_path` applied them here: under
                    // `* text=auto eol=lf` a CRLF file hashed as its LF
                    // form, so the folder's copy of every such file read as
                    // a change of the folder's and went to the checkout
                    // converted — 3,305 archived .eml files in the finance
                    // checkout, rewritten with LF minutes after each was
                    // saved (2026-10-11). Streamed, not read whole.
                    let known = hashed_before(&absolute, &meta).filter(|oid| odb.exists(*oid));
                    let blob = known
                        .map(|oid| Ok(Some(oid)))
                        .unwrap_or_else(|| {
                            (|| -> Result<Option<Oid>> {
                                let mut file = match std::fs::File::open(&absolute) {
                                    Ok(file) => file,
                                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                        return Ok(None)
                                    }
                                    Err(e) => return Err(e.into()),
                                };
                                let mut writer = self.repo.blob_writer(None)?;
                                std::io::copy(&mut file, &mut writer)?;
                                let oid = writer.commit()?;
                                remember_hashed(&absolute, &meta, oid);
                                Ok(Some(oid))
                            })()
                        })
                        .with_context(|| format!("reading {}", absolute.display()))?;
                    // Gone since its metadata was read, as a database's
                    // write-ahead log comes and goes: gone, as above.
                    let Some(blob) = blob else {
                        if base_tree.get_path(rel).is_ok() {
                            builder.remove(rel);
                        }
                        continue;
                    };
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

/// A file's stat, as far as telling whether it changed goes: what git's
/// index compares, nanoseconds included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
    dev: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Stamp {
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
            dev: meta.dev(),
        }
    }
}

/// The blobs files hashed to, by path and stat, for this process's life.
///
/// What syncs though ignored (`crate::sync_ignored`) is in no index, so
/// nothing remembered its hashes, and every snapshot of the folder read and
/// compressed all of it again: 30 s a pass over a project's 1.7 GB of data,
/// several passes a sync, so a file dropped in the folder reached the
/// checkout minutes later (2026-10-12). A file whose stat is what it was
/// when it was hashed is the blob it was, as git's index has it; one
/// changed within the last two seconds is not remembered, since a write in
/// the same tick as the hash would leave its stat as it was.
static HASHED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (Stamp, Oid)>>,
> = std::sync::LazyLock::new(Default::default);

/// How many files [`HASHED`] remembers before it starts over.
const HASHED_MAX: usize = 500_000;

pub(crate) fn hashed_before(path: &std::path::Path, meta: &std::fs::Metadata) -> Option<Oid> {
    let hashed = HASHED.lock().unwrap_or_else(|e| e.into_inner());
    hashed
        .get(path)
        .filter(|(stamp, _)| *stamp == Stamp::of(meta))
        .map(|(_, oid)| *oid)
}

fn remember_hashed(path: &std::path::Path, meta: &std::fs::Metadata, oid: Oid) {
    let settled = meta
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(2));
    let mut hashed = HASHED.lock().unwrap_or_else(|e| e.into_inner());
    if !settled {
        hashed.remove(path);
        return;
    }
    if hashed.len() >= HASHED_MAX {
        hashed.clear();
    }
    hashed.insert(path.to_path_buf(), (Stamp::of(meta), oid));
}

/// The snapshot, as a shell script for a working copy this process cannot
/// open — one in a VM, run there by the files service beside the files.
///
/// The same definition as [`GitWorkspace::snapshot_worktree`], spelled in
/// git's own plumbing: a temporary index seeded from HEAD's tree (so the
/// snapshot is the whole working copy and deletions land) — by
/// `read-tree -m` over the last snapshot's index, kept for it
/// (`taste-snapshot-cache` in the git directory), which keeps each
/// unchanged file's stat data, so `add -A` reads only the files that
/// changed: a plain `read-tree` keeps none, and php-src's 28,601 files were
/// read and hashed whole on every snapshot, two seconds of every pass
/// (2026-10-26) —, `add -A` (which
/// honours `.gitignore` for free and is exactly what `git status` would
/// show), `write-tree`, `commit-tree` parented on HEAD, `update-ref`. Neither HEAD nor the real index is touched; the
/// scratch index is the script's own — named for its process, so two
/// snapshots at once do not meet on one index lock, as six did after a
/// burst of ref moves (2026-10-02) — and is removed however the script
/// ends. A file that changes while it is read, the commonest of them, has
/// a second `add -A`, and a synced file one of its own
/// ([`crate::sync_ignored::snapshot_lines`]). Two at once still meet on the
/// ref: the one that loses the race compares its tree with the winner's and
/// tries again, where it once failed the snapshot ("cannot lock ref …
/// expected", 2026-10-12). A `write-tree` that fails takes the synced
/// files' kept index with it, since gc counts no such index as a reference
/// and a blob it named may be gone; the next snapshot builds it again.
/// Nothing in it takes an optional lock on the
/// repository the agent is working in (`GIT_OPTIONAL_LOCKS=0`). An unchanged working
/// copy writes nothing and says so. The last line is what
/// [`parse_script_output`] reads: the commit, and `wrote` or `unchanged`.
///
/// `name` is the ref, which is `refs/taste/snapshot/<env>` and therefore
/// safe to single-quote; anything else is refused here rather than
/// interpolated into a shell.
pub fn script(name: &str) -> Result<String> {
    script_avoiding(name, None)
}

/// [`script`], never answering with `avoid`: unchanged or not, the working
/// copy gets a new commit rather than that one. A mirror pass that wrote
/// into the checkout refuses the snapshot it observed through, which is
/// from before its writes (`taste_git::mirror`); but a checkout put back
/// exactly as it was — the file the pass wrote moved away again — has
/// that snapshot's very tree, and an unchanged snapshot is the old commit,
/// so every pass after was refused and the sync stood still (the sync's
/// simulation, 2026-10-12).
pub fn script_avoiding(name: &str, avoid: Option<Oid>) -> Result<String> {
    if !name.starts_with(SNAPSHOT_REF_PREFIX)
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        bail!("{name} is not a snapshot ref name this script will take");
    }
    let synced = crate::sync_ignored::snapshot_lines();
    let avoid = avoid.map(|oid| oid.to_string()).unwrap_or_default();
    // Shell-safe as they stand: a bare word and an address.
    let (nobody, nobody_email) = (crate::NOBODY_NAME, crate::NOBODY_EMAIL);
    Ok(format!(
        r#"set -eu
export GIT_AUTHOR_NAME={nobody} GIT_AUTHOR_EMAIL={nobody_email}
export GIT_COMMITTER_NAME={nobody} GIT_COMMITTER_EMAIL={nobody_email}
export GIT_OPTIONAL_LOCKS=0
name='{name}'
avoid='{avoid}'
gitdir=$(git rev-parse --git-dir)
export GIT_INDEX_FILE="$gitdir/taste-snapshot-index.$$"
trap 'rm -f "$GIT_INDEX_FILE" "$GIT_INDEX_FILE".*' EXIT
rm -f "$GIT_INDEX_FILE"
prev=$(git rev-parse --verify -q "$name" || true)
head=$(git rev-parse --verify -q HEAD || true)
cache="$gitdir/taste-snapshot-cache"
if [ -n "$head" ]; then
  cp -p "$cache" "$GIT_INDEX_FILE" 2>/dev/null || rm -f "$GIT_INDEX_FILE"
  git read-tree -m "$head" 2>/dev/null || {{ rm -f "$GIT_INDEX_FILE"; git read-tree "$head"; }}
fi
git add -A -- . 2>/dev/null || git add -A -- .
{synced}tree=$(git write-tree) || {{ rm -f "$gitdir/taste-synced-index"; exit 1; }}
mv -f "$GIT_INDEX_FILE" "$cache"
if [ -n "$head" ]; then msg="taste snapshot against $head"; else msg="taste snapshot against an unborn branch"; fi
tries=0
while :; do
  if [ -n "$prev" ] && [ "$prev" != "$avoid" ] && [ "$(git rev-parse "$prev^{{tree}}")" = "$tree" ] \
     && [ "$(git log -1 --format=%B "$prev")" = "$msg" ]; then
    printf '%s unchanged
' "$prev"
    exit 0
  fi
  if [ -n "$head" ]; then commit=$(git commit-tree "$tree" -p "$head" -m "$msg"); else commit=$(git commit-tree "$tree" -m "$msg"); fi
  # The same tree, parent, and message in the same second is the same
  # commit: the one to avoid, said apart by a line of its own.
  if [ "$commit" = "$avoid" ]; then
    again="Taken again after $avoid."
    if [ -n "$head" ]; then commit=$(git commit-tree "$tree" -p "$head" -m "$msg" -m "$again"); else commit=$(git commit-tree "$tree" -m "$msg" -m "$again"); fi
  fi
  if git update-ref "$name" "$commit" "$prev" 2>/dev/null; then break; fi
  tries=$((tries + 1))
  if [ "$tries" -ge 3 ]; then git update-ref "$name" "$commit" "$prev"; fi
  prev=$(git rev-parse --verify -q "$name" || true)
done
printf '%s wrote
' "$commit"
"#
    ))
}

/// What a checkout's git keeps after its snapshots have moved on, cleared:
/// run in the checkout, it removes loose objects and whole packs that no
/// ref, reflog, or index reaches and that are over half an hour old, and
/// the temporary packs a snapshot that ran out of room left; the last line
/// is `freed <bytes>`.
///
/// A snapshot is parented on HEAD alone, so the one it replaces is
/// unreachable at once; but git keeps what is unreachable two weeks, and a
/// big synced file's every version, each in a pack of its own, filled a
/// 16 GiB VM: 3.9 GB of packs for 2.3 GB of data, and 0.5 GB of temporary
/// packs from snapshots that failed for want of room (2026-10-12). Half an
/// hour is long past any git operation that has written an object and not
/// yet referenced it — and while one that can pause midway is in progress
/// (a rebase, a merge, a cherry-pick, a revert, a bisect), nothing is
/// removed. It touches only the checkout in the VM, never the user's
/// folder, whose git keeps its own two weeks.
///
/// Its sorts spill into the git directory (`sort -T`), never `/tmp`: the
/// files service cannot write there, and a repository the size of
/// php-src — 1.1 million objects — is past what sort holds in memory, so
/// every tidy of it failed and its replaced snapshots stayed (2026-10-26).
pub const TIDY_SCRIPT: &str = r#"set -eu
gd=$(git rev-parse --git-dir)
for s in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD BISECT_LOG sequencer; do
  if [ -e "$gd/$s" ]; then echo "freed 0"; exit 0; fi
done
before=$(du -sk "$gd/objects" | cut -f1)
git prune --expire=30.minutes.ago
reach="$gd/taste-reach.$$"
trap 'rm -f "$reach"' EXIT
git rev-list --objects --all --reflog --indexed-objects --no-object-names | LC_ALL=C sort -T "$gd" -u > "$reach"
for idx in "$gd"/objects/pack/pack-*.idx; do
  [ -e "$idx" ] || continue
  base=${idx%.idx}
  [ -e "$base.pack" ] || continue
  [ -e "$base.keep" ] && continue
  [ -n "$(find "$base.pack" -mmin -30)" ] && continue
  if git show-index < "$idx" | awk '{print $2}' | LC_ALL=C sort -T "$gd" | LC_ALL=C comm -12 - "$reach" | grep -q .; then
    continue
  fi
  rm -f "$base.pack" "$base.idx" "$base.rev" "$base.bitmap" "$base.mtimes"
done
find "$gd/objects/pack" -maxdepth 1 \( -name 'tmp_*' -o -name '.tmp-*' \) -mmin +30 -exec rm -f {} +
after=$(du -sk "$gd/objects" | cut -f1)
echo "freed $(( (before - after) * 1024 ))"
"#;

/// The bytes [`TIDY_SCRIPT`] says it freed.
pub fn tidy_freed(stdout: &str) -> u64 {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("freed "))
        .and_then(|n| n.trim().parse::<i64>().ok())
        .map_or(0, |n| n.max(0) as u64)
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
# Through files, not pipes: sh has no pipefail, so a git that failed
# halfway into a pipe left the script reporting "restored" over nothing.
gitdir=$(git rev-parse --git-dir)
removed="$gitdir/taste-restore-removed"
archive="$gitdir/taste-restore.tar"
trap 'rm -f "$removed" "$archive"' EXIT
if [ -n "$head" ]; then
  git diff-tree -r --name-only --diff-filter=D -z "$head" "$snap" >"$removed"
  xargs -0 -r rm -f -- <"$removed"
fi
git archive --format=tar -o "$archive" "$snap"
tar -xf "$archive"
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
    /// The tidy takes a replaced snapshot's big file — a pack of its own,
    /// unreachable, old — and stale temporary packs, and leaves what any
    /// ref reaches, what is new, and the repository whole.
    #[test]
    fn the_tidy_clears_what_replaced_snapshots_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        fs::write(root.join(".gitignore"), "data/\n").unwrap();
        fs::write(root.join(".gitattributes"), "/data/** taste-sync\n").unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        let text = script("refs/taste/snapshot/script").unwrap();
        let sh = |script: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(root)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.bigFileThreshold")
                .env("GIT_CONFIG_VALUE_0", "1k")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let packs = || -> Vec<PathBuf> {
            fs::read_dir(root.join(".git/objects/pack"))
                .map(|d| {
                    d.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
                        .collect()
                })
                .unwrap_or_default()
        };
        // Two versions of a big file, each snapshotted into a pack of its
        // own; the first is then unreachable.
        fs::write(root.join("data/backup.bin"), vec![1u8; 64 * 1024]).unwrap();
        sh(&text);
        let first = packs();
        assert_eq!(first.len(), 1, "a big file streams into a pack");
        fs::write(root.join("data/backup.bin"), vec![2u8; 64 * 1024]).unwrap();
        sh(&text);
        assert_eq!(packs().len(), 2);
        let stale = root.join(".git/objects/pack/tmp_pack_leftover");
        fs::write(&stale, b"half a pack").unwrap();
        // New: nothing goes yet.
        sh(TIDY_SCRIPT);
        assert_eq!(packs().len(), 2, "a new pack is never taken");
        // Aged past the half hour.
        // Git makes its packs read-only; touch sets their time all the same.
        for path in fs::read_dir(root.join(".git/objects/pack"))
            .unwrap()
            .flatten()
        {
            let ok = std::process::Command::new("touch")
                .args(["-d", "2 hours ago"])
                .arg(path.path())
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        let freed = tidy_freed(&sh(TIDY_SCRIPT));
        assert!(freed > 0, "{freed}");
        assert_eq!(packs().len(), 1, "the replaced snapshot's pack went");
        assert!(!first.iter().any(|p| p.exists()), "and it was the first");
        assert!(!stale.exists(), "the stale temporary pack went");
        let fsck = std::process::Command::new("git")
            .args(["fsck", "--connectivity-only"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stderr)
        );
        // While a rebase is under way, nothing is touched.
        fs::create_dir_all(root.join(".git/rebase-merge")).unwrap();
        assert_eq!(tidy_freed(&sh(TIDY_SCRIPT)), 0);
    }

    /// Every sort the tidy runs keeps its spill in the git directory: the
    /// files service cannot write `/tmp`, and a large repository's object
    /// list is past what sort holds in memory.
    #[test]
    fn the_tidy_never_sorts_through_tmp() {
        let sorts: Vec<&str> = TIDY_SCRIPT
            .split('|')
            .filter(|part| part.contains(" sort"))
            .collect();
        assert_eq!(sorts.len(), 2, "{sorts:?}");
        for sort in sorts {
            assert!(sort.contains(r#"sort -T "$gd""#), "{sort}");
        }
    }

    /// A sync rule that matches nothing yet — a directory set to sync
    /// with no files in it — is a snapshot like any other, not a failure
    /// (the sync's simulation found every pass failing on one).
    #[test]
    fn a_sync_rule_matching_nothing_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        fs::write(root.join(".gitignore"), "/ign/\n").unwrap();
        fs::write(root.join(".gitattributes"), "/ign/** taste-sync\n").unwrap();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        let text = script("refs/taste/snapshot/script").unwrap();
        for _ in 0..2 {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&text)
                .current_dir(root)
                .env(
                    "GIT_ATTR_SOURCE",
                    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                )
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A synced file that will not read — as one being written fails, and
    /// as an unreadable one does here — costs that file, not the snapshot:
    /// the rest land, new files with them, and it keeps the previous
    /// snapshot's copy rather than reading as deleted.
    #[test]
    fn a_synced_file_that_will_not_read_keeps_its_last_copy() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "data/\n").unwrap();
        std::fs::write(root.join(".gitattributes"), "/data/** taste-sync\n").unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/a.txt"), "a1").unwrap();
        std::fs::write(root.join("data/b.db"), "b1").unwrap();
        let text = script("refs/taste/snapshot/script").unwrap();
        let run = || {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&text)
                .current_dir(root)
                .env(
                    "GIT_ATTR_SOURCE",
                    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                )
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            parse_script_output(&String::from_utf8_lossy(&out.stdout)).unwrap()
        };
        run();
        std::fs::write(root.join("data/a.txt"), "a2").unwrap();
        std::fs::write(root.join("data/b.db"), "b2").unwrap();
        std::fs::write(root.join("data/c.txt"), "c1").unwrap();
        let db = root.join("data/b.db");
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&db).is_ok() {
            eprintln!("SKIP: running as a user every file is readable to");
            return;
        }
        let second = run();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(second.wrote);
        let repo = git2::Repository::open(root).unwrap();
        let tree = repo.find_commit(second.commit).unwrap().tree().unwrap();
        let read = |path: &str| {
            let entry = tree.get_path(Path::new(path)).expect(path);
            repo.find_blob(entry.id()).unwrap().content().to_vec()
        };
        assert_eq!(read("data/a.txt"), b"a2");
        assert_eq!(read("data/c.txt"), b"c1");
        assert_eq!(read("data/b.db"), b"b1", "kept from the last snapshot");
    }

    /// A file hashed once is not read again while its stat holds — and a
    /// rewrite is seen even when it keeps the length and the mtime, by its
    /// ctime.
    #[test]
    fn a_remembered_hash_gives_way_to_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "data/\n").unwrap();
        std::fs::write(root.join(".gitattributes"), "/data/** taste-sync\n").unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let file = root.join("data/a.db");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let write = |text: &str| {
            std::fs::write(&file, text).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        write("one");
        let ws = GitWorkspace::discover(root).unwrap();
        let blob_in = |tree: Oid| {
            let tree = ws.repo.find_tree(tree).unwrap();
            let entry = tree.get_path(Path::new("data/a.db")).unwrap();
            ws.repo.find_blob(entry.id()).unwrap().content().to_vec()
        };
        let first = ws.worktree_tree().unwrap();
        assert_eq!(blob_in(first), b"one");
        assert!(hashed_before(&file, &std::fs::symlink_metadata(&file).unwrap()).is_some());
        assert_eq!(ws.worktree_tree().unwrap(), first, "unchanged, from memory");
        write("two");
        assert_eq!(blob_in(ws.worktree_tree().unwrap()), b"two");
    }

    /// The VM's snapshot keeps the synced files' index between runs: a
    /// rewritten file is read again, a deleted one leaves it, and a new
    /// one joins it.
    #[test]
    fn the_scripts_kept_index_follows_the_synced_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "data/\n").unwrap();
        std::fs::write(root.join(".gitattributes"), "/data/** taste-sync\n").unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/a.txt"), "a1").unwrap();
        std::fs::write(root.join("data/b.txt"), "b1").unwrap();
        let text = script("refs/taste/snapshot/script").unwrap();
        let run = || {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&text)
                .current_dir(root)
                .env(
                    "GIT_ATTR_SOURCE",
                    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                )
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            parse_script_output(&String::from_utf8_lossy(&out.stdout)).unwrap()
        };
        run();
        assert!(root.join(".git/taste-synced-index").exists());
        std::fs::write(root.join("data/a.txt"), "a2").unwrap();
        std::fs::remove_file(root.join("data/b.txt")).unwrap();
        std::fs::write(root.join("data/c.txt"), "c1").unwrap();
        let second = run();
        let repo = git2::Repository::open(root).unwrap();
        let tree = repo.find_commit(second.commit).unwrap().tree().unwrap();
        let read = |path: &str| {
            tree.get_path(Path::new(path))
                .ok()
                .map(|entry| repo.find_blob(entry.id()).unwrap().content().to_vec())
        };
        assert_eq!(read("data/a.txt").as_deref(), Some(&b"a2"[..]));
        assert_eq!(read("data/b.txt"), None, "deleted, so gone");
        assert_eq!(read("data/c.txt").as_deref(), Some(&b"c1"[..]));
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with("taste-snapshot-index"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The VM's snapshot keeps its index between runs for the stat data,
    /// and that never changes what a snapshot holds: a file changed since
    /// is read again, and one the last snapshot's index had but HEAD no
    /// longer tracks — dropped from the repository and ignored — is not
    /// carried into the next.
    #[test]
    fn the_scripts_cached_index_changes_only_what_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "a1\n").unwrap();
        std::fs::write(root.join("junk.o"), "junk\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let text = script("refs/taste/snapshot/script").unwrap();
        let run = || {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&text)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            parse_script_output(&String::from_utf8_lossy(&out.stdout)).unwrap()
        };
        let repo = git2::Repository::open(root).unwrap();
        let read = |commit: Oid, path: &str| {
            let tree = repo.find_commit(commit).unwrap().tree().unwrap();
            tree.get_path(Path::new(path))
                .ok()
                .map(|entry| repo.find_blob(entry.id()).unwrap().content().to_vec())
        };
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        let first = run();
        assert!(root.join(".git/taste-snapshot-cache").exists());
        assert_eq!(read(first.commit, "a.txt").as_deref(), Some(&b"a2\n"[..]));
        assert_eq!(
            read(first.commit, "junk.o").as_deref(),
            Some(&b"junk\n"[..])
        );

        // Dropped from the repository and ignored, still on disk.
        git(&["rm", "-q", "--cached", "junk.o"]);
        std::fs::write(root.join(".gitignore"), "*.o\n").unwrap();
        git(&["add", ".gitignore"]);
        git(&["commit", "-qm", "ignore objects"]);
        // Changed again, by a write of the same length.
        std::fs::write(root.join("a.txt"), "a3\n").unwrap();
        let second = run();
        assert_eq!(read(second.commit, "a.txt").as_deref(), Some(&b"a3\n"[..]));
        assert_eq!(read(second.commit, "junk.o"), None);

        // Unchanged: nothing written, and the same tree.
        let third = run();
        assert!(!third.wrote);
        assert_eq!(third.commit, second.commit);
    }

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
        std::fs::write(root.join(".gitignore"), "target/\n*.pdf\n").unwrap();
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

        // Ignored but set to sync (`crate::sync_ignored`), by the root's
        // `.gitattributes` and by a nested one, beside ignored files that
        // are not; and a filter that fails if anything runs it.
        std::fs::write(
            root.join(".gitattributes"),
            "* text=auto eol=lf\n/target/keep/** taste-sync\n*.bin filter=boom\n*.pdf taste-sync\n",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("target/keep/deep")).unwrap();
        std::fs::write(root.join("target/keep/deep/w.bin"), "weights").unwrap();
        // A file with CRLF line endings under `eol=lf`: both snapshots keep
        // its bytes, unconverted. The library converted it once, and the
        // folder's copy then read as a change, sent to the checkout as LF.
        std::fs::write(
            root.join("target/keep/message.eml"),
            "To: a\r\nSubject: b\r\n\r\nbody\r\n",
        )
        .unwrap();
        // A type's rule over files in folders git has never seen: one with
        // a source file beside it, and one holding nothing but what is
        // ignored — which git's `attr:` pathspec does not enter, since it
        // asks the folder's own attribute first (2026-10-20: a new deck's
        // PDF missing from the tree).
        std::fs::write(root.join("new/deep/deck.pdf"), "pdf").unwrap();
        std::fs::create_dir_all(root.join("fresh/out")).unwrap();
        std::fs::write(root.join("fresh/out/only.pdf"), "pdf").unwrap();
        std::fs::create_dir_all(root.join("sub/out")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "out/\n").unwrap();
        std::fs::write(root.join("sub/.gitattributes"), "/out/*.txt taste-sync\n").unwrap();
        std::fs::write(root.join("sub/out/a.txt"), "a").unwrap();
        std::fs::write(root.join("sub/out/b.o"), "b").unwrap();
        repo.config()
            .unwrap()
            .set_str("filter.boom.clean", "false")
            .unwrap();
        repo.config()
            .unwrap()
            .set_bool("filter.boom.required", true)
            .unwrap();

        let ws = GitWorkspace::discover(root).unwrap();
        let library = ws.snapshot_worktree("refs/taste/snapshot/lib").unwrap();
        assert!(library.wrote);

        let text = script("refs/taste/snapshot/script").unwrap();
        let run = |script: &str| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(root)
                // As the keeper runs it: no attribute of the repository's.
                .env(
                    "GIT_ATTR_SOURCE",
                    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                )
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
        let scripted_tree = repo.find_tree(tree_of(scripted.commit)).unwrap();
        let eml = scripted_tree
            .get_path(Path::new("target/keep/message.eml"))
            .expect("the CRLF file is in the script's snapshot");
        assert_eq!(
            repo.find_blob(eml.id()).unwrap().content(),
            b"To: a\r\nSubject: b\r\n\r\nbody\r\n",
            "kept as its bytes"
        );
        for (path, there) in [
            ("target/keep/deep/w.bin", true),
            ("sub/out/a.txt", true),
            ("new/deep/deck.pdf", true),
            ("fresh/out/only.pdf", true),
            ("target/ignored.o", false),
            ("sub/out/b.o", false),
        ] {
            assert_eq!(
                scripted_tree.get_path(Path::new(path)).is_ok(),
                there,
                "{path} in the script's snapshot"
            );
        }
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

        // A real change does write, on HEAD rather than on the first: the
        // snapshot it replaces is left unreachable, for git to prune.
        fs::write(dir.path().join("untracked.txt"), "changed\n").unwrap();
        let third = ws.snapshot_worktree(REF).unwrap();
        assert!(third.wrote);
        assert_ne!(third.commit, first.commit);
        let commit = ws.repo.find_commit(third.commit).unwrap();
        let head = ws.repo.head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(commit.parent_ids().collect::<Vec<_>>(), vec![head]);
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

    /// A committed file the working copy made a directory: the snapshot
    /// holds the directory's files and not the file.
    #[test]
    fn a_file_made_a_directory_is_snapshotted() {
        let (dir, ws) = temp_repo();
        fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        fs::create_dir_all(dir.path().join("tracked.txt")).unwrap();
        fs::write(dir.path().join("tracked.txt/inside"), "now a directory\n").unwrap();
        let tree = ws.repo.find_tree(ws.worktree_tree().unwrap()).unwrap();
        let entry = tree.get_path(Path::new("tracked.txt/inside")).unwrap();
        assert_eq!(
            ws.repo.find_blob(entry.id()).unwrap().content(),
            b"now a directory\n"
        );
        assert_eq!(
            tree.get_path(Path::new("tracked.txt")).unwrap().kind(),
            Some(git2::ObjectType::Tree)
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
