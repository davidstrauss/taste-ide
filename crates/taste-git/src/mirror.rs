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
//! branch at the checkout's commit, its index to that commit's tree when
//! HEAD moves, and its working tree to the checkout's — observed as a
//! snapshot (`crate::snapshot`), which is `git status`'s definition of the
//! working copy, so what `.gitignore` excludes is never written and never
//! removed.
//!
//! **Both ways, and never over anyone's own edits.** This is Unison's
//! design (docs/SYNC.md): an archive of what the two last agreed on
//! ([`MIRROR_REF`]), each side's changes measured from it, and every path
//! decided by `crate::plan` — a change on one side goes to the other, the
//! same change on both is agreement, and different changes on both are a
//! conflict, written on neither side until the user answers (David: "If
//! there isn't a conflict, I want the sync to be two-way"; "Stop and
//! ask"). Both directions go in one pass. Every write lands only while
//! its side still holds what the pass observed there, checked at the last
//! moment before the rename, so a save made during a pass is kept and
//! decided by the next; and the archive records, per path, only what
//! landed, so a pass stopped anywhere loses nothing.
//!
//! **Only a snapshot of the tip it is on.** A snapshot names the HEAD it
//! was taken against; one taken before the checkout switched branch or
//! committed describes a working copy that no longer exists, and is
//! [`Mirror::Stale`]: nothing is written until a fresh one arrives. A
//! snapshot whose tree IS the tip's (a clean checkout just after a commit,
//! which writes no new snapshot) is current whatever it names.
//!
//! **What it must guarantee** — safety, propagation, convergence, and the
//! rest, under every interleaving of edits on both sides — is
//! docs/SYNC.md, which this module is measured against.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use git2::{Delta, Oid};

use crate::GitWorkspace;

/// What the mirror last wrote into the folder: a commit whose tree is the
/// folder's working copy as the mirror left it.
pub const MIRROR_REF: &str = "refs/taste/mirror/primary";

/// Where an earlier version kept what the folder had sent since the two
/// last agreed, as a second archive beside [`MIRROR_REF`]; the archive is
/// one ref, recorded per path, since 2026-10-12 (docs/SYNC.md → "The
/// design as built"), and a stale one is deleted when found.
const RETIRED_SENT_REF: &str = "refs/taste/mirror/sent";

pub use crate::plan::{Resolve, Side};

/// A path's entry in a working copy, as a tree holds it: its blob and its
/// mode.
pub type Entry = (Oid, i32);

/// One of the folder's writes for the checkout: the entry, with its bytes,
/// and what the checkout must still hold at its path for it to land
/// (docs/SYNC.md, P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    pub change: Change,
    pub expect: Option<Entry>,
}

/// What carries a pass's writes into the checkout, wherever it is.
pub trait CheckoutWriter {
    /// Land each of `sends` whose path the checkout still holds as its
    /// `expect`, checked at the write, and say which landed, in order. The
    /// directories a removal empties go too (P11). A write that did not
    /// land is not an error: its side changed, and the next pass decides
    /// it again.
    fn apply(&self, sends: &[Send]) -> Result<Vec<bool>>;
}

/// How the mirror's baseline commit names the branch the folder was on
/// when it was recorded.
const RECORDED_ON: &str = "taste mirror on ";

/// How the mirror's baseline commit names the commit that branch was at.
const RECORDED_TIP: &str = "Tip: ";

/// What a side's HEAD names, which is where it stands. Whatever commit-ish
/// is checked out — a branch, a tag, a remote branch, `HEAD~2`, a hash —
/// HEAD ends up holding one of two things: a branch, or the commit the
/// commit-ish resolved to, detached. The folder and Personal follow each
/// other by that alone, as `git checkout <commit-ish>` does: onto the
/// branch when it is one, detached at the commit otherwise (David,
/// 2026-10-04: "detached heads should still work"; "something more
/// generic like git's 'commit-ish' concept").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Commit(Oid),
}

impl Head {
    /// What HEAD's own file says, read as git reads it: `ref:
    /// refs/heads/<branch>`, or a commit id.
    pub fn parse(head_file: &str) -> Option<Self> {
        let text = head_file.trim();
        match text.strip_prefix("ref: ") {
            Some(target) => target
                .strip_prefix("refs/heads/")
                .map(|b| Head::Branch(b.to_string())),
            None => Oid::from_str(text).ok().map(Head::Commit),
        }
    }

    /// The commit-ish that puts the other side here: the branch's name, or
    /// the commit's id.
    pub fn commitish(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Commit(oid) => oid.to_string(),
        }
    }

    /// How it reads in a sentence: the branch's name, or "the commit
    /// 1a2b3c4".
    pub fn describe(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Commit(oid) => {
                let id = oid.to_string();
                format!("the commit {}", &id[..7.min(id.len())])
            }
        }
    }
}

/// How the mirror's baseline records a detached HEAD in place of a branch.
const DETACHED: &str = "HEAD";

/// What a mirror pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mirror {
    /// The pass ran: every path decided, every write that could land
    /// landed, the archive recorded per path.
    Done(Pass),
    /// The snapshot is of another commit than the branch's tip: wait for a
    /// fresh one.
    Stale,
    /// The folder is in the middle of something of git's or the user's own
    /// — a merge, a rebase, a branch the user switched to here — or its
    /// disk cannot take what the pass would write. Nothing was written or
    /// sent; `reason` says what to do.
    Paused { reason: String },
}

/// What a pass that ran did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// The checkout's changes written into the folder.
    pub received: usize,
    /// The folder's changes written into the checkout.
    pub sent: usize,
    /// Writes not made, because their side changed the path after the
    /// pass observed it: the next pass decides those again.
    pub missed: usize,
    /// Paths both sides changed, differently, since they last agreed:
    /// written on neither, until the user answers.
    pub conflicts: Vec<PathBuf>,
    /// The folder was switched onto the checkout's branch.
    pub switched: bool,
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

/// Where the mirror once kept paths it held back after a `.gitignore`
/// change; removed when found (`mirror_at`), since nothing is held now.
const RETIRED_HELD_FILE: &str = "taste-mirror-held";

/// The commit a snapshot names in its message ("taste snapshot against
/// <oid>"), if it names one.
fn snapshot_base(message: &str) -> Option<Oid> {
    let rest = message.trim().strip_prefix("taste snapshot against ")?;
    Oid::from_str(rest.split_whitespace().next()?).ok()
}

