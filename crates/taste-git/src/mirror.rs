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

/// What the folder has sent the checkout since the mirror last agreed with
/// it: [`MIRROR_REF`]'s tree with each sent path at the entry it was sent
/// as ([`GitWorkspace::record_sent`]). The checkout holding one of these is
/// holding the folder's own earlier version, so a later save of the same
/// file here is the folder's to send, not a conflict with itself (review,
/// 2026-09-23: two quick saves raised "your folder and Personal disagree"
/// over the user's own file).
pub const SENT_REF: &str = "refs/taste/mirror/sent";

/// One path the folder has sent the checkout since the two last agreed
/// ([`GitWorkspace::sent_against_folder`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentPath {
    pub path: PathBuf,
    /// The object it was sent as; `None`, sent as removed.
    pub sent: Option<Oid>,
    /// The object the folder's working copy holds now; `None`, absent.
    pub now: Option<Oid>,
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
    /// The folder is in the middle of something of git's or the user's own
    /// — a merge, a rebase, a detached HEAD, a branch the user switched to
    /// here — and a diff across it would read git's work as the user's
    /// edits. Nothing was written or sent; `reason` says what to do.
    Paused { reason: String },
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

/// Whether a path component names a repository's metadata directory, the
/// way git's `verify_path` sees it.
fn is_git_dir_name(name: &str) -> bool {
    let folded = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    folded == ".git" || folded == "git~1"
}

