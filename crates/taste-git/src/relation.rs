//! Where a branch comes from and where it goes: the two relations the
//! header's ↓ and ↑ count.
//!
//! In the ordinary case they are one ref — `feature` tracks
//! `origin/feature` and pushes there — and that is how this used to be
//! modelled. They come apart in a fork's workflow, which is the common way
//! to contribute to someone else's project:
//!
//! - **The named-remote setup** git itself describes (`git help config`,
//!   `branch.<name>.pushRemote`): the upstream is `origin/main`, the push
//!   remote is the fork. Sync rebases onto `origin/main`; Push goes to
//!   `<fork>/<branch>`.
//! - **What `gh pr checkout` writes** for a pull request from a fork:
//!   `branch.<name>.remote` and `pushRemote` are the fork's URL, not a
//!   named remote, so git keeps no tracking ref for it and `@{upstream}`
//!   does not resolve. The push target is still known — that URL and the
//!   branch's name — and its tip is learnt by asking the remote
//!   (`git ls-remote`, [`record_remote_tip`]), never by writing a ref into
//!   the user's repository. The base is what the pull request is against,
//!   which git does not record; the default branch of the repository's own
//!   remote (`refs/remotes/<remote>/HEAD`, which `git clone` sets) is the
//!   answer git can give.
//!
//! Every input is git's own documented configuration: `branch.<name>.
//! remote`, `.merge`, `.pushRemote`, `remote.pushDefault`, and
//! `push.default`, read as git reads them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use git2::{BranchType, Oid};

use crate::{GitWorkspace, SyncStatus};

/// The most commits either side of a divergence is walked for to compare
/// them change by change ([`GitWorkspace::diverged`]).
const DIVERGENCE_LIMIT: usize = 1000;

/// How far back a branch's reflog is read for a commit it held
/// (`GitWorkspace::has_held`): a branch's last hundred moves.
const HELD_LOOKBACK: usize = 100;

/// What Pull would do ([`GitWorkspace::pull_plan_of`]). A click does the
/// first two; the third is a rebase, which replays the branch's own
/// commits, and so is held for (David, 2026-10-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPlan {
    /// The target has nothing the branch does not.
    UpToDate,
    /// The target is ahead by `theirs`, and the branch has nothing of its
    /// own: it moves there.
    FastForward { theirs: usize },
    /// Both moved: the branch's `mine` commits replayed onto the target's
    /// `theirs`.
    Rebase { mine: usize, theirs: usize },
    /// Both moved, but the target's `theirs` are this branch's own from
    /// before it was rewritten here — rebased, reworded, amended
    /// ([`GitWorkspace::rewrite_dropping`]). Nothing to pull: a rebase onto
    /// them replays the branch onto its own old copy, which is how a
    /// rebase the agent had done was undone (2026-10-19). The push
    /// replaces them; `whole` says everything they change is in the branch
    /// ([`Carried`]).
    Rewritten {
        mine: usize,
        theirs: usize,
        whole: bool,
    },
}

/// What a push replacing what the remote has with this branch would lose,
/// judged in aggregate, by content: the remote's whole change since the
/// two parted, merged into this branch's tip
/// ([`GitWorkspace::rewrite_dropping`]). Where that merge changes nothing,
/// this branch has every change the remote made — the same, or more —
/// however its commits were cut, reworded, or reordered (David,
/// 2026-10-22: "analyze whether the changes are the same (or a superset)
/// in aggregate, not just match up commits"). Files by path, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    /// Files where the remote's change is not in this branch: merging it
    /// in would change them, cleanly. The push drops those changes.
    pub missing: Vec<String>,
    /// Files both sides changed in the same places, differently: merging
    /// conflicts, so whether the remote's change is in this branch cannot
    /// be said. The push replaces the remote's version with this one.
    pub differing: Vec<String>,
    /// Each of those files with its evidence, for the push card's section
    /// on them: missing ones first, then differing, each by path; at most
    /// [`LOSS_DETAILS`] of them.
    pub details: Vec<Loss>,
}

/// One file a push would not keep, with what shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loss {
    pub path: String,
    /// Changed differently here and there (a conflict), rather than a
    /// change this branch lacks.
    pub differing: bool,
    /// For a missing change, the unified diff merging it into this branch
    /// would apply to the file; for a differing one, the conflicting
    /// regions as git marks them, "this branch" against "there". At most
    /// [`LOSS_LINES`] lines.
    pub detail: String,
    /// The commits there that changed this file, newest first, each as its
    /// summary and its author's name, and whether that author is someone
    /// other than this branch's (`SyncStatus::others_there`).
    pub changed_by: Vec<(String, String, bool)>,
}

/// How many files a [`Carried`] carries the evidence of.
pub const LOSS_DETAILS: usize = 40;

/// How many of the replaced commits are read for which files they changed
/// ([`Loss::changed_by`]).
pub const ATTRIBUTED: usize = 200;

/// How many lines of evidence a [`Loss`] carries.
pub const LOSS_LINES: usize = 400;

impl Carried {
    /// Everything the remote changed is in this branch: nothing is lost.
    pub fn whole(&self) -> bool {
        self.missing.is_empty() && self.differing.is_empty()
    }
}

/// The branch a branch is rebased onto to bring a pull request up to date
/// ([`GitWorkspace::canonical_of`]): the header's ⇊.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    /// The full ref, `refs/remotes/origin/main`.
    pub full: String,
    /// As a person reads it, `origin/main`.
    pub name: String,
    /// Commits there whose change the branch does not have yet: what a
    /// rebase onto it brings in. Merges, and commits the branch already has
    /// a copy of, are not counted.
    pub new: usize,
    /// The branch's own commits: those whose change is not there. What a
    /// pull request from it shows.
    pub own: usize,
    /// The commits [`Canonical::new`] counts: what ⇊'s card lists.
    pub incoming: crate::CommitList,
}

/// Where a branch's push goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// A configured remote's name, or a URL when the branch names one
    /// directly (`gh pr checkout`).
    pub remote: String,
    /// The branch name on that remote.
    pub branch: String,
    /// `remote` is a configured remote, with tracking refs under
    /// `refs/remotes/<remote>/`.
    pub named: bool,
}

impl PushTarget {
    /// How a person reads it: `origin/feature`, or the branch on a URL.
    pub fn display(&self) -> String {
        if self.named {
            format!("{}/{}", self.remote, self.branch)
        } else {
            format!("{} on {}", self.branch, self.remote)
        }
    }

    /// Where the push target's tip is kept here, for a named remote:
    /// `refs/remotes/<remote>/<branch>`.
    pub fn tracking_ref(&self) -> Option<String> {
        self.named
            .then(|| format!("refs/remotes/{}/{}", self.remote, self.branch))
    }
}

/// What a URL remote's branches were last seen to point at: `Some(None)`
/// for a branch asked about and absent. Process-wide, because the
/// repository handles that read it are opened per refresh; lost with the
/// process, and asked again by the first fetch of the next.
type RemoteTips = Mutex<HashMap<(String, String), Option<Oid>>>;

fn remote_tips() -> &'static RemoteTips {
    static TIPS: OnceLock<RemoteTips> = OnceLock::new();
    TIPS.get_or_init(Default::default)
}