impl GitWorkspace {
    /// Make this folder and the checkout each other's mirror, one pass
    /// (docs/SYNC.md): HEAD onto `branch` at `tip`, and each working copy
    /// onto the other's changes since they last agreed, the checkout's
    /// observed as `snapshot`. `resolve` is the user's answer to a
    /// conflict; `checkout` carries the folder's writes over there.
    pub fn mirror_from(
        &self,
        branch: &str,
        tip: Oid,
        snapshot: Oid,
        resolve: Option<&Resolve>,
        checkout: &dyn CheckoutWriter,
    ) -> Result<Mirror> {
        self.mirror_from_within(branch, tip, snapshot, resolve, checkout, &|_| None)
    }

    /// [`Self::mirror_from`], asking `room` before anything is written
    /// whether the folder's disk can take the bytes this pass would write
    /// there; a reason back pauses the pass with it, and nothing is
    /// written. The checkout is the agent's, and a file it makes is a file
    /// this host stores a second time, so what the mirror brings in is
    /// bounded by the desktop's free space rather than by the VM's.
    pub fn mirror_from_within(
        &self,
        branch: &str,
        tip: Oid,
        snapshot: Oid,
        resolve: Option<&Resolve>,
        checkout: &dyn CheckoutWriter,
        room: &dyn Fn(u64) -> Option<String>,
    ) -> Result<Mirror> {
        let local = format!("refs/heads/{branch}");
        if !git2::Reference::is_valid_name(&local) {
            bail!("{branch} is not a branch name");
        }
        self.mirror_at(Some(&local), tip, snapshot, resolve, checkout, room)
    }

    /// [`Self::mirror_from_within`] for a checkout whose HEAD is detached
    /// at `tip`: the folder detached there too.
    pub fn mirror_detached_within(
        &self,
        tip: Oid,
        snapshot: Oid,
        resolve: Option<&Resolve>,
        checkout: &dyn CheckoutWriter,
        room: &dyn Fn(u64) -> Option<String>,
    ) -> Result<Mirror> {
        self.mirror_at(None, tip, snapshot, resolve, checkout, room)
    }

    /// The pass itself, onto the branch `local` names or — `None` — onto
    /// `tip` with no branch: the three working copies observed, every path
    /// decided (`crate::plan`), each write landed only where its side
    /// still holds what the pass observed, and the archive recorded per
    /// path for what landed.
    fn mirror_at(
        &self,
        local: Option<&str>,
        tip: Oid,
        snapshot: Oid,
        resolve: Option<&Resolve>,
        checkout: &dyn CheckoutWriter,
        room: &dyn Fn(u64) -> Option<String>,
    ) -> Result<Mirror> {
        let branch = local.map(|l| l.trim_start_matches("refs/heads/").to_string());
        let tip_commit = self
            .repo
            .find_commit(tip)
            .with_context(|| format!("finding the checkout's tip {tip}"))?;
        let snap = self
            .repo
            .find_commit(snapshot)
            .with_context(|| format!("finding the snapshot {snapshot}"))?;
        if let Some(reason) = self.mirror_paused()? {
            return Ok(Mirror::Paused { reason });
        }
        // A list of held paths an earlier version kept; nothing reads it.
        let _ = std::fs::remove_file(self.repo.path().join(RETIRED_HELD_FILE));
        let target = snap.tree()?;
        let based_on_tip = snapshot_base(snap.message().unwrap_or("")) == Some(tip);
        if !based_on_tip && target.id() != tip_commit.tree_id() {
            return Ok(Mirror::Stale);
        }

        // A folder switched here to a branch of its own is not followed
        // back to Personal's: the switch is the folder's act, and the peer
        // sync carries it to Personal (`taste_devcontainer::peer`), which
        // then records the two agreeing on it (`record_switch`). Until
        // then there is nothing to mirror.
        if let (Some(local), Some(branch)) = (local, branch.as_deref()) {
            if self.switched_here(local)? {
                let head = self.head_ref_name().unwrap_or_default();
                return Ok(Mirror::Paused {
                    reason: format!(
                        "this folder is on {} and Personal on {branch}; Personal follows the \
                         folder once it can",
                        head.trim_start_matches("refs/heads/")
                    ),
                });
            }
        }
        // The archive: what the two last agreed on, or — the first time —
        // the folder's own HEAD, which a folder with no uncommitted work
        // matches and one with some does not.
        let archive_tree = match self.read_ref(MIRROR_REF)? {
            Some(oid) => self.repo.find_commit(oid)?.tree_id(),
            None => match self.repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
                Some(tree) => tree.id(),
                None => self.repo.treebuilder(None)?.write()?,
            },
        };
        let folder_tree = self.worktree_tree()?;
        // One set of rules for both sides: the folder's, and the
        // `.gitattributes` each of the three trees carries — the one being
        // written brings its own before it is in the folder to be read.
        let mut rules = self.sync_rules();
        for tree in [archive_tree, folder_tree, target.id()] {
            rules.add_set(crate::sync_ignored::attribute_files_in_tree(
                &self.repo, tree,
            ));
        }
        let archive = self.entries(archive_tree)?;
        let folder = self.entries(folder_tree)?;
        let theirs = self.entries(target.id())?;
        // A file the folder's working copy leaves out and its disk holds is
        // ignored there, whatever the rules read here say. A directory is
        // not: what is beneath it is in the copy, or is not, on its own.
        let unseen: std::collections::BTreeSet<PathBuf> = archive
            .keys()
            .chain(theirs.keys())
            .filter(|path| !folder.contains_key(*path))
            .filter(|path| {
                std::fs::symlink_metadata(self.workdir.join(path)).is_ok_and(|m| !m.is_dir())
            })
            .cloned()
            .collect();
        let plan = crate::plan::plan(
            &crate::plan::Observed {
                archive: &archive,
                folder: &folder,
                checkout: &theirs,
                folder_unseen: &unseen,
            },
            |path| !self.ignored_unsynced(path, &rules),
            resolve,
        );
        if let Some(reason) = room(self.bytes_of(&plan.to_folder)?) {
            return Ok(Mirror::Paused { reason });
        }