/// Where the mirror keeps the paths it will never touch or send: those the
/// folder ignored at the moment a pass changed a `.gitignore`, one per
/// line, inside the folder's own git directory.
const HELD_FILE: &str = "taste-mirror-held";

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
        self.mirror_from_within(branch, tip, snapshot, force, &|_| None)
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
        force: bool,
        room: &dyn Fn(u64) -> Option<String>,
    ) -> Result<Mirror> {
        let local = format!("refs/heads/{branch}");
        if !git2::Reference::is_valid_name(&local) {
            bail!("{branch} is not a branch name");
        }
        self.mirror_at(Some(&local), tip, snapshot, force, room)
    }

    /// [`Self::mirror_from_within`] for a checkout whose HEAD is detached
    /// at `tip`: the folder detached there too, its files Personal's.
    pub fn mirror_detached_within(
        &self,
        tip: Oid,
        snapshot: Oid,
        force: bool,
        room: &dyn Fn(u64) -> Option<String>,
    ) -> Result<Mirror> {
        self.mirror_at(None, tip, snapshot, force, room)
    }

    /// The mirror itself, onto the branch `local` names or — `None` — onto
    /// `tip` with no branch.
    fn mirror_at(
        &self,
        local: Option<&str>,
        tip: Oid,
        snapshot: Oid,
        force: bool,
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
        let mut current = self.worktree_tree()?;
        if current != baseline {
            // Each side's changes since the base, path by path, as the
            // entry each now has (`None`: deleted).
            let held = self.held_paths();
            let mut here = self.entries_between(baseline, current)?;
            // What the folder ignored before the checkout un-ignored it is
            // still the folder's own and stays here: a `.env` a `.gitignore`
            // change surfaced is not the user's edit to send.
            here.retain(|path, entry| !under_any(path, &held) && self.folder_owns(path, entry));
            let there = self.entries_between(baseline, target.id())?;
            // What the checkout holds because the folder sent it is the
            // folder's own, not a change of the checkout's.
            let sent = match self.read_ref(SENT_REF)? {
                Some(oid) => {
                    self.entries_between(baseline, self.repo.find_commit(oid)?.tree_id())?
                }
                None => Default::default(),
            };
            let theirs = |path: &PathBuf| {
                there
                    .get(path)
                    .filter(|entry| sent.get(path) != Some(*entry))
            };
            let conflicts: Vec<PathBuf> = here
                .iter()
                .filter(|(path, entry)| theirs(path).is_some_and(|t| t != *entry))
                .map(|(path, _)| path.clone())
                .collect();
            if !conflicts.is_empty() {
                if !force {
                    return Ok(Mirror::Drift { paths: conflicts });
                }
                // "Keep Personal's" is about the paths it was asked about:
                // those take the checkout's side here, and every other
                // change of the folder's still goes out below (review,
                // 2026-09-23: forcing the whole tree dropped new files
                // nobody had been asked about).
                let forced: std::collections::BTreeMap<_, _> = conflicts
                    .iter()
                    .map(|path| (path.clone(), there.get(path).cloned().flatten()))
                    .collect();
                let resolved = self.tree_with(current, &forced)?;
                self.write_tree_over(current, resolved)?;
                current = resolved;
                for path in &conflicts {
                    here.remove(path);
                }
            }
            // The folder's own changes the checkout does not yet have go
            // there first; the ones it already has are agreement.
            let outgoing: Vec<&PathBuf> = here
                .iter()
                .filter(|(path, entry)| there.get(*path) != Some(*entry))
                .map(|(path, _)| path)
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

        // On the branch — or, for a detached checkout, detached too.
        let on_branch = self.head_ref_name().as_deref() == local;
        let at_tip = self.repo.head().ok().and_then(|h| h.target()) == Some(tip);
        if current == target.id() && on_branch && at_tip {
            self.record_at(target.id(), local, tip)?;
            return Ok(Mirror::Unchanged);
        }

        if let Some(reason) = room(self.bytes_written_over(current, target.id())?) {
            return Ok(Mirror::Paused { reason });
        }
        self.hold_ignored_if_rules_change(current, target.id())?;
        let changed = self.write_tree_over(current, target.id())?;
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
        self.record_at(target.id(), local, tip)?;
        Ok(Mirror::Applied {
            changed,
            switched: !on_branch,
        })
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
        if self.read_ref(SENT_REF)?.is_some() {
            if let Ok(mut sent) = self.repo.find_reference(SENT_REF) {
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

    /// Note that `changes` were written into the checkout: until the mirror
    /// next agrees with it, the checkout holding one of them is holding
    /// the folder's own version ([`SENT_REF`]).
    pub fn record_sent(&self, changes: &[Change]) -> Result<()> {
        let base = match self.read_ref(SENT_REF)?.or(self.read_ref(MIRROR_REF)?) {
            Some(oid) => self.repo.find_commit(oid)?.tree_id(),
            None => match self.repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
                Some(tree) => tree.id(),
                None => self.repo.treebuilder(None)?.write()?,
            },
        };
        let mut entries = std::collections::BTreeMap::new();
        for change in changes {
            let entry = match &change.content {
                None => None,
                Some(bytes) => {
                    let mode = if change.link {
                        git2::FileMode::Link
                    } else if change.executable {
                        git2::FileMode::BlobExecutable
                    } else {
                        git2::FileMode::Blob
                    };
                    Some((self.repo.blob(bytes)?, i32::from(mode)))
                }
            };
            entries.insert(change.path.clone(), entry);
        }
        let tree = self.repo.find_tree(self.tree_with(base, &entries)?)?;
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost")?;
        let commit = self.repo.commit(
            None,
            &signature,
            &signature,
            "taste mirror: what the folder has sent since they last agreed",
            &tree,
            &[],
        )?;
        self.set_ref(SENT_REF, commit)
    }

    /// `base` with each of `entries` put in place (`None`: removed).
    fn tree_with(
        &self,
        base: Oid,
        entries: &std::collections::BTreeMap<PathBuf, Option<(Oid, i32)>>,
    ) -> Result<Oid> {
        let base = self.repo.find_tree(base)?;
        let mut update = git2::build::TreeUpdateBuilder::new();
        for (path, entry) in entries {
            match entry {
                Some((oid, mode)) => {
                    let mode = match *mode {
                        m if m == i32::from(git2::FileMode::BlobExecutable) => {
                            git2::FileMode::BlobExecutable
                        }
                        m if m == i32::from(git2::FileMode::Link) => git2::FileMode::Link,
                        m if m == i32::from(git2::FileMode::Commit) => git2::FileMode::Commit,
                        _ => git2::FileMode::Blob,
                    };
                    update.upsert(path, *oid, mode);
                }
                // Only what the base has: removing an absent path is an
                // error in libgit2, and a removal recorded as sent can name
                // a path the agreed tree never held.
                None => {
                    if base.get_path(path).is_ok() {
                        update.remove(path);
                    }
                }
            }
        }
        Ok(update.create_updated(&self.repo, &base)?)
    }

    /// Each path the folder has sent the checkout since the two last
    /// agreed ([`SENT_REF`]), with the object it was sent as and the one
    /// the folder's working copy holds now (`None`: absent). A checkout
    /// still holding what was sent, where the folder has since moved on —
    /// reverted it, most often — holds the folder's stale copy rather than
    /// work of its own; the sync drops it rather than carry it through its
    /// stash (`taste_devcontainer::peer`'s sync script).
    pub fn sent_against_folder(&self) -> Result<Vec<SentPath>> {
        let (Some(sent), Some(agreed)) = (self.read_ref(SENT_REF)?, self.read_ref(MIRROR_REF)?)
        else {
            return Ok(Vec::new());
        };
        let sent = self.repo.find_commit(sent)?.tree_id();
        let agreed = self.repo.find_commit(agreed)?.tree_id();
        let current = self.repo.find_tree(self.worktree_tree()?)?;
        Ok(self
            .entries_between(agreed, sent)?
            .into_iter()
            .map(|(path, entry)| SentPath {
                now: current.get_path(&path).ok().map(|e| e.id()),
                sent: entry.map(|(oid, _)| oid),
                path,
            })
            .collect())
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
        let held = self.held_paths();
        self.entries_between(baseline, current)?
            .iter()
            .filter(|(path, entry)| !under_any(path, &held) && self.folder_owns(path, entry))
            .map(|(path, _)| self.change_at(&current_tree, path))
            .collect()
    }

    /// Whether a difference between the baseline and the folder's working
    /// copy is the folder's own change to send. Ignored paths never are,
    /// in this direction as in the other ([`Self::write_tree_over`]): the
    /// working copy's tree leaves them out, so a baseline that holds them —
    /// one recorded from a checkout that did not ignore them yet — reads
    /// every one as deleted here. And a removal is never sent for a path
    /// still on this disk, whatever the rules say: absent from the tree is
    /// not absent from the folder (2026-10-04: a Drupal install, ignored
    /// here and not yet in the checkout, was deleted from the checkout
    /// file by file).
    fn folder_owns(&self, path: &Path, entry: &Option<(Oid, i32)>) -> bool {
        if self.repo.is_path_ignored(path).unwrap_or(true) {
            return false;
        }
        entry.is_some() || std::fs::symlink_metadata(self.workdir.join(path)).is_err()
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

    /// The paths held back from the mirror (`HELD_FILE`).
    fn held_paths(&self) -> Vec<PathBuf> {
        std::fs::read_to_string(self.repo.path().join(HELD_FILE))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    /// When this pass is about to change a `.gitignore`, hold back what the
    /// folder ignores right now: once the new rules are on disk, a file
    /// they no longer ignore would read as the folder's own change and go
    /// to the checkout — a secret the VM never had, carried across by the
    /// VM's own edit to the rules.
    fn hold_ignored_if_rules_change(&self, from: Oid, to: Oid) -> Result<()> {
        let from_tree = self.repo.find_tree(from)?;
        let to_tree = self.repo.find_tree(to)?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?;
        let rules_change = diff.deltas().any(|d| {
            [d.old_file().path(), d.new_file().path()]
                .into_iter()
                .flatten()
                .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"))
        });
        if !rules_change {
            return Ok(());
        }
        let mut held = self.held_paths();
        let statuses = self.repo.statuses(Some(
            git2::StatusOptions::new()
                .include_ignored(true)
                .recurse_ignored_dirs(false)
                .include_untracked(false),
        ))?;
        for entry in statuses.iter() {
            if entry.status().contains(git2::Status::IGNORED) {
                if let Some(path) = entry.path() {
                    let path = PathBuf::from(path.trim_end_matches('/'));
                    if !held.contains(&path) {
                        held.push(path);
                    }
                }
            }
        }
        let text: String = held.iter().map(|p| format!("{}\n", p.display())).collect();
        std::fs::write(self.repo.path().join(HELD_FILE), text)
            .context("recording the paths the mirror holds back")?;
        Ok(())
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
        if self.read_ref(SENT_REF)?.is_some() {
            if let Ok(mut sent) = self.repo.find_reference(SENT_REF) {
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

    /// The bytes writing `to` over `from` would put in the folder: every
    /// file the pass would write, whole — the room the write needs at its
    /// peak, since each is staged beside its old copy before replacing it.
    fn bytes_written_over(&self, from: Oid, to: Oid) -> Result<u64> {
        let from_tree = self.repo.find_tree(from)?;
        let to_tree = self.repo.find_tree(to)?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?;
        let odb = self.repo.odb()?;
        let mut bytes = 0u64;
        for delta in diff.deltas() {
            if matches!(delta.status(), Delta::Deleted) {
                continue;
            }
            let file = delta.new_file();
            if file.mode() == git2::FileMode::Commit {
                continue;
            }
            if let Ok((size, _)) = odb.read_header(file.id()) {
                bytes = bytes.saturating_add(size as u64);
            }
        }
        Ok(bytes)
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
        // Decided before anything is written, so a `.gitignore` this pass
        // writes cannot change the answer halfway: what the folder ignores
        // now, and what it held back when its rules last changed, is never
        // written over or removed — a checkout that adds `.env` does not
        // get to replace the user's (the docs' promise that ignored files
        // are never written or removed).
        let held = self.held_paths();
        let protected: std::collections::HashSet<PathBuf> = diff
            .deltas()
            .flat_map(|d| [d.old_file().path(), d.new_file().path()])
            .flatten()
            .filter(|path| {
                under_any(path, &held) || self.repo.is_path_ignored(path).unwrap_or(true)
            })
            .map(Path::to_path_buf)
            .collect();
        let skip = |delta: &git2::DiffDelta| {
            let path = delta.new_file().path().or(delta.old_file().path());
            path.is_some_and(|p| protected.contains(p))
        };
        // Every removal before any write: the delta lists `a` (now a file)
        // before `a/b` (a file of the directory it replaces), and written
        // in that order the file met the directory still standing and
        // failed the pass, every pass (review, 2026-09-23).
        let mut changed = 0;
        for delta in diff.deltas() {
            if skip(&delta) {
                continue;
            }
            if matches!(delta.status(), Delta::Deleted | Delta::Typechange) {
                if let Some(rel) = delta.old_file().path() {
                    self.remove_in_worktree(rel)?;
                }
            }
            if matches!(delta.status(), Delta::Deleted) {
                changed += 1;
            }
        }
        for delta in diff.deltas() {
            if skip(&delta) || matches!(delta.status(), Delta::Deleted) {
                continue;
            }
            changed += 1;
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
            // A path that was a file becoming one, a link, or a directory
            // the old tree never had: cleared first, so the write below
            // cannot land through a link.
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
                    // Written beside and renamed over, so a file is never
                    // seen half-written — by the user's editor, a build, or
                    // a close that stops the pass partway.
                    let name = absolute.file_name().unwrap_or_default().to_string_lossy();
                    let staging = absolute.with_file_name(format!(".{name}.taste-mirror"));
                    std::fs::write(&staging, blob.content())
                        .with_context(|| format!("writing {}", absolute.display()))?;
                    let executable = mode == git2::FileMode::BlobExecutable;
                    let mut permissions = std::fs::metadata(&staging)?.permissions();
                    let bits = match std::fs::metadata(&absolute) {
                        Ok(existing) => existing.permissions().mode(),
                        Err(_) => permissions.mode(),
                    };
                    permissions.set_mode(if executable {
                        bits | 0o111
                    } else {
                        bits & !0o111
                    });
                    std::fs::set_permissions(&staging, permissions)?;
                    if let Err(e) = std::fs::rename(&staging, &absolute) {
                        let _ = std::fs::remove_file(&staging);
                        return Err(e).with_context(|| format!("writing {}", absolute.display()));
                    }
                }
            }
        }
        Ok(changed)
    }

    /// A path inside the working tree, refused when it would reach outside
    /// it or into `.git` — a tree is data, and data does not get to choose
    /// where on this host it is written.
    fn checked_path(&self, rel: &Path) -> Result<PathBuf> {
        use std::path::Component;
        // `.git` in ANY component, as git's own verify_path refuses it:
        // a tree carrying `sub/.git/config` would plant a repository's
        // config in the folder, and git on this host reads it — and runs
        // what `core.fsmonitor` or an alias names — in that directory. Case
        // and the trailing dots, spaces, and 8.3 name some filesystems fold
        // into `.git` too.
        let ok = rel.components().all(|c| match c {
            Component::Normal(name) => !is_git_dir_name(&name.to_string_lossy()),
            _ => false,
        });
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

/// Whether `path` is one of `prefixes` or inside one.
fn under_any(path: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|prefix| path.starts_with(prefix))
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
        ws.mirror_from(&main, tip, snap, false).unwrap();
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
        // Forced: the checkout's wins for the path asked about, and a file
        // nobody was asked about still goes out rather than being deleted.
        fs::write(dir.path().join("new.txt"), "unasked\n").unwrap();
        match ws.mirror_from(&main, tip2, snap2, true).unwrap() {
            Mirror::Outgoing { changes } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].path, PathBuf::from("new.txt"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "edited in the vm\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "unasked\n"
        );
    }

    #[test]
    fn a_second_save_after_a_send_is_the_folders_not_a_conflict() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        ws.mirror_from(&main, tip, snap, false).unwrap();

        fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |_, _| {});
        let Mirror::Outgoing { changes } = ws.mirror_from(&main, tip2, snap2, false).unwrap()
        else {
            panic!("v1 goes out");
        };
        ws.record_sent(&changes).unwrap();

        // Saved again before the checkout's snapshot came back with v1.
        fs::write(dir.path().join("a.txt"), "v2\n").unwrap();
        let (tip3, snap3) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("a.txt"), "v1\n").unwrap();
        });
        match ws.mirror_from(&main, tip3, snap3, false).unwrap() {
            Mirror::Outgoing { changes } => {
                assert_eq!(changes[0].content.as_deref(), Some(&b"v2\n"[..]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_branch_the_user_switched_to_here_pauses_the_mirror() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        ws.mirror_from(&main, tip, snap, false).unwrap();

        let head = ws.repo.head().unwrap().peel_to_commit().unwrap();
        ws.repo.branch("mine", &head, false).unwrap();
        ws.repo.set_head("refs/heads/mine").unwrap();
        fs::write(dir.path().join("a.txt"), "on my branch\n").unwrap();
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |_, _| {});
        assert!(matches!(
            ws.mirror_from(&main, tip2, snap2, false).unwrap(),
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
        ws.mirror_from(&main, tip, snap, false).unwrap();

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
        let outcome = ws
            .mirror_detached_within(tip2, snap2, false, &|_| None)
            .unwrap();
        assert!(matches!(outcome, Mirror::Applied { .. }), "{outcome:?}");
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
            ws.mirror_detached_within(tip2, snap2, false, &|_| None)
                .unwrap(),
            Mirror::Unchanged
        );
    }

    /// A directory the checkout held untracked, and the folder then came to
    /// ignore, is never sent to the checkout as deleted: the files are
    /// still in the folder, and ignored paths are not the mirror's in either
    /// direction (2026-10-04: a Drupal install deleted from Personal).
    #[test]
    fn what_the_folder_ignores_is_never_sent_as_deleted() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::create_dir_all(vm.join("dep/sub")).unwrap();
            fs::write(vm.join("dep/sub/x.txt"), "installed\n").unwrap();
        });
        ws.mirror_from(&main, tip, snap, false).unwrap();
        assert!(dir.path().join("dep/sub/x.txt").exists());

        fs::write(dir.path().join(".gitignore"), "/dep/\n").unwrap();
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::create_dir_all(vm.join("dep/sub")).unwrap();
            fs::write(vm.join("dep/sub/x.txt"), "installed\n").unwrap();
        });
        match ws.mirror_from(&main, tip2, snap2, false).unwrap() {
            Mirror::Outgoing { changes } => {
                let paths: Vec<_> = changes.iter().map(|c| c.path.clone()).collect();
                assert_eq!(paths, [PathBuf::from(".gitignore")]);
            }
            other => panic!("{other:?}"),
        }
        assert!(ws
            .folder_changes()
            .unwrap()
            .iter()
            .all(|c| !c.path.starts_with("dep")));
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
            ws.mirror_from(&main, new_tip, old_snap, false).unwrap(),
            Mirror::Stale
        );
    }

    #[test]
    fn a_pass_the_disk_cannot_take_writes_nothing() {
        let (dir, ws) = repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let (tip, snap) = checkout_state(&ws, dir.path(), |_, _| {});
        ws.mirror_from(&main, tip, snap, false).unwrap();

        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join("big.bin"), vec![7u8; 4096]).unwrap();
        });
        let asked = std::cell::Cell::new(0u64);
        let result = ws
            .mirror_from_within(&main, tip2, snap2, false, &|bytes| {
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
        ws.mirror_from(&main, tip, snap, false).unwrap();

        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::remove_dir_all(vm.join("a")).unwrap();
            fs::write(vm.join("a"), "a file now\n").unwrap();
        });
        assert!(matches!(
            ws.mirror_from(&main, tip2, snap2, false).unwrap(),
            Mirror::Applied { .. }
        ));
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

    #[test]
    fn an_ignored_file_is_never_written_over_nor_sent_when_the_rules_change() {
        let (dir, ws) = repo();
        fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new(".gitignore")).unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        fs::write(dir.path().join(".env"), "SECRET=mine\n").unwrap();
        let main = ws.branch_name().unwrap();

        // The checkout un-ignores .env and writes one of its own.
        let (tip, snap) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join(".gitignore"), "").unwrap();
            fs::write(vm.join(".env"), "SECRET=theirs\n").unwrap();
        });
        ws.mirror_from(&main, tip, snap, false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET=mine\n",
            "an ignored file is not written over"
        );
        // The rules changed, so the next pass must not send the secret.
        let (tip2, snap2) = checkout_state(&ws, dir.path(), |vm, _| {
            fs::write(vm.join(".gitignore"), "").unwrap();
            fs::write(vm.join(".env"), "SECRET=theirs\n").unwrap();
            fs::write(vm.join("b.txt"), "more\n").unwrap();
        });
        if let Mirror::Outgoing { changes } = ws.mirror_from(&main, tip2, snap2, false).unwrap() {
            assert!(
                changes.iter().all(|c| c.path != Path::new(".env")),
                "the held-back file is never sent: {changes:?}"
            );
        }
        assert!(ws
            .folder_changes()
            .unwrap()
            .iter()
            .all(|c| c.path != Path::new(".env")));
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET=mine\n"
        );
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