/// Record what `git ls-remote <url> refs/heads/<branch>` printed: the
/// branch's tip there, or nothing when the remote has no such branch.
pub fn record_remote_tip(url: &str, branch: &str, ls_remote_stdout: &str) {
    let wanted = format!("refs/heads/{branch}");
    let tip = ls_remote_stdout.lines().find_map(|line| {
        let (oid, name) = line.split_once('\t')?;
        (name.trim() == wanted).then(|| Oid::from_str(oid.trim()).ok())?
    });
    remote_tips()
        .lock()
        .unwrap()
        .insert((url.to_string(), branch.to_string()), tip);
}

fn remote_tip(url: &str, branch: &str) -> Option<Option<Oid>> {
    remote_tips()
        .lock()
        .unwrap()
        .get(&(url.to_string(), branch.to_string()))
        .copied()
}

impl GitWorkspace {
    pub(crate) fn config_string(&self, key: &str) -> Option<String> {
        self.repo.config().ok()?.get_string(key).ok()
    }

    pub(crate) fn is_named_remote(&self, remote: &str) -> bool {
        self.repo.find_remote(remote).is_ok()
    }

    /// Where `branch` pushes, as `git push` with no arguments would send
    /// it: the remote from `branch.<name>.pushRemote`, `remote.pushDefault`,
    /// `branch.<name>.remote`, or the default remote, in that order; the
    /// name on it from `push.default` — the upstream's branch only under
    /// `upstream` (or `tracking`) and only to the upstream's own remote,
    /// the branch's own name otherwise, which is what the default `simple`
    /// pushes or refuses to.
    pub fn push_target(&self, branch: &str) -> PushTarget {
        let upstream_remote = self.config_string(&format!("branch.{branch}.remote"));
        let remote = self
            .config_string(&format!("branch.{branch}.pushRemote"))
            .or_else(|| self.config_string("remote.pushDefault"))
            .or_else(|| upstream_remote.clone())
            .unwrap_or_else(|| self.default_remote());
        let merge = self
            .config_string(&format!("branch.{branch}.merge"))
            .and_then(|merge| merge.strip_prefix("refs/heads/").map(str::to_string));
        let push_default = self
            .config_string("push.default")
            .unwrap_or_else(|| "simple".into());
        let to_upstream_name = matches!(push_default.as_str(), "upstream" | "tracking")
            && upstream_remote.as_deref() == Some(remote.as_str());
        let name = match merge {
            Some(merge) if to_upstream_name => merge,
            _ => branch.to_string(),
        };
        PushTarget {
            named: self.is_named_remote(&remote),
            remote,
            branch: name,
        }
    }

    /// What Sync rebases `branch` onto when that is not its push target:
    /// the upstream when it is another ref (the named-remote fork setup, or
    /// a branch made from `origin/main`), and the repository remote's
    /// default branch when the upstream is a URL with no tracking ref
    /// (`gh pr checkout`). `None` when the push target is the base too —
    /// the ordinary case — or when there is nothing to rebase onto.
    /// A full ref name.
    pub fn base_ref_of(&self, branch: &str) -> Option<String> {
        let target = self.push_target(branch);
        let local = self.repo.find_branch(branch, BranchType::Local).ok()?;
        if let Ok(upstream) = local.upstream() {
            let name = upstream.get().name()?.to_string();
            return (Some(&name) != target.tracking_ref().as_ref()).then_some(name);
        }
        let upstream_remote = self.config_string(&format!("branch.{branch}.remote"))?;
        if self.is_named_remote(&upstream_remote) {
            return None;
        }
        // The upstream names a URL: the remote this repository was cloned
        // from has the default branch it is a contribution to.
        let head = self
            .repo
            .find_reference(&format!("refs/remotes/{}/HEAD", self.default_remote()))
            .ok()?;
        head.symbolic_target().map(str::to_string)
    }

    /// Make Personal's `branch` what an environment's `branch` pulls from,
    /// in that environment's peer: a remote `personal` reading the folder's
    /// copy of Personal's branches (`refs/taste/vm/*` in `folder`), fetched
    /// now, and — as git's fork setup has it — that remote as the branch's
    /// upstream with its push remote kept where it was. The file tree's
    /// Pull then counts and rebases against `personal/<branch>`, and its
    /// Push goes where it went. A branch whose upstream someone chose — any
    /// remote but the clone's own `origin`, or one with a push remote of
    /// its own — is left alone, as is one Personal does not have. Says
    /// whether the branch now pulls from Personal.
    pub fn track_personal(&self, folder: &Path, branch: &str) -> Result<bool> {
        const REMOTE: &str = "personal";
        const FETCH: &str = "+refs/taste/vm/*:refs/remotes/personal/*";
        let url = folder.to_string_lossy().to_string();
        match self.repo.find_remote(REMOTE) {
            Ok(remote) if remote.url() == Some(url.as_str()) => {}
            Ok(_) => self.repo.remote_set_url(REMOTE, &url)?,
            Err(_) => {
                self.repo.remote_with_fetch(REMOTE, &url, FETCH)?;
            }
        }
        self.update_refs_from(folder, &[FETCH])?;
        if self
            .repo
            .find_reference(&format!("refs/remotes/{REMOTE}/{branch}"))
            .is_err()
            || self.repo.find_branch(branch, BranchType::Local).is_err()
        {
            return Ok(false);
        }
        let remote = self.config_string(&format!("branch.{branch}.remote"));
        let push_remote = self.config_string(&format!("branch.{branch}.pushRemote"));
        match (remote.as_deref(), push_remote) {
            (Some(REMOTE), _) => return Ok(true),
            (None, _) | (Some("origin"), None) => {}
            _ => return Ok(false),
        }
        let mut config = self.repo.config()?;
        if let Some(remote) = &remote {
            config.set_str(&format!("branch.{branch}.pushRemote"), remote)?;
        }
        config.set_str(&format!("branch.{branch}.remote"), REMOTE)?;
        config.set_str(
            &format!("branch.{branch}.merge"),
            &format!("refs/taste/vm/{branch}"),
        )?;
        Ok(true)
    }

    /// The ref Sync rebases `branch` onto: its base when it has one, its
    /// upstream otherwise. A full ref name, or `None` with neither.
    pub fn rebase_target_of(&self, branch: &str) -> Option<String> {
        self.base_ref_of(branch)
            .or_else(|| self.upstream_ref_of(branch))
    }

    /// `git rebase --autostash <target>`: Sync's rebase, onto
    /// [`GitWorkspace::rebase_target_of`].
    pub fn rebase_onto_command(&self, target: &str) -> (String, Vec<String>) {
        self.git_command(&["rebase", "--autostash", target])
    }

    /// `git merge --ff-only --autostash <target>`: Pull's click, which
    /// moves the branch only where nothing of its own has to be replayed,
    /// and refuses rather than rewrite should a commit have landed since
    /// [`GitWorkspace::pull_plan_of`] looked.
    pub fn fast_forward_onto_command(&self, target: &str) -> (String, Vec<String>) {
        self.git_command(&["merge", "--ff-only", "--autostash", target])
    }

    /// What Pull would do to `branch` against its rebase target
    /// ([`GitWorkspace::rebase_target_of`]), as the refs here stand: so
    /// asked after the fetch. `None` with no target, or a branch or target
    /// that does not resolve.
    pub fn pull_plan_of(&self, branch: &str) -> Option<PullPlan> {
        let target = self.rebase_target_of(branch)?;
        // Against its own push target, a branch that diverged may only
        // have been rewritten here; against a base — a fork's `origin/main`
        // — divergence is what Sync is for.
        let own = self.base_ref_of(branch).is_none();
        self.pull_plan_onto(branch, &target, own)
    }