        // The folder's changes to the checkout, then the checkout's to the
        // folder: neither order is safer, since every write checks its own
        // side first, and a pass stopped between them records nothing.
        let sends = plan
            .to_checkout
            .iter()
            .map(|w| {
                Ok(Send {
                    change: self.change_for(&w.path, w.entry)?,
                    expect: w.expect,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let landed_checkout = if sends.is_empty() {
            Vec::new()
        } else {
            checkout.apply(&sends)?
        };
        if landed_checkout.len() != sends.len() {
            bail!(
                "the checkout answered for {} of {} writes",
                landed_checkout.len(),
                sends.len()
            );
        }
        let landed_folder = self.apply_to_folder(&plan.to_folder)?;

        // The position: on the branch — or, for a detached checkout,
        // detached too — at the checkout's tip, and the index there when
        // HEAD moved. An index the user staged in, at the tip already, is
        // theirs and stays.
        let on_branch = self.head_ref_name().as_deref() == local;
        let at_tip = self.repo.head().ok().and_then(|h| h.target()) == Some(tip);
        if !(on_branch && at_tip) {
            match (local, branch.as_deref()) {
                (Some(local), Some(branch)) => {
                    self.set_ref(local, tip)?;
                    if !on_branch {
                        self.repo
                            .set_head(local)
                            .with_context(|| format!("switching this folder to {branch}"))?;
                    }
                }
                _ => self
                    .repo
                    .set_head_detached(tip)
                    .with_context(|| format!("detaching this folder at {tip}"))?,
            }
            // The index is the tip's tree: the checkout's changes read as
            // changes here, as they do there. What it has STAGED is not
            // carried — a snapshot holds a working copy, not an index.
            let mut index = self.repo.index()?;
            index.read_tree(&tip_commit.tree()?)?;
            index.write()?;
        }

        let next = crate::plan::archived(
            &archive,
            &plan,
            |i| landed_folder[i],
            |i| landed_checkout[i],
        );
        let tree = self.tree_from(archive_tree, &next)?;
        self.record_at(tree, local, tip)?;
        let landed = |l: &[bool]| l.iter().filter(|x| **x).count();
        Ok(Mirror::Done(Pass {
            received: landed(&landed_folder),
            sent: landed(&landed_checkout),
            missed: landed_folder.len() + landed_checkout.len()
                - landed(&landed_folder)
                - landed(&landed_checkout),
            conflicts: plan.conflicts,
            switched: !on_branch,
        }))
    }

    /// Why the mirror should leave the folder alone this pass, if it
    /// should: git is partway through something here. A detached HEAD is
    /// not a pause: it is a position the two follow each other into.
    /// A switch to a branch of the folder's own is not a pause any more:
    /// the folder goes back to Personal's branch (`switched_here`).
    fn mirror_paused(&self) -> Result<Option<String>> {
        if self.repo.path().join("index.lock").exists() {
            return Ok(Some("git is working in this folder".into()));
        }
        let what = match self.repo.state() {
            git2::RepositoryState::Clean => None,
            git2::RepositoryState::Merge => Some("a merge"),
            git2::RepositoryState::Revert | git2::RepositoryState::RevertSequence => {
                Some("a revert")
            }
            git2::RepositoryState::CherryPick | git2::RepositoryState::CherryPickSequence => {
                Some("a cherry-pick")
            }
            git2::RepositoryState::Bisect => Some("a bisect"),
            _ => Some("a rebase"),
        };
        if let Some(what) = what {
            return Ok(Some(format!(
                "{what} is in progress in this folder; the folder follows Personal again once \
                 it is finished or aborted"
            )));
        }
        Ok(None)
    }

    /// Whether this folder was switched, here, to a branch of its own: on
    /// neither Personal's branch (`local`) nor the one the mirror last left
    /// it on. A folder still on the recorded branch while Personal moved is
    /// Personal's switch, which the folder follows with its edits sent
    /// first, as ever.
    fn switched_here(&self, local: &str) -> Result<bool> {
        let Some(head) = self.head_ref_name() else {
            return Ok(false);
        };
        let recorded = self
            .read_ref(MIRROR_REF)?
            .and_then(|oid| self.repo.find_commit(oid).ok())
            .and_then(|c| {
                let message = c.message()?.to_string();
                Some(
                    message
                        .strip_prefix(RECORDED_ON)?
                        .lines()
                        .next()?
                        .to_string(),
                )
            });
        Ok(recorded.is_some_and(|recorded| head != recorded && head != local))
    }

    /// Record that Personal followed this folder onto `branch`: the two
    /// agree on that branch at its tip, and on its committed files — not
    /// on whatever the folder has uncommitted, which is the folder's own
    /// and goes to Personal on the next pass, as any edit does.
    pub fn record_switch(&self, branch: &str) -> Result<()> {
        let local = format!("refs/heads/{branch}");
        let Some(tip) = self.read_ref(&local)? else {
            bail!("this folder has no branch {branch}");
        };
        let tree = self.repo.find_commit(tip)?.tree_id();
        self.record_mirror(tree, &local)
    }

    /// Record that Personal followed this folder to its detached HEAD at
    /// `tip`, as [`Self::record_switch`] does for a branch.
    pub fn record_switch_detached(&self, tip: Oid) -> Result<()> {
        let tree = self.repo.find_commit(tip)?.tree_id();
        self.record_at(tree, None, tip)
    }

    /// What the folder's HEAD names: its branch, or the commit it is
    /// detached at; `None` for a branch with no commit yet.
    pub fn checked_out(&self) -> Option<Head> {
        match self.head_ref_name() {
            Some(local) => self.repo.head().ok().and(
                local
                    .strip_prefix("refs/heads/")
                    .map(|b| Head::Branch(b.to_string())),
            ),
            None => self
                .repo
                .head()
                .ok()
                .and_then(|h| h.target())
                .map(Head::Commit),
        }
    }

    /// Where the mirror last recorded the two agreeing: a branch, or a
    /// commit with no branch.
    pub fn mirror_recorded_head(&self) -> Result<Option<Head>> {
        let Some(oid) = self.read_ref(MIRROR_REF)? else {
            return Ok(None);
        };
        let last = self.repo.find_commit(oid)?;
        let message = last.message().unwrap_or_default();
        let Some(on) = message
            .strip_prefix(RECORDED_ON)
            .and_then(|m| m.lines().next())
        else {
            return Ok(None);
        };
        if on == DETACHED {
            return Ok(message
                .lines()
                .find_map(|line| line.strip_prefix(RECORDED_TIP))
                .and_then(|tip| Oid::from_str(tip.trim()).ok())
                .map(Head::Commit));
        }
        Ok(on
            .strip_prefix("refs/heads/")
            .map(|b| Head::Branch(b.to_string())))
    }

    /// [`Self::record_mirror`] for either position: the branch `local`
    /// names, or — `None` — a HEAD detached at `tip`.
    fn record_at(&self, tree: Oid, local: Option<&str>, tip: Oid) -> Result<()> {
        match local {
            Some(local) => self.record_mirror(tree, local),
            None => self.record_mirror_detached(tree, tip),
        }
    }

    fn record_mirror_detached(&self, tree: Oid, tip: Oid) -> Result<()> {
        if self.read_ref(RETIRED_SENT_REF)?.is_some() {
            if let Ok(mut sent) = self.repo.find_reference(RETIRED_SENT_REF) {
                sent.delete()?;
            }
        }
        let message = format!(
            "{RECORDED_ON}{DETACHED}\n\nThe folder as the IDE last wrote it.\n\n{RECORDED_TIP}{tip}"
        );
        if let Some(oid) = self.read_ref(MIRROR_REF)? {
            let last = self.repo.find_commit(oid)?;
            if last.tree_id() == tree && last.message() == Some(message.as_str()) {
                return Ok(());
            }
        }
        let tree = self.repo.find_tree(tree)?;
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost")?;
        let commit = self
            .repo
            .commit(None, &signature, &signature, &message, &tree, &[])?;
        self.set_ref(MIRROR_REF, commit)
    }

    /// The branch the mirror last recorded the two agreeing on.
    pub fn mirror_recorded_branch(&self) -> Result<Option<String>> {
        let Some(oid) = self.read_ref(MIRROR_REF)? else {
            return Ok(None);
        };
        let last = self.repo.find_commit(oid)?;
        Ok(last
            .message()
            .and_then(|m| m.strip_prefix(RECORDED_ON))
            .and_then(|m| m.lines().next())
            .and_then(|r| r.strip_prefix("refs/heads/"))
            .map(str::to_string))
    }

    /// The tip the mirror last recorded `branch` at, when its baseline is
    /// of that branch and says: the point both sides last agreed on.
    pub fn mirror_recorded_tip(&self, branch: &str) -> Result<Option<Oid>> {
        let Some(oid) = self.read_ref(MIRROR_REF)? else {
            return Ok(None);
        };
        let last = self.repo.find_commit(oid)?;
        let message = last.message().unwrap_or_default();
        if message.lines().next() != Some(format!("{RECORDED_ON}refs/heads/{branch}").as_str()) {
            return Ok(None);
        }
        Ok(message
            .lines()
            .find_map(|line| line.strip_prefix(RECORDED_TIP))
            .and_then(|tip| Oid::from_str(tip.trim()).ok()))
    }

    /// A tree's working copy as the sync sees it: every file and link,
    /// path by path. A submodule's pointer is a commit in another
    /// repository, synced as a pair of its own (`taste_devcontainer::peer`),
    /// and is not in it.
    fn entries(&self, tree: Oid) -> Result<std::collections::BTreeMap<PathBuf, Entry>> {
        let tree = self.repo.find_tree(tree)?;
        let mut out = std::collections::BTreeMap::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let mut path = PathBuf::from(std::ffi::OsStr::from_bytes(root.as_bytes()));
                path.push(std::ffi::OsStr::from_bytes(entry.name_bytes()));
                out.insert(path, (entry.id(), entry.filemode()));
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(out)
    }

    /// The tree holding `copy`, and the submodule pointers `base` holds,
    /// which the sync leaves where they are. Built whole rather than as
    /// updates to `base`, which libgit2 refuses where a file takes a
    /// directory's place.
    fn tree_from(
        &self,
        base: Oid,
        copy: &std::collections::BTreeMap<PathBuf, Entry>,
    ) -> Result<Oid> {
        let mut index = git2::Index::new()?;
        let mut add = |path: &[u8], id: Oid, mode: u32| {
            index.add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags: 0,
                flags_extended: 0,
                path: path.to_vec(),
            })
        };
        for (path, (id, mode)) in copy {
            add(path.as_os_str().as_bytes(), *id, *mode as u32)?;
        }
        let base = self.repo.find_tree(base)?;
        let mut pointers = Vec::new();
        base.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.filemode() == i32::from(git2::FileMode::Commit) {
                let mut path = root.as_bytes().to_vec();
                path.extend_from_slice(entry.name_bytes());
                pointers.push((path, entry.id()));
            }
            git2::TreeWalkResult::Ok
        })?;
        for (path, id) in pointers {
            let path_buf = PathBuf::from(std::ffi::OsStr::from_bytes(&path));
            // A pointer where the copy now has a file, or beneath one, has
            // gone with what replaced it.
            let taken =
                copy.contains_key(&path_buf) || path_buf.ancestors().any(|a| copy.contains_key(a));
            if !taken {
                add(&path, id, u32::from(git2::FileMode::Commit))?;
            }
        }
        Ok(index.write_tree_to(&self.repo)?)
    }

    /// The entry at `path` as the checkout should take it: its bytes, its
    /// executable bit, a link as a link, absent as a removal.
    fn change_for(&self, path: &Path, entry: Option<Entry>) -> Result<Change> {
        Ok(match entry {
            Some((id, mode)) => Change {
                path: path.to_path_buf(),
                content: Some(self.repo.find_blob(id)?.content().to_vec()),
                executable: mode == i32::from(git2::FileMode::BlobExecutable),
                link: mode == i32::from(git2::FileMode::Link),
            },
            None => Change {
                path: path.to_path_buf(),
                content: None,
                executable: false,
                link: false,
            },
        })
    }

    /// Whether this folder's `branch` is where the mirror last left it: no
    /// commit, reset, or rebase made here since the two last agreed. A
    /// branch that diverged from the checkout's while this holds was
    /// rewritten THERE — the agent rebased it — and the folder has nothing
    /// of its own to lose by following; one that does not hold has work of
    /// its own, and divergence is the user's to settle.
    ///
    /// A baseline from before the tip was recorded is judged by its tree:
    /// the folder's working copy as the mirror wrote it, which is the
    /// branch's tree plus what git does not track, so a tip whose tree it
    /// only adds to is the tip it was written at.
    pub fn unmoved_since_mirror(&self, branch: &str) -> Result<bool> {
        let local = format!("refs/heads/{branch}");
        let Some(mine) = self.read_ref(&local)? else {
            return Ok(false);
        };
        let Some(oid) = self.read_ref(MIRROR_REF)? else {
            return Ok(false);
        };
        let last = self.repo.find_commit(oid)?;
        let message = last.message().unwrap_or_default();
        if message.lines().next() != Some(format!("{RECORDED_ON}{local}").as_str()) {
            return Ok(false);
        }
        if let Some(tip) = message
            .lines()
            .find_map(|line| line.strip_prefix(RECORDED_TIP))
        {
            return Ok(Oid::from_str(tip.trim()).ok() == Some(mine));
        }
        let mine_tree = self.repo.find_commit(mine)?.tree()?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&mine_tree), Some(&last.tree()?), None)?;
        Ok(diff.deltas().all(|delta| delta.status() == Delta::Added))
    }

    /// Record `tree` as the folder the mirror and the checkout agree on,
    /// with the branch the folder is on (`local`), which is how a switch
    /// the user makes here is told from one the checkout made
    /// ([`Self::switched_here`]). What was sent before is agreement now.
    fn record_mirror(&self, tree: Oid, local: &str) -> Result<()> {
        if self.read_ref(RETIRED_SENT_REF)?.is_some() {
            if let Ok(mut sent) = self.repo.find_reference(RETIRED_SENT_REF) {
                sent.delete()?;
            }
        }
        // The tip too: a folder still at it has made no commit, reset, or
        // rebase since, which is what lets a rebase in the checkout be
        // taken here rather than read as a divergence (`unmoved_since_mirror`).
        let tip = self
            .read_ref(local)?
            .map(|oid| format!("\n\n{RECORDED_TIP}{oid}"))
            .unwrap_or_default();
        let message = format!("{RECORDED_ON}{local}\n\nThe folder as the IDE last wrote it.{tip}");
        if let Some(oid) = self.read_ref(MIRROR_REF)? {
            let last = self.repo.find_commit(oid)?;
            if last.tree_id() == tree && last.message() == Some(message.as_str()) {
                return Ok(());
            }
        }
        let tree = self.repo.find_tree(tree)?;
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost")?;
        let commit = self
            .repo
            .commit(None, &signature, &signature, &message, &tree, &[])?;
        self.set_ref(MIRROR_REF, commit)
    }

    /// The bytes `writes` would put in the folder: every file whole — the
    /// room a write needs at its peak, since each is staged beside its old
    /// copy before replacing it.
    fn bytes_of(&self, writes: &[crate::plan::Write<Entry>]) -> Result<u64> {
        let odb = self.repo.odb()?;
        let mut bytes = 0u64;
        for (id, _) in writes.iter().filter_map(|w| w.entry) {
            if let Ok((size, _)) = odb.read_header(id) {
                bytes = bytes.saturating_add(size as u64);
            }
        }
        Ok(bytes)
    }

    /// What the folder's disk holds at `rel` now, as an entry: `Ok(None)`
    /// for nothing there, `Err` for something no entry describes — a
    /// directory, a socket — which no write expects.
    fn on_disk(&self, rel: &Path) -> Result<Option<Entry>, ()> {
        let absolute = self.workdir.join(rel);
        let meta = match std::fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(()),
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute).map_err(|_| ())?;
            let id = Oid::hash_object(git2::ObjectType::Blob, target.as_os_str().as_bytes())
                .map_err(|_| ())?;
            return Ok(Some((id, i32::from(git2::FileMode::Link))));
        }
        if !meta.is_file() {
            return Err(());
        }
        let mode = if meta.permissions().mode() & 0o111 != 0 {
            git2::FileMode::BlobExecutable
        } else {
            git2::FileMode::Blob
        };
        let id = match crate::snapshot::hashed_before(&absolute, &meta) {
            Some(id) => id,
            None => Oid::hash_file(git2::ObjectType::Blob, &absolute).map_err(|_| ())?,
        };
        Ok(Some((id, i32::from(mode))))
    }

    /// Land the checkout's changes in the folder, each only while the
    /// folder still holds at its path what the pass observed there —
    /// checked again after the new file is staged and before it is renamed
    /// into place, which is the last moment there is (docs/SYNC.md, P1):
    /// a save the user made since the pass read the folder is kept, and
    /// decided by the next pass. Removals first, deepest first, then
    /// writes, shallowest first, so a file and a directory never meet at
    /// one path (P12). Through `beneath`, which refuses a path out of the
    /// folder, a directory on the way that is a link, and a staging name
    /// planted as one — the checkout chose these paths, and it does not get
    /// to choose where on this host they land. Which landed, in order.
    fn apply_to_folder(&self, writes: &[crate::plan::Write<Entry>]) -> Result<Vec<bool>> {
        let mut landed = vec![false; writes.len()];
        let still = |w: &crate::plan::Write<Entry>| self.on_disk(&w.path) == Ok(w.expect);
        let mut removals: Vec<usize> = (0..writes.len())
            .filter(|i| writes[*i].entry.is_none())
            .collect();
        removals.sort_by(|a, b| writes[*b].path.cmp(&writes[*a].path));
        // A write that fails — a path `beneath` refuses, a directory that
        // appeared at the last moment — costs that path, said in the log,
        // and not the pass: it did not land, so the archive does not
        // record it, and the next pass decides it again (P10).
        let failed = |w: &crate::plan::Write<Entry>, e: anyhow::Error| {
            tracing::warn!("not writing {} in the folder: {e:#}", w.path.display());
            false
        };
        for i in removals {
            let w = &writes[i];
            if still(w) {
                landed[i] = match self.remove_in_worktree(&w.path) {
                    Ok(()) => true,
                    Err(e) => failed(w, e),
                };
            }
        }
        let mut puts: Vec<usize> = (0..writes.len())
            .filter(|i| writes[*i].entry.is_some())
            .collect();
        puts.sort_by(|a, b| writes[*a].path.cmp(&writes[*b].path));
        for i in puts {
            let w = &writes[i];
            let Some((id, mode)) = w.entry else { continue };
            if !still(w) {
                continue;
            }
            let blob = self.repo.find_blob(id)?;
            let ready = || still(w);
            let written = if mode == i32::from(git2::FileMode::Link) {
                crate::beneath::write_link_if(
                    &self.workdir,
                    &w.path,
                    std::ffi::OsStr::from_bytes(blob.content()),
                    &ready,
                )
            } else {
                crate::beneath::write_file_if(
                    &self.workdir,
                    &w.path,
                    blob.content(),
                    mode == i32::from(git2::FileMode::BlobExecutable),
                    &ready,
                )
            };
            landed[i] = written.unwrap_or_else(|e| failed(w, e));
        }
        Ok(landed)
    }

    /// A path inside the working tree, refused when it would reach outside
    /// it, into `.git`, or through a link ([`crate::beneath::checked`]).
    fn checked_path(&self, rel: &Path) -> Result<PathBuf> {
        crate::beneath::checked(&self.workdir, rel)
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
    use std::cell::RefCell;
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

    /// The checkout's side, for these tests: every write lands, and is
    /// kept to be looked at; `during` runs first, which is the moment
    /// between a pass's decision and the folder's writes.
    #[derive(Default)]
    struct Took {
        sends: RefCell<Vec<Send>>,
        during: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl CheckoutWriter for Took {
        fn apply(&self, sends: &[Send]) -> Result<Vec<bool>> {
            if let Some(during) = self.during.borrow_mut().take() {
                during();
            }
            self.sends.borrow_mut().extend(sends.iter().cloned());
            Ok(vec![true; sends.len()])
        }
    }

    impl Took {
        /// What was sent, path by path: the bytes, or `None` for a removal.
        fn sent(&self) -> Vec<(String, Option<String>)> {
            let mut out: Vec<_> = self
                .sends
                .borrow()
                .iter()
                .map(|s| {
                    (
                        s.change.path.display().to_string(),
                        s.change
                            .content
                            .as_ref()
                            .map(|b| String::from_utf8_lossy(b).to_string()),
                    )
                })
                .collect();
            out.sort();
            out
        }
    }

    fn done(outcome: Mirror) -> Pass {
        match outcome {
            Mirror::Done(pass) => pass,
            other => panic!("the pass did not run: {other:?}"),
        }
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

    /// A folder with `a.txt` committed, mirrored once with a checkout that
    /// matches it: the archive in place.
    fn agreed() -> (tempfile::TempDir, GitWorkspace, String) {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        (dir, ws, main)
    }

    /// An ignored path whose `taste-sync` is set is carried to the folder —
    /// and other ignored files still are not — and the folder's edit to it
    /// is sent back like any change of its own.
    #[test]
    fn an_ignored_path_set_to_sync_syncs_both_ways() {
        let (dir, ws) = repo();
        fs::write(dir.path().join(".gitignore"), "data/\n").unwrap();
        ws.stage(Path::new(".gitignore")).unwrap();
        ws.commit("ignore data").unwrap();
        let main = ws.branch_name().unwrap();
        let checkout = |vm: &Path, _: &GitWorkspace| {
            let attributes = crate::sync_ignored::with_path("", Path::new("data/keep"), true);
            fs::write(vm.join(".gitattributes"), attributes).unwrap();
            fs::create_dir_all(vm.join("data/keep")).unwrap();
            fs::write(vm.join("data/keep/model.bin"), "weights\n").unwrap();
            fs::write(vm.join("data/other.bin"), "not this\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("data/keep/model.bin")).unwrap(),
            "weights\n"
        );
        assert!(dir.path().join(".gitattributes").exists());
        assert!(
            !dir.path().join("data/other.bin").exists(),
            "not set to sync"
        );

        fs::write(dir.path().join("data/keep/model.bin"), "retrained\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(
            took.sent(),
            vec![("data/keep/model.bin".into(), Some("retrained\n".into()))]
        );
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

        let pass = done(
            ws.mirror_from("feature", tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert!(pass.switched, "{pass:?}");
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
            done(
                ws.mirror_from("feature", tip, snap, None, &Took::default())
                    .unwrap()
            ),
            Pass::default()
        );
    }

    /// The folder is "unmoved" while its branch is at the tip the mirror
    /// recorded, and stops being so the moment a commit is made here —
    /// which is what tells a rebase in the checkout (taken) from a
    /// divergence of the folder's own (asked about).
    #[test]
    fn the_folder_is_unmoved_until_it_commits_on_its_own() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        assert!(!ws.unmoved_since_mirror(&main).unwrap(), "never mirrored");
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, vm_ws| {
            fs::write(vm.join("b.txt"), "two\n").unwrap();
            vm_ws.stage(Path::new("b.txt")).unwrap();
            vm_ws.commit("second").unwrap();
        });
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert!(ws.unmoved_since_mirror(&main).unwrap());
        fs::write(dir.path().join("c.txt"), "mine\n").unwrap();
        ws.stage(Path::new("c.txt")).unwrap();
        ws.commit("the folder's own").unwrap();
        assert!(!ws.unmoved_since_mirror(&main).unwrap());
        assert!(!ws.unmoved_since_mirror("elsewhere").unwrap());
    }

    /// A baseline written before the tip was recorded is judged by its
    /// tree: the folder's working copy, which only ADDS untracked files to
    /// the tip it was written at.
    #[test]
    fn a_baseline_without_a_tip_is_judged_by_its_tree() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        // The old format: the folder with an untracked file, no Tip line.
        fs::write(dir.path().join("untracked.txt"), "loose\n").unwrap();
        let tree = ws.worktree_tree().unwrap();
        let tree = ws.repo.find_tree(tree).unwrap();
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost").unwrap();
        let old = ws
            .repo
            .commit(
                None,
                &signature,
                &signature,
                &format!("{RECORDED_ON}refs/heads/{main}\n\nThe folder as the IDE last wrote it."),
                &tree,
                &[],
            )
            .unwrap();
        ws.set_ref(MIRROR_REF, old).unwrap();
        assert!(ws.unmoved_since_mirror(&main).unwrap());
        fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("moved").unwrap();
        assert!(!ws.unmoved_since_mirror(&main).unwrap());
    }

    /// Both ways in one pass: a file dropped into the folder goes to the
    /// checkout, and the checkout's edit elsewhere comes in, neither
    /// waiting on the other (docs/SYNC.md, P3).
    #[test]
    fn a_file_dropped_in_here_goes_out_as_the_checkouts_edit_comes_in() {
        let (dir, ws, main) = agreed();
        fs::write(dir.path().join("dropped.txt"), "mine\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "the vm moved on\n").unwrap();
        });
        let took = Took::default();
        let pass = done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!((pass.sent, pass.received, pass.missed), (1, 1, 0));
        assert_eq!(
            took.sent(),
            vec![("dropped.txt".into(), Some("mine\n".into()))]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "the vm moved on\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("dropped.txt")).unwrap(),
            "mine\n"
        );
        // The checkout as it now is: the two agree, and nothing moves.
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "the vm moved on\n").unwrap();
            fs::write(vm.join("dropped.txt"), "mine\n").unwrap();
        });
        let took = Took::default();
        assert_eq!(
            done(ws.mirror_from(&main, tip, snap, None, &took).unwrap()),
            Pass::default()
        );
        assert!(took.sent().is_empty());
    }

    /// A path both sides changed, differently, is written on neither and
    /// asked about, while everything else still goes; and the answer
    /// writes the paths it was about and no others (P2).
    #[test]
    fn the_same_path_changed_on_both_sides_is_asked_about() {
        let (dir, ws, main) = agreed();
        fs::write(dir.path().join("a.txt"), "edited in the folder\n").unwrap();
        fs::write(dir.path().join("new.txt"), "unasked\n").unwrap();
        let checkout = |vm: &Path, _: &GitWorkspace| {
            fs::write(vm.join("a.txt"), "edited in the vm\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        let took = Took::default();
        let pass = done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(pass.conflicts, vec![PathBuf::from("a.txt")]);
        assert_eq!(
            took.sent(),
            vec![("new.txt".into(), Some("unasked\n".into()))]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited in the folder\n",
            "never written over on a guess"
        );

        // Answered: keep Personal's, for a.txt.
        let resolve = Resolve {
            keep: Side::Checkout,
            paths: [PathBuf::from("a.txt")].into(),
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, ws| {
            checkout(vm, ws);
            fs::write(vm.join("new.txt"), "unasked\n").unwrap();
        });
        let took = Took::default();
        let pass = done(
            ws.mirror_from(&main, tip, snap, Some(&resolve), &took)
                .unwrap(),
        );
        assert!(pass.conflicts.is_empty(), "{pass:?}");
        assert!(took.sent().is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited in the vm\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "unasked\n"
        );
    }

    /// Saved again before the checkout's snapshot came back with the first
    /// save: the folder's to send, never a conflict with itself (P6).
    #[test]
    fn a_second_save_after_a_send_is_the_folders_not_a_conflict() {
        let (dir, ws, main) = agreed();
        fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(took.sent(), vec![("a.txt".into(), Some("v1\n".into()))]);

        fs::write(dir.path().join("a.txt"), "v2\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "v1\n").unwrap();
        });
        let took = Took::default();
        let pass = done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert!(pass.conflicts.is_empty(), "{pass:?}");
        assert_eq!(took.sent(), vec![("a.txt".into(), Some("v2\n".into()))]);
    }

    /// A file dropped in, sent, and renamed before the two agree: the
    /// rename goes out whole — the new name and the old one's removal — and
    /// the checkout's copy under the old name is never written back (P5).
    #[test]
    fn a_file_renamed_after_it_was_sent_is_not_written_back() {
        let (dir, ws, main) = agreed();
        fs::write(dir.path().join("activity.qfx"), "statement\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );

        // Renamed here; the checkout has the file under its first name.
        fs::rename(dir.path().join("activity.qfx"), dir.path().join("amex.qfx")).unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("activity.qfx"), "statement\n").unwrap();
        });
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(
            took.sent(),
            vec![
                ("activity.qfx".into(), None),
                ("amex.qfx".into(), Some("statement\n".into()))
            ]
        );

        // The checkout took both, and the two agree.
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("amex.qfx"), "statement\n").unwrap();
        });
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert!(
            !dir.path().join("activity.qfx").exists(),
            "not written back"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("amex.qfx")).unwrap(),
            "statement\n"
        );
    }

    /// A save the user makes after the pass observed the folder, and
    /// before its write to that path lands, is kept: the write checks the
    /// folder at the last moment, does not land, and the next pass finds
    /// the two sides both changed (P1).
    #[test]
    fn a_save_during_the_pass_is_never_written_over() {
        let (dir, ws, main) = agreed();
        // Something to send, so the checkout's side runs — which is the
        // moment between the pass's decision and its writes here.
        fs::write(dir.path().join("dropped.txt"), "mine\n").unwrap();
        let checkout = |vm: &Path, _: &GitWorkspace| {
            fs::write(vm.join("a.txt"), "from the vm\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        let took = Took::default();
        let saved = dir.path().join("a.txt");
        *took.during.borrow_mut() = Some(Box::new(move || {
            fs::write(&saved, "saved meanwhile\n").unwrap();
        }));
        let pass = done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!((pass.sent, pass.received, pass.missed), (1, 0, 1));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "saved meanwhile\n"
        );
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, ws| {
            checkout(vm, ws);
            fs::write(vm.join("dropped.txt"), "mine\n").unwrap();
        });
        let pass = done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert_eq!(pass.conflicts, vec![PathBuf::from("a.txt")]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "saved meanwhile\n"
        );
    }

    #[test]
    fn a_branch_the_user_switched_to_here_pauses_the_mirror() {
        let (dir, ws, main) = agreed();
        let head = ws.repo.head().unwrap().peel_to_commit().unwrap();
        ws.repo.branch("mine", &head, false).unwrap();
        ws.repo.set_head("refs/heads/mine").unwrap();
        fs::write(dir.path().join("a.txt"), "on my branch\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        assert!(matches!(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
            Mirror::Paused { .. }
        ));
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "on my branch\n"
        );
        assert_eq!(ws.head_ref_name().as_deref(), Some("refs/heads/mine"));
    }

    /// Personal detached at a commit — any commit-ish that is not a branch
    /// — puts the folder there too, detached, its working copy Personal's,
    /// and the mirror records that position as where the two agree.
    #[test]
    fn a_detached_checkout_detaches_the_folder() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        let first = ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );

        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, vm_ws| {
            fs::write(vm.join("b.txt"), "two\n").unwrap();
            vm_ws.stage(Path::new("b.txt")).unwrap();
            vm_ws.commit("second").unwrap();
            let commit = vm_ws.repo.find_commit(first).unwrap();
            vm_ws
                .repo
                .checkout_tree(
                    commit.as_object(),
                    Some(git2::build::CheckoutBuilder::new().force()),
                )
                .unwrap();
            vm_ws.repo.set_head_detached(first).unwrap();
            fs::write(vm.join("a.txt"), "edited while detached\n").unwrap();
        });
        assert_eq!(tip2, first);
        done(
            ws.mirror_detached_within(tip2, snap2, None, &Took::default(), &|_| None)
                .unwrap(),
        );
        assert_eq!(ws.checked_out(), Some(Head::Commit(first)));
        assert_eq!(
            ws.mirror_recorded_head().unwrap(),
            Some(Head::Commit(first))
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited while detached\n"
        );
        assert!(!dir.path().join("b.txt").exists());
        // Again: nothing to do, and a detached folder is not a paused one.
        assert_eq!(
            done(
                ws.mirror_detached_within(tip2, snap2, None, &Took::default(), &|_| None)
                    .unwrap()
            ),
            Pass::default()
        );
    }

    /// A directory the checkout held untracked, and the folder then came to
    /// ignore, is never sent to the checkout as deleted: the files are
    /// still in the folder, and ignored paths are not the mirror's in either
    /// direction (2026-10-04: a Drupal install deleted from Personal).
    #[test]
    fn what_the_folder_ignores_is_never_sent_as_deleted() {
        let (dir, ws, main) = agreed();
        let installed = |vm: &Path, _: &GitWorkspace| {
            fs::create_dir_all(vm.join("dep/sub")).unwrap();
            fs::write(vm.join("dep/sub/x.txt"), "installed\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), installed);
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert!(dir.path().join("dep/sub/x.txt").exists());

        fs::write(dir.path().join(".gitignore"), "/dep/\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), installed);
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(
            took.sent(),
            vec![(".gitignore".into(), Some("/dep/\n".into()))]
        );
        assert!(dir.path().join("dep/sub/x.txt").exists());
    }

    #[test]
    fn heads_read_as_git_writes_them() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            Head::parse("ref: refs/heads/main\n"),
            Some(Head::Branch("main".into()))
        );
        assert_eq!(
            Head::parse(&format!("{oid}\n")),
            Some(Head::Commit(Oid::from_str(oid).unwrap()))
        );
        let detached = Head::Commit(Oid::from_str(oid).unwrap());
        assert_eq!(detached.commitish(), oid);
        assert_eq!(detached.describe(), "the commit 0123456");
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
            ws.mirror_from(&main, new_tip, old_snap, None, &Took::default())
                .unwrap(),
            Mirror::Stale
        );
    }

    #[test]
    fn a_pass_the_disk_cannot_take_writes_nothing() {
        let (dir, ws, main) = agreed();
        fs::write(dir.path().join("dropped.txt"), "mine\n").unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("big.bin"), vec![7u8; 4096]).unwrap();
        });
        let asked = std::cell::Cell::new(0u64);
        let took = Took::default();
        let result = ws
            .mirror_from_within(&main, tip, snap, None, &took, &|bytes| {
                asked.set(bytes);
                Some("no room".into())
            })
            .unwrap();
        assert_eq!(
            result,
            Mirror::Paused {
                reason: "no room".into()
            }
        );
        assert_eq!(asked.get(), 4096, "the bytes the pass would write");
        assert!(!dir.path().join("big.bin").exists());
        assert!(took.sent().is_empty(), "nor anything sent");
    }

    #[test]
    fn a_directory_the_checkout_made_a_file_is_replaced() {
        let (dir, ws) = repo();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/b"), "inside\n").unwrap();
        ws.stage(Path::new("a/b")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );

        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::remove_dir_all(vm.join("a")).unwrap();
            fs::write(vm.join("a"), "a file now\n").unwrap();
        });
        let pass = done(
            ws.mirror_from(&main, tip2, snap2, None, &Took::default())
                .unwrap(),
        );
        assert_eq!((pass.received, pass.missed), (2, 0), "{pass:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a")).unwrap(),
            "a file now\n"
        );
        let strays: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".taste-mirror"))
            .collect();
        assert!(strays.is_empty(), "no staging file is left behind");
    }

    #[test]
    fn a_tree_path_outside_the_folder_is_refused() {
        let (_dir, ws) = repo();
        assert!(ws.checked_path(Path::new("../escape")).is_err());
        assert!(ws.checked_path(Path::new(".git/config")).is_err());
        assert!(
            ws.checked_path(Path::new("sub/.git/config")).is_err(),
            "any component"
        );
        assert!(
            ws.checked_path(Path::new("sub/.GIT/config")).is_err(),
            "any case"
        );
        assert!(ws.checked_path(Path::new("sub/.git./config")).is_err());
        assert!(ws.checked_path(Path::new("sub/git~1/config")).is_err());
        assert!(ws.checked_path(Path::new("ok/file")).is_ok());
        assert!(ws.checked_path(Path::new("ok/.gitignore")).is_ok());
    }

    /// An ignored file is never written over; once the rule that ignored
    /// it is gone, it is an ordinary path, and the folder's version and
    /// the checkout's are two sides of it — different, so asked about,
    /// with neither sent over the other (P7).
    #[test]
    fn an_unignored_file_on_both_sides_is_asked_about_never_overwritten() {
        let (dir, ws) = repo();
        fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new(".gitignore")).unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        fs::write(dir.path().join(".env"), "SECRET=mine\n").unwrap();
        let main = ws.branch_name().unwrap();

        // The checkout un-ignores .env and writes one of its own.
        let checkout = |vm: &Path, _: &GitWorkspace| {
            fs::write(vm.join(".gitignore"), "").unwrap();
            fs::write(vm.join(".env"), "SECRET=theirs\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET=mine\n",
            "an ignored file is not written over"
        );
        // The rule is gone here too now: the two versions are a conflict.
        let (tip, snap) = checkout_state(&ws, dir.path(), checkout);
        let took = Took::default();
        let pass = done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert_eq!(pass.conflicts, vec![PathBuf::from(".env")]);
        assert!(took.sent().is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET=mine\n"
        );
    }

    /// A rule taken out in the checkout takes effect here outright: what it
    /// kept in this folder alone goes to the checkout on the next pass,
    /// whether or not the rule was ever committed (David, 2026-10-09:
    /// "Honor every removal outright").
    #[test]
    fn a_removed_rule_sends_what_it_kept_here() {
        let (dir, ws) = repo();
        fs::write(dir.path().join(".gitignore"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new(".gitignore")).unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();

        // Personal ignores ext/ in its working copy, uncommitted...
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join(".gitignore"), "/ext/\n").unwrap();
        });
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        fs::create_dir_all(dir.path().join("ext")).unwrap();
        fs::write(dir.path().join("ext/notes.txt"), "mine\n").unwrap();
        let ignoring = |vm: &Path, _: &GitWorkspace| {
            fs::write(vm.join(".gitignore"), "/ext/\n").unwrap();
        };
        let (tip, snap) = checkout_state(&ws, dir.path(), ignoring);
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert!(took.sent().is_empty(), "ignored for now");

        // ...and takes the rule back out.
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join(".gitignore"), "").unwrap();
        });
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join(".gitignore"), "").unwrap();
        });
        let took = Took::default();
        done(ws.mirror_from(&main, tip, snap, None, &took).unwrap());
        assert!(
            took.sent().iter().any(|(path, _)| path == "ext/notes.txt"),
            "{:?}",
            took.sent()
        );
        assert!(!dir.path().join(".git").join(RETIRED_HELD_FILE).exists());
    }

    /// A ref an earlier version kept beside the archive is let go at the
    /// first record.
    #[test]
    fn the_retired_sent_ref_is_deleted() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        let first = ws.commit("first").unwrap();
        ws.set_ref(RETIRED_SENT_REF, first).unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        done(
            ws.mirror_from(&main, tip, snap, None, &Took::default())
                .unwrap(),
        );
        assert_eq!(ws.read_ref(RETIRED_SENT_REF).unwrap(), None);
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