    /// The folder's remotes and branch upstreams, as Personal's checkout
    /// in a VM is given them (`taste_devcontainer::peer::share_tracking`):
    /// each remote's URL and fetch rules, and each branch's `remote`,
    /// `merge`, and `pushRemote` — from this repository's own config, in
    /// git's lower-cased key form. A URL's userinfo is left out where it
    /// can be a secret (an `https://token@host` remote), and push URLs and
    /// URL rewrites are not given at all: the checkout cannot reach a
    /// remote, and the user's credentials never go where an agent works.
    /// So `git status` there counts against the same upstream the user's
    /// does, and nothing there sets one (David, 2026-10-20: "I don't even
    /// want the agent to be able to set it; I just want it to be correct").
    pub fn tracking_config(&self) -> Vec<(String, String)> {
        let Ok(config) = self.repo.config() else {
            return Vec::new();
        };
        let Ok(local) = config.open_level(git2::ConfigLevel::Local) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let Ok(entries) = local.entries(None) else {
            return out;
        };
        let mut entries = entries;
        while let Some(Ok(entry)) = entries.next() {
            let (Some(name), Some(value)) = (entry.name(), entry.value()) else {
                continue;
            };
            if !is_tracking_key(name) {
                continue;
            }
            // Every value, not only a remote's url: a branch's `remote`
            // and `pushremote` can be URLs too (`gh pr checkout` writes
            // them), and a token in one is as much a secret (review,
            // 2026-10-20). A name is left as it is.
            out.push((name.to_string(), without_secret(value)));
        }
        out
    }

    /// What the header's ↓ pulls `branch` from: its push target's tip as
    /// it is kept here (`refs/remotes/<remote>/<branch>`), when the push
    /// target is a named remote that has the branch. A full ref name.
    pub fn own_target_of(&self, branch: &str) -> Option<String> {
        let tracking = self.push_target(branch).tracking_ref()?;
        self.repo.find_reference(&tracking).ok()?;
        Some(tracking)
    }

    /// What a pull request's branch is rebased onto: the branch's base when
    /// it has one (a fork's `upstream/main`), otherwise the default remote's
    /// default branch — `refs/remotes/<remote>/HEAD`, which `git clone`
    /// sets, else its `main` or `master`. Never a local branch, which is
    /// only as fresh as its last pull. `None` when that is where `branch`
    /// itself pushes — the default branch has nothing to rebase onto but
    /// its own copy, which is Pull's — or when there is no such ref. A full
    /// ref name, resolved past `HEAD`.
    pub fn canonical_ref_of(&self, branch: &str) -> Option<String> {
        let candidate = self.canonical_candidate(branch)?;
        let own = self.push_target(branch).tracking_ref();
        (own.as_deref() != Some(candidate.as_str())).then_some(candidate)
    }

    /// Whether `branch` pushes to the canonical branch itself — it is the
    /// default branch's checkout — so a push of it moves what every other
    /// branch is rebased onto, and is held for even as a fast-forward
    /// (David, 2026-10-19).
    pub fn pushes_to_canonical(&self, branch: &str) -> bool {
        let own = self.push_target(branch).tracking_ref();
        own.is_some() && self.canonical_candidate(branch) == own
    }

    /// [`GitWorkspace::canonical_ref_of`] before the branch's own push
    /// target is ruled out.
    fn canonical_candidate(&self, branch: &str) -> Option<String> {
        let candidate = self.base_ref_of(branch).or_else(|| {
            let remote = self.default_remote();
            self.repo
                .find_reference(&format!("refs/remotes/{remote}/HEAD"))
                .ok()
                .and_then(|head| head.symbolic_target().map(str::to_string))
                .or_else(|| {
                    ["main", "master"]
                        .iter()
                        .map(|name| format!("refs/remotes/{remote}/{name}"))
                        .find(|name| self.repo.find_reference(name).is_ok())
                })
        })?;
        self.repo.find_reference(&candidate).ok()?;
        Some(candidate)
    }

    /// `branch` against [`GitWorkspace::canonical_ref_of`], counted by
    /// change rather than by commit, as `git cherry` counts: a branch
    /// already rebased onto it reads nothing new, whatever the hashes.
    pub fn canonical_of(&self, branch: &str) -> Option<Canonical> {
        let full = self.canonical_ref_of(branch)?;
        let there = self.repo.find_reference(&full).ok()?.target()?;
        let tip = self
            .repo
            .find_branch(branch, BranchType::Local)
            .ok()?
            .get()
            .target()?;
        let (incoming, own) = match self.diverged(tip, there) {
            Some((theirs, mine)) => {
                let new = self.unmatched_of(&theirs, &mine);
                (
                    self.commit_list(new.iter().copied(), new.len()),
                    self.unmatched(&mine, &theirs),
                )
            }
            // Too far apart to compare change by change: by commit.
            None => {
                let (own, new) = self.repo.graph_ahead_behind(tip, there).ok()?;
                (self.commits_between(there, &[tip], new), own)
            }
        };
        Some(Canonical {
            name: short_ref(&full),
            full,
            new: incoming.commits.len() + incoming.more,
            own,
            incoming,
        })
    }

    /// The commits `from` has that none of `hide` do, newest first, as a
    /// card lists them; `count` is how many there are, already counted.
    fn commits_between(&self, from: Oid, hide: &[Oid], count: usize) -> crate::CommitList {
        let Ok(mut walk) = self.repo.revwalk() else {
            return crate::CommitList::default();
        };
        if walk.push(from).is_err() || hide.iter().any(|oid| walk.hide(*oid).is_err()) {
            return crate::CommitList::default();
        }
        let _ = walk.set_sorting(git2::Sort::TIME);
        let listed: Vec<Oid> = walk
            .filter_map(Result::ok)
            .take(crate::COMMIT_LIST_CAP)
            .collect();
        self.commit_list(listed, count)
    }

    /// `oids`, up to [`crate::COMMIT_LIST_CAP`] of them, as a card lists
    /// them, out of `count`.
    fn commit_list(&self, oids: impl IntoIterator<Item = Oid>, count: usize) -> crate::CommitList {
        let commits: Vec<crate::CommitLine> = oids
            .into_iter()
            .take(crate::COMMIT_LIST_CAP)
            .filter_map(|oid| self.commit_line(oid))
            .collect();
        crate::CommitList {
            more: count.saturating_sub(commits.len()),
            commits,
        }
    }

    fn commit_line(&self, oid: Oid) -> Option<crate::CommitLine> {
        let commit = self.repo.find_commit(oid).ok()?;
        let message = String::from_utf8_lossy(commit.message_bytes()).to_string();
        let (summary, body) = match message.split_once('\n') {
            Some((summary, body)) => (summary.trim().to_string(), body.trim().to_string()),
            None => (message.trim().to_string(), String::new()),
        };
        let author = commit.author();
        Some(crate::CommitLine {
            id: oid.to_string(),
            summary,
            body,
            author: String::from_utf8_lossy(author.name_bytes()).to_string(),
            when: commit.time().seconds(),
        })
    }

    /// What a pull of `branch` from `target` (a full ref) would do, as the
    /// refs here stand. `own` says `target` is where the branch pushes, so
    /// a divergence may be its own rewrite ([`PullPlan::Rewritten`]).
    /// `None` for a branch or target that does not resolve.
    pub fn pull_plan_onto(&self, branch: &str, target: &str, own: bool) -> Option<PullPlan> {
        let theirs_tip = self.repo.find_reference(target).ok()?.target()?;
        let mine_tip = self
            .repo
            .find_branch(branch, BranchType::Local)
            .ok()?
            .get()
            .target()?;
        let (mine, theirs) = self.repo.graph_ahead_behind(mine_tip, theirs_tip).ok()?;
        Some(match (mine, theirs) {
            (_, 0) => PullPlan::UpToDate,
            (0, theirs) => PullPlan::FastForward { theirs },
            (mine, theirs) => match own
                .then(|| self.rewrite_dropping(branch, mine_tip, theirs_tip))
                .flatten()
            {
                Some(carried) => PullPlan::Rewritten {
                    mine,
                    theirs,
                    whole: carried.whole(),
                },
                None => PullPlan::Rebase { mine, theirs },
            },
        })
    }

    /// The commits each of `a` and `b` has that the other lacks: `(only
    /// b's, only a's)`, newest first. `None` when either side is past
    /// [`DIVERGENCE_LIMIT`], which is no comparison anyone should be told
    /// the answer to by change.
    fn diverged(&self, a: Oid, b: Oid) -> Option<(Vec<Oid>, Vec<Oid>)> {
        let only = |from: Oid, hide: Oid| -> Option<Vec<Oid>> {
            let mut walk = self.repo.revwalk().ok()?;
            walk.push(from).ok()?;
            walk.hide(hide).ok()?;
            let commits: Vec<Oid> = walk
                .take(DIVERGENCE_LIMIT + 1)
                .filter_map(Result::ok)
                .collect();
            (commits.len() <= DIVERGENCE_LIMIT).then_some(commits)
        };
        Some((only(b, a)?, only(a, b)?))
    }

    /// How many of `commits` make a change none of `against` does: merges
    /// passed over, having no change of their own to compare.
    fn unmatched(&self, commits: &[Oid], against: &[Oid]) -> usize {
        self.unmatched_of(commits, against).len()
    }

    /// The commits [`Self::unmatched`] counts, in the order given.
    fn unmatched_of(&self, commits: &[Oid], against: &[Oid]) -> Vec<Oid> {
        let theirs: std::collections::HashSet<Change> = against
            .iter()
            .filter_map(|oid| self.change_of(*oid))
            .collect();
        commits
            .iter()
            .copied()
            .filter(|oid| !self.is_merge(*oid))
            .filter(|oid| {
                self.change_of(*oid)
                    .is_none_or(|key| !theirs.contains(&key))
            })
            .collect()
    }

    fn is_merge(&self, oid: Oid) -> bool {
        self.repo
            .find_commit(oid)
            .is_ok_and(|commit| commit.parent_count() > 1)
    }

    /// Whether the commits `there` has that `tip` lacks are `branch`'s own
    /// from before it was rewritten here, and if so what a push replacing
    /// them would lose ([`Carried`]). Its own when either holds:
    ///
    /// - every one of them makes the same change as a commit `tip` has
    ///   that `there` lacks (git's patch-id, as `git cherry` compares) —
    ///   a rebase, a reword, an amend that kept the change; merges are
    ///   passed over, having no change of their own to compare;
    /// - `there` is a commit `branch` itself has held — pointed at, or had
    ///   in its history (its reflog): everything there was in this branch,
    ///   whatever was done to it since. Pointed at alone missed a branch
    ///   that took the remote's tip in a merge and was then rewritten from
    ///   its root, with files taken out of every commit: no patch matched,
    ///   it never pointed at the tip, and the push replacing the remote's
    ///   history was offered as an ordinary one, with a pull beside it
    ///   (2026-10-22: "Why doesn't push show as a forced one, given the
    ///   lack of common ancestry?").
    ///
    /// `None` when neither holds: someone else's commits, which are for a
    /// rebase to take. A rewrite the agent made in a checkout in a VM
    /// reaches the folder's reflog as the IDE moving the branch, so it
    /// reads the same here.
    pub fn rewrite_dropping(&self, branch: &str, tip: Oid, there: Oid) -> Option<Carried> {
        let (theirs, mine) = self.diverged(tip, there)?;
        if theirs.is_empty() || mine.is_empty() {
            return None;
        }
        let held = self.has_held(branch, there);
        // A merge there is someone's — a host's "Update branch", a
        // maintainer's — and has no change of its own to find here, so
        // only the branch having held it makes it this branch's
        // (review, 2026-10-20: a merge alone read as "nothing is lost").
        if !held && theirs.iter().any(|oid| self.is_merge(*oid)) {
            return None;
        }
        // Whose they are is one question, what a push loses another. Its
        // own when it pointed there, or when each of them makes a change
        // one of its own commits makes; content alone cannot say whose —
        // someone's empty commit changes nothing, and so is "here". What
        // replacing them loses is judged by content, in aggregate.
        let own = held || self.unmatched(&theirs, &mine) == 0;
        own.then(|| self.carried(tip, there, &theirs, &mine))
    }

    /// Whether `branch` has ever held `there`: pointed at it, or at a
    /// commit with it in its history, by the branch's reflog. Its newest
    /// [`HELD_LOOKBACK`] entries, since this is asked at every refresh of
    /// the sync counts; and a yes is kept, a reflog only gaining entries
    /// until it is expired.
    fn has_held(&self, branch: &str, there: Oid) -> bool {
        thread_local! {
            static HELD: std::cell::RefCell<
                std::collections::HashSet<(std::path::PathBuf, String, Oid)>,
            > = std::cell::RefCell::new(std::collections::HashSet::new());
        }
        let key = (self.repo.path().to_path_buf(), branch.to_string(), there);
        if HELD.with(|held| held.borrow().contains(&key)) {
            return true;
        }
        let held = self
            .repo
            .reflog(&format!("refs/heads/{branch}"))
            .ok()
            .is_some_and(|log| {
                log.iter().take(HELD_LOOKBACK).any(|entry| {
                    let at = entry.id_new();
                    at == there
                        || (!at.is_zero()
                            && self.repo.graph_descendant_of(at, there).unwrap_or(false))
                })
            });
        if held {
            HELD.with(|cache| cache.borrow_mut().insert(key));
        }
        held
    }

    /// What a push of `tip` over `there` would lose ([`Carried`]): `there`
    /// merged into `tip` from where they parted, and each file the merge
    /// would leave other than `tip` has it named — `missing` where the
    /// merge is clean, `differing` where it conflicts. A file the remote
    /// deleted that `tip` keeps is missing too: its deletion is a change.
    /// With no common ancestor, or a merge that cannot be made, every file
    /// the remote has that `tip` does not have alike is differing.
    fn carried(&self, tip: Oid, there: Oid, theirs: &[Oid], mine: &[Oid]) -> Carried {
        // Asked at every refresh of the sync counts, several a second on a
        // checkout in a VM, about the same two tips: a merge and its diffs
        // once per pair.
        thread_local! {
            static KNOWN: std::cell::RefCell<HashMap<(Oid, Oid), Carried>> =
                std::cell::RefCell::new(HashMap::new());
        }
        if let Some(known) = KNOWN.with(|known| known.borrow().get(&(tip, there)).cloned()) {
            return known;
        }
        let mut carried = self.carried_now(tip, there);
        self.attribute(&mut carried, theirs, mine);
        KNOWN.with(|known| {
            let mut known = known.borrow_mut();
            if known.len() >= 16 {
                known.clear();
            }
            known.insert((tip, there), carried.clone());
        });
        carried
    }

    /// A commit's author's email, lower-cased, and name, and its summary.
    fn authored(&self, oid: Oid) -> Option<(String, String, String)> {
        self.repo.find_commit(oid).ok().map(|commit| {
            let author = commit.author();
            (
                author.email().unwrap_or_default().to_lowercase(),
                author.name().unwrap_or_default().to_string(),
                commit.summary().unwrap_or_default().to_string(),
            )
        })
    }

    /// This branch's authors: who wrote `mine`, and the project's author
    /// here — the repository's own `user.email`, never the host's.
    fn our_authors(&self, mine: &[Oid]) -> std::collections::HashSet<String> {
        let mut ours: std::collections::HashSet<String> = mine
            .iter()
            .filter_map(|oid| self.authored(*oid))
            .map(|(email, _, _)| email)
            .collect();
        if let Some(identity) = crate::local_identity(&self.repo) {
            ours.insert(identity.email.to_lowercase());
        }
        ours
    }

    /// Who other than this branch's authors wrote the commits `there` has
    /// that `tip` lacks, and how many each, most first: what a pull brings
    /// from someone else, or a push replacing them overwrites (David,
    /// 2026-10-22: "consider if new remote commits or content appears to
    /// be from a different author"). Empty past [`DIVERGENCE_LIMIT`].
    pub(crate) fn others_there(&self, tip: Oid, there: Oid) -> Vec<(String, usize)> {
        let Some((theirs, mine)) = self.diverged(tip, there) else {
            return Vec::new();
        };
        let ours = self.our_authors(&mine);
        let mut others: std::collections::BTreeMap<String, usize> = Default::default();
        for oid in &theirs {
            if let Some((email, name, _)) = self.authored(*oid) {
                if !ours.contains(&email) {
                    *others.entry(format!("{name} <{email}>")).or_default() += 1;
                }
            }
        }
        let mut others: Vec<(String, usize)> = others.into_iter().collect();
        others.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        others
    }

    /// For each file `carried` names, the commits of `theirs` that changed
    /// it, and whether each is by someone other than this branch's authors
    /// (`mine`'s, and `user.email`). At most [`ATTRIBUTED`] of `theirs`
    /// are read.
    fn attribute(&self, carried: &mut Carried, theirs: &[Oid], mine: &[Oid]) {
        let ours = self.our_authors(mine);
        let author = |oid: Oid| self.authored(oid);
        if carried.details.is_empty() {
            return;
        }
        let wanted: std::collections::HashSet<String> = carried
            .details
            .iter()
            .map(|loss| loss.path.clone())
            .collect();
        let mut by_path: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();
        for oid in theirs.iter().take(ATTRIBUTED) {
            let Ok(commit) = self.repo.find_commit(*oid) else {
                continue;
            };
            if commit.parent_count() > 1 {
                continue;
            }
            let Some((email, name, summary)) = author(*oid) else {
                continue;
            };
            let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
            let Ok(tree) = commit.tree() else { continue };
            let Ok(diff) = self
                .repo
                .diff_tree_to_tree(parent.as_ref(), Some(&tree), None)
            else {
                continue;
            };
            for delta in diff.deltas() {
                let path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|path| path.display().to_string());
                if let Some(path) = path.filter(|path| wanted.contains(path)) {
                    by_path.entry(path).or_default().push((
                        summary.clone(),
                        name.clone(),
                        !ours.contains(&email),
                    ));
                }
            }
        }
        for loss in &mut carried.details {
            loss.changed_by = by_path.remove(&loss.path).unwrap_or_default();
        }
    }

    /// [`Self::carried`], worked out.
    fn carried_now(&self, tip: Oid, there: Oid) -> Carried {
        let tree = |oid: Oid| self.repo.find_commit(oid).ok().and_then(|c| c.tree().ok());
        let (Some(ours), Some(theirs)) = (tree(tip), tree(there)) else {
            return Carried::default();
        };
        let merged = self
            .repo
            .merge_base(tip, there)
            .ok()
            .and_then(tree)
            .and_then(|base| self.repo.merge_trees(&base, &ours, &theirs, None).ok());
        let Some(merged) = merged else {
            return Carried {
                differing: differing_files(&self.repo, &ours, &theirs),
                ..Carried::default()
            };
        };
        let mut carried = Carried::default();
        let mut conflicted = std::collections::BTreeSet::new();
        let mut differing_details = Vec::new();
        if let Ok(conflicts) = merged.conflicts() {
            for conflict in conflicts.flatten() {
                let path = [&conflict.our, &conflict.their, &conflict.ancestor]
                    .into_iter()
                    .flatten()
                    .map(|entry| String::from_utf8_lossy(&entry.path).to_string())
                    .next();
                if let Some(path) = path {
                    if differing_details.len() < LOSS_DETAILS {
                        differing_details.push(Loss {
                            detail: self.conflict_detail(&conflict),
                            path: path.clone(),
                            differing: true,
                            changed_by: Vec::new(),
                        });
                    }
                    conflicted.insert(path);
                }
            }
        }
        // The merge's side of each missing file, for its diff: `None` for
        // one the remote deleted.
        let mut merged_ids: HashMap<String, Option<Oid>> = HashMap::new();
        // Every file the clean part of the merge leaves, against `tip`'s.
        let mut seen = std::collections::BTreeSet::new();
        for entry in merged.iter() {
            let path = String::from_utf8_lossy(&entry.path).to_string();
            if conflicted.contains(&path) {
                continue;
            }
            let same = ours
                .get_path(Path::new(&path))
                .is_ok_and(|ours| ours.id() == entry.id && ours.filemode() == entry.mode as i32);
            if !same {
                carried.missing.push(path.clone());
                merged_ids.insert(path.clone(), Some(entry.id));
            }
            seen.insert(path);
        }
        // And every file of `tip`'s the merge no longer has: deleted there.
        let _ = ours.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if matches!(
                entry.kind(),
                Some(git2::ObjectType::Blob) | Some(git2::ObjectType::Commit)
            ) {
                let path = format!("{dir}{}", entry.name().unwrap_or_default());
                if !seen.contains(&path) && !conflicted.contains(&path) {
                    carried.missing.push(path);
                }
            }
            git2::TreeWalkResult::Ok
        });
        carried.missing.sort();
        carried.differing = conflicted.into_iter().collect();
        for path in carried.missing.iter().take(LOSS_DETAILS) {
            let theirs = merged_ids.get(path).copied().flatten();
            carried.details.push(Loss {
                detail: self.missing_detail(path, &ours, theirs),
                path: path.clone(),
                differing: false,
                changed_by: Vec::new(),
            });
        }
        differing_details.sort_by(|a, b| a.path.cmp(&b.path));
        let room = LOSS_DETAILS.saturating_sub(carried.details.len());
        carried
            .details
            .extend(differing_details.into_iter().take(room));
        carried
    }

    /// The diff merging the remote's change into `ours` would apply to
    /// `path`, whose merged content is `theirs` (`None`: deleted there).
    fn missing_detail(&self, path: &str, ours: &git2::Tree, theirs: Option<Oid>) -> String {
        let old = ours.get_path(Path::new(path)).ok().map(|entry| entry.id());
        self.blob_diff(path, old, theirs)
    }

    /// The unified diff from blob `old` to blob `new` of `path` (`None`:
    /// no file), from its first hunk, capped.
    fn blob_diff(&self, path: &str, old: Option<Oid>, new: Option<Oid>) -> String {
        let blob = |oid: Oid| self.repo.find_blob(oid).ok();
        let old = old.and_then(blob);
        let new = new.and_then(blob);
        let (old, new) = (
            old.as_ref().map_or(&[][..], |b| b.content()),
            new.as_ref().map_or(&[][..], |b| b.content()),
        );
        if old.contains(&0) || new.contains(&0) {
            return "A binary file: its content differs.".to_string();
        }
        let mut options = git2::DiffOptions::new();
        options.context_lines(3);
        let patch = git2::Patch::from_buffers(
            old,
            Some(Path::new(path)),
            new,
            Some(Path::new(path)),
            Some(&mut options),
        );
        let text = patch
            .and_then(|mut patch| patch.to_buf())
            .map(|buf| String::from_utf8_lossy(&buf).to_string())
            .unwrap_or_default();
        // From the first hunk: the headers repeat the path the row names.
        let body = match text.find("\n@@") {
            Some(at) => &text[at + 1..],
            None => text.as_str(),
        };
        capped_lines(body)
    }

    /// The conflicting regions of a file both sides changed, as git marks
    /// them, two lines either side, "this branch" against "there".
    fn conflict_detail(&self, conflict: &git2::IndexConflict) -> String {
        let (Some(ours), Some(theirs)) = (&conflict.our, &conflict.their) else {
            return match (&conflict.our, &conflict.their) {
                (None, _) => "Deleted in this branch, changed there.".to_string(),
                _ => "Changed in this branch, deleted there.".to_string(),
            };
        };
        let mut options = git2::MergeFileOptions::new();
        options.our_label("this branch").their_label("there");
        let merged = match &conflict.ancestor {
            Some(ancestor) => {
                self.repo
                    .merge_file_from_index(ancestor, ours, theirs, Some(&mut options))
            }
            // Made on both sides, with nothing to merge from: this branch's
            // version against theirs.
            None => {
                let path = String::from_utf8_lossy(&ours.path).to_string();
                return format!(
                    "Made on both sides; this branch's version against theirs:\n{}",
                    self.blob_diff(&path, Some(ours.id), Some(theirs.id))
                );
            }
        };
        let Ok(merged) = merged else {
            return "Changed differently here and there.".to_string();
        };
        let text = String::from_utf8_lossy(merged.content()).to_string();
        let lines: Vec<&str> = text.lines().collect();
        let mut keep = vec![false; lines.len()];
        let mut inside = false;
        for (at, line) in lines.iter().enumerate() {
            if line.starts_with("<<<<<<<") {
                inside = true;
            }
            if inside {
                let end = (at + 3).min(lines.len());
                for near in &mut keep[at.saturating_sub(2)..end] {
                    *near = true;
                }
            }
            if line.starts_with(">>>>>>>") {
                inside = false;
            }
        }
        let mut out = String::new();
        let mut gap = false;
        for (line, keep) in lines.iter().zip(keep) {
            if keep {
                if gap && !out.is_empty() {
                    out.push_str("…\n");
                }
                out.push_str(line);
                out.push('\n');
                gap = false;
            } else {
                gap = true;
            }
        }
        if out.is_empty() {
            return "Changed differently here and there.".to_string();
        }
        capped_lines(&out)
    }

    /// The change a commit makes, to be found again after a rebase: its
    /// [`Self::patch_id`], or for a commit that changes nothing its message
    /// and author, which a rebase keeps (`--allow-empty`'s markers). `None`
    /// for a merge. Kept by oid, which fixes a commit's content: a branch
    /// far behind its base compares the same thousand commits at every
    /// refresh.
    fn change_of(&self, oid: Oid) -> Option<Change> {
        thread_local! {
            static CHANGES: std::cell::RefCell<std::collections::HashMap<Oid, Option<Change>>> =
                std::cell::RefCell::new(std::collections::HashMap::new());
        }
        if let Some(known) = CHANGES.with(|cache| cache.borrow().get(&oid).cloned()) {
            return known;
        }
        let change = match self.patch_id(oid) {
            Some(id) => Some(Change::Patch(id)),
            None => self.repo.find_commit(oid).ok().and_then(|commit| {
                (commit.parent_count() <= 1).then(|| {
                    Change::Empty(
                        commit.message_bytes().to_vec(),
                        commit.author().email_bytes().to_vec(),
                    )
                })
            }),
        };
        CHANGES.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 50_000 {
                cache.clear();
            }
            cache.insert(oid, change.clone());
        });
        change
    }

    /// The change a commit makes, as git's patch-id names it: the same for
    /// two commits whose diffs are the same, whatever their parents,
    /// messages, or dates. `None` for a merge, and for a commit that changes
    /// nothing, which has no change to be the same as another's.
    fn patch_id(&self, oid: Oid) -> Option<Oid> {
        let commit = self.repo.find_commit(oid).ok()?;
        let parent = match commit.parent_count() {
            0 => None,
            1 => Some(commit.parent(0).ok()?.tree().ok()?),
            _ => return None,
        };
        let diff = self
            .repo
            .diff_tree_to_tree(parent.as_ref(), Some(&commit.tree().ok()?), None)
            .ok()?;
        if diff.deltas().len() == 0 {
            return None;
        }
        diff.patchid(None).ok()
    }

    /// Fetch every configured remote: the base and a named push remote
    /// both, which in a fork's setup are two. The fetch reaches into the
    /// submodules whose commits it brings, so it carries the rewrites that
    /// fetch those on this repository's host by its scheme
    /// ([`crate::transport`]).
    pub fn fetch_all_command(&self) -> (String, Vec<String>) {
        let mut args = self.align_args();
        args.extend(["fetch", "--all", "--prune"].map(String::from));
        self.git_command_owned(args)
    }

    /// For a push target that is a URL: the ask for its tip there, whose
    /// output goes to [`record_remote_tip`]. `None` for a named remote,
    /// whose fetch keeps a tracking ref instead.
    pub fn ask_push_tip_command(&self, branch: &str) -> Option<(String, Vec<String>, PushTarget)> {
        let target = self.push_target(branch);
        if target.named {
            return None;
        }
        let (program, args) = self.git_command_owned(vec![
            "ls-remote".into(),
            target.remote.clone(),
            format!("refs/heads/{}", target.branch),
        ]);
        Some((program, args, target))
    }

    /// The fetch that brings a URL push target's tip here when it is not
    /// yet, so it can be counted against. Objects only: no ref is written,
    /// and no FETCH_HEAD.
    pub fn fetch_push_tip_command(&self, target: &PushTarget) -> Option<(String, Vec<String>)> {
        let tip = remote_tip(&target.remote, &target.branch)??;
        if self.repo.find_commit(tip).is_ok() {
            return None;
        }
        Some(self.git_command_owned(vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            target.remote.clone(),
            format!("refs/heads/{}", target.branch),
        ]))
    }

    /// How `branch` — any local branch, not only the one checked out here —
    /// relates to where it pushes and to what it is rebased onto.
    ///
    /// `ahead` is against the push target: commits it does not have. With
    /// no tip known there — a branch it does not have, or a URL not yet
    /// asked — it is what no remote's refs reach. `behind` is against the
    /// base when there is one, and the push target otherwise.
    pub fn sync_status_of(&self, branch: &str) -> Result<SyncStatus> {
        let Ok(local) = self.repo.find_branch(branch, BranchType::Local) else {
            return Ok(SyncStatus::no_upstream());
        };
        let Some(tip) = local.get().target() else {
            return Ok(SyncStatus::no_upstream());
        };
        let has_upstream = self
            .config_string(&format!("branch.{branch}.remote"))
            .is_some();
        if !has_upstream && self.remotes().unwrap_or_default().is_empty() {
            return Ok(SyncStatus::no_upstream());
        }
        let target = self.push_target(branch);
        // The push target's tip: a tracking ref, or what the URL said —
        // `None` there for a URL not asked yet, `Some(None)` for a branch
        // it does not have.
        let observed = (!target.named)
            .then(|| remote_tip(&target.remote, &target.branch))
            .flatten();
        let there = match target.tracking_ref() {
            Some(name) => self
                .repo
                .find_reference(&name)
                .ok()
                .and_then(|r| r.target()),
            None => observed.flatten(),
        };
        // A tip named but not here — a URL whose fetch has not landed —
        // cannot be counted against.
        let there = there.filter(|oid| self.repo.find_commit(*oid).is_ok());
        let base = self.base_ref_of(branch);
        let base_tip = base
            .as_deref()
            .and_then(|name| self.repo.find_reference(name).ok())
            .and_then(|r| r.target());

        let mut status = SyncStatus {
            upstream: Some(target.display()),
            base: base.as_deref().map(short_ref),
            push_by_url: !target.named,
            ..SyncStatus::no_upstream()
        };
        match there {
            Some(there) => {
                let (ahead, behind_target) = self.repo.graph_ahead_behind(tip, there)?;
                status.ahead = ahead;
                status.behind = behind_target;
                status.own_behind = behind_target;
                if behind_target > 0 {
                    status.replaces = Some((there.to_string(), behind_target));
                    // The push target's commits are this branch's own,
                    // rewritten here: nothing to catch up with, and the
                    // push is what replaces them.
                    if ahead > 0 {
                        status.rewritten = self.rewrite_dropping(branch, tip, there);
                        if status.rewritten.is_some() {
                            status.own_behind = 0;
                            if base.is_none() {
                                status.behind = 0;
                            }
                        }
                    }
                }
            }
            None => {
                let mut walk = self.repo.revwalk()?;
                walk.push(tip)?;
                // What ANY remote's refs reach is hidden, not only the push
                // remote's: a fork shares its parent's history, and a URL
                // push remote has no tracking refs at all.
                let _ = walk.hide_glob("refs/remotes/*");
                status.ahead = walk.filter(Result::is_ok).count();
                // No tip to count against. A named remote without the
                // branch, or a URL that said it has none, is a branch the
                // push creates; a URL not asked yet, or whose tip has not
                // been fetched, is unknown, and the count is a ceiling.
                if target.named || observed == Some(None) {
                    status.new_branch = true;
                } else {
                    status.push_unknown = true;
                }
            }
        }
        if let Some(base_tip) = base_tip {
            status.behind = self.repo.graph_ahead_behind(tip, base_tip)?.1;
        }
        // The pull request's own count: what the push sends that the
        // canonical branch does not already have — a branch just rebased
        // onto main sends main's new commits too, and they are not its own.
        status.canonical = self.canonical_of(branch);
        status.on_canonical = self.pushes_to_canonical(branch);
        // ↑'s commits, walked as `ahead` was counted — less the canonical
        // branch's, when there is one, which makes the count `own_ahead`.
        let canonical_tip = status
            .canonical
            .as_ref()
            .and_then(|canonical| self.repo.find_reference(&canonical.full).ok())
            .and_then(|r| r.target());
        let mut walk = self.repo.revwalk()?;
        walk.push(tip)?;
        if let Some(there) = there {
            walk.hide(there)?;
        } else {
            let _ = walk.hide_glob("refs/remotes/*");
        }
        if let Some(canonical_tip) = canonical_tip {
            walk.hide(canonical_tip)?;
        }
        let sends: Vec<Oid> = walk.filter_map(Result::ok).collect();
        status.own_ahead = if status.canonical.is_some() {
            sends.len()
        } else {
            status.ahead
        };
        status.sends = self.commit_list(sends, status.own_ahead);
        // ↓'s: what the push target has that the branch lacks, unless
        // those are the branch's own rewritten, which `own_behind` is then
        // 0 for.
        if let Some(there) = there.filter(|_| status.own_behind > 0) {
            status.incoming = self.commits_between(there, &[tip], status.own_behind);
        }
        // Whose the commits there are, whether a pull would bring them or a
        // push replace them.
        if let Some(there) = there.filter(|_| status.replaces.is_some()) {
            status.others_there = self.others_there(tip, there);
        }
        Ok(status)
    }

    /// The push of `branch` by name, as [`GitWorkspace::push_target`]
    /// resolves it: `<remote> refs/heads/<branch>:refs/heads/<name>`, with
    /// `--set-upstream` only for a branch that tracks nothing yet — never
    /// over an upstream that is the base (a fork's `origin/main`), which
    /// it would replace with the push target. `extra_refspecs` ride along
    /// after it.
    pub fn push_branch_command(
        &self,
        branch: &str,
        extra_refspecs: &[&str],
    ) -> (String, Vec<String>) {
        self.push_branch_command_leased(branch, None, extra_refspecs)
    }

    /// [`GitWorkspace::push_branch_command`] replacing what the push target
    /// has — a rebased branch's push — leased on `expected`, the tip the
    /// IDE last saw there: a push that finds anything else there refuses,
    /// so commits someone pushed since are never overwritten unseen.
    pub fn push_branch_command_leased(
        &self,
        branch: &str,
        expected: Option<&str>,
        extra_refspecs: &[&str],
    ) -> (String, Vec<String>) {
        let target = self.push_target(branch);
        // The submodule commits the branch records are pushed first, each
        // on its own branch ([`GitWorkspace::submodule_pushes`]); this one
        // only checks they are there. git's `on-demand` hands the parent's
        // refspec to each submodule's push, and a submodule without a
        // branch of that name refused the whole push (2026-10-20:
        // "src refspec 'refs/heads/intel-2026-10' must name a ref").
        let mut args = self.align_args();
        args.extend(["push", crate::PUSH_SUBMODULES_CHECK].map(String::from));
        if self
            .config_string(&format!("branch.{branch}.merge"))
            .is_none()
        {
            args.push("--set-upstream".to_string());
        }
        if let Some(expected) = expected {
            args.push(format!(
                "--force-with-lease=refs/heads/{}:{expected}",
                target.branch
            ));
        }
        args.push(target.remote.clone());
        args.push(format!("refs/heads/{branch}:refs/heads/{}", target.branch));
        args.extend(extra_refspecs.iter().map(|s| (*s).to_string()));
        self.git_command_owned(args)
    }
}

/// A submodule's push that the parent's push needs first: its commit the
/// parent records, on the submodule's branch that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmodulePush {
    /// The submodule's path in the parent.
    pub path: PathBuf,
    /// The branch pushed.
    pub branch: String,
    /// Where it goes, for the keys and the askpass.
    pub url: Option<String>,
    /// The commits it sends that no remote-tracking ref of the submodule
    /// has — what the Push card says it takes along.
    pub commits: usize,
    pub program: String,
    pub args: Vec<String>,
}

impl GitWorkspace {
    /// The pushes of submodule commits that `branch` records and no
    /// remote-tracking ref of the submodule has: each on the submodule's
    /// branch that holds it — the one checked out when it does, else any —
    /// to the same name on its push target, its upstream set when it has
    /// none. An error names a submodule whose commit no branch holds, which
    /// nothing can push: the parent is not pushed pointing at a commit
    /// only this machine has (David, 2026-10-06). A submodule not cloned
    /// here is passed over; the parent's `--recurse-submodules=check`
    /// refuses then, in git's words.
    pub fn submodule_pushes(
        &self,
        branch: &str,
    ) -> std::result::Result<Vec<SubmodulePush>, String> {
        let Ok(tip) = self.repo.revparse_single(&format!("refs/heads/{branch}")) else {
            return Ok(Vec::new());
        };
        let Ok(tree) = tip.peel_to_tree() else {
            return Ok(Vec::new());
        };
        let Ok(submodules) = self.repo.submodules() else {
            return Ok(Vec::new());
        };
        let mut pushes = Vec::new();
        for submodule in submodules {
            let path = submodule.path().to_path_buf();
            let Ok(entry) = tree.get_path(&path) else {
                continue;
            };
            if entry.kind() != Some(git2::ObjectType::Commit) {
                continue;
            }
            let recorded = entry.id();
            let Ok(sub) = submodule.open() else {
                continue;
            };
            let holds = |tip: Option<Oid>| {
                tip.is_some_and(|tip| {
                    tip == recorded || sub.graph_descendant_of(tip, recorded).unwrap_or(false)
                })
            };
            let remote_has = sub
                .references_glob("refs/remotes/*")
                .map(|refs| refs.flatten().any(|r| holds(r.target())))
                .unwrap_or(false);
            if remote_has {
                continue;
            }
            let head = sub
                .head()
                .ok()
                .filter(|head| head.is_branch())
                .and_then(|head| Some((head.shorthand()?.to_string(), head.target())));
            let chosen = match head {
                Some((name, tip)) if holds(tip) => Some(name),
                _ => sub
                    .branches(Some(git2::BranchType::Local))
                    .ok()
                    .and_then(|branches| {
                        branches.flatten().find_map(|(b, _)| {
                            holds(b.get().target())
                                .then(|| b.name().ok().flatten().map(str::to_string))
                                .flatten()
                        })
                    }),
            };
            let Some(name) = chosen else {
                return Err(format!(
                    "the submodule {} records {}, which is on none of its branches: check out a \
                     branch at it there, then push again",
                    path.display(),
                    &recorded.to_string()[..7]
                ));
            };
            let Some(workdir) = sub.workdir() else {
                continue;
            };
            let Some(within) = GitWorkspace::discover(workdir) else {
                continue;
            };
            let target = within.push_target(&name);
            // On the parent's host, in the parent's scheme, as its fetch.
            let mut args = self.align_args();
            args.push("push".to_string());
            if within
                .config_string(&format!("branch.{name}.merge"))
                .is_none()
            {
                args.push("--set-upstream".to_string());
            }
            args.push(target.remote.clone());
            args.push(format!("refs/heads/{name}:refs/heads/{}", target.branch));
            let (program, args) = within.git_command_owned(args);
            let commits = sub
                .revwalk()
                .ok()
                .and_then(|mut walk| {
                    walk.push(recorded).ok()?;
                    let _ = walk.hide_glob("refs/remotes/*");
                    Some(walk.take(1000).count())
                })
                .unwrap_or(0);
            pushes.push(SubmodulePush {
                url: within.push_url_of(&name),
                path,
                branch: name,
                commits,
                program,
                args,
            });
        }
        Ok(pushes)
    }

    /// The submodules whose recorded commit `target`'s new commits move:
    /// what a pull of them changes beyond the branch's own files, read from
    /// the merge base with `branch` to `target`. Both are full ref names.
    pub fn submodule_moves(&self, branch: &str, target: &str) -> Vec<PathBuf> {
        let tip = |name: &str| {
            self.repo
                .find_reference(name)
                .ok()
                .and_then(|r| r.peel_to_commit().ok())
        };
        let (Some(mine), Some(theirs)) = (tip(branch), tip(target)) else {
            return Vec::new();
        };
        let Ok(base) = self.repo.merge_base(mine.id(), theirs.id()) else {
            return Vec::new();
        };
        if base == theirs.id() {
            return Vec::new();
        }
        let (Ok(base), Ok(theirs)) = (
            self.repo.find_commit(base).and_then(|c| c.tree()),
            theirs.tree(),
        ) else {
            return Vec::new();
        };
        let Ok(diff) = self
            .repo
            .diff_tree_to_tree(Some(&base), Some(&theirs), None)
        else {
            return Vec::new();
        };
        diff.deltas()
            .filter(|delta| delta.new_file().mode() == git2::FileMode::Commit)
            .filter_map(|delta| delta.new_file().path().map(Path::to_path_buf))
            .collect()
    }
}

/// Whether a config key is one [`GitWorkspace::tracking_config`] gives:
/// `remote.<name>.url` or `.fetch`, or `branch.<name>.remote`, `.merge`,
/// or `.pushremote`, as git names them (lower-cased first and last parts).
pub fn is_tracking_key(key: &str) -> bool {
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let Some((name, field)) = rest.rsplit_once('.') else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    match section {
        "remote" => matches!(field, "url" | "fetch"),
        "branch" => matches!(field, "remote" | "merge" | "pushremote"),
        _ => false,
    }
}

/// `url` without userinfo that can be a secret: `https://user:token@host/x`
/// is `https://host/x`. An ssh URL's user (`ssh://git@host`, `git@host:x`)
/// is a login name, not a credential, and stays.
pub fn without_secret(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    if matches!(scheme, "ssh" | "git+ssh" | "ssh+git") {
        return url.to_string();
    }
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{path}"),
        None => url.to_string(),
    }
}

/// What a commit changes, for matching it across a rewrite
/// ([`GitWorkspace::change_of`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Change {
    Patch(Oid),
    /// A commit that changes nothing: its message and author's email.
    Empty(Vec<u8>, Vec<u8>),
}

/// `refs/remotes/origin/main` → `origin/main`.
fn short_ref(name: &str) -> String {
    name.strip_prefix("refs/remotes/")
        .or_else(|| name.strip_prefix("refs/heads/"))
        .unwrap_or(name)
        .to_string()
}

/// Files whose content `theirs` has and `ours` does not have alike, by
/// path, sorted: what a push of `ours` over `theirs` replaces, where no
/// merge could say more.
fn differing_files(repo: &git2::Repository, ours: &git2::Tree, theirs: &git2::Tree) -> Vec<String> {
    let mut paths = std::collections::BTreeSet::new();
    if let Ok(diff) = repo.diff_tree_to_tree(Some(ours), Some(theirs), None) {
        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                paths.insert(path.display().to_string());
            }
        }
    }
    paths.into_iter().collect()
}

/// `text` cut to [`LOSS_LINES`] lines, saying how many more there were.
fn capped_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= LOSS_LINES {
        return text.trim_end().to_string();
    }
    format!(
        "{}\n… {} more lines",
        lines[..LOSS_LINES].join("\n"),
        lines.len() - LOSS_LINES
    )
}
