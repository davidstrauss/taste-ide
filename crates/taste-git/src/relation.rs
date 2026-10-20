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
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use git2::{BranchType, Oid};

use crate::{GitWorkspace, SyncStatus};

/// The most commits either side of a divergence is walked for to compare
/// them change by change ([`GitWorkspace::diverged`]).
const DIVERGENCE_LIMIT: usize = 1000;

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
    /// replaces them; `dropped` of them make a change the branch does not.
    Rewritten {
        mine: usize,
        theirs: usize,
        dropped: usize,
    },
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
        let (new, own) = match self.diverged(tip, there) {
            Some((theirs, mine)) => (
                self.unmatched(&theirs, &mine),
                self.unmatched(&mine, &theirs),
            ),
            // Too far apart to compare change by change: by commit.
            None => {
                let (own, new) = self.repo.graph_ahead_behind(tip, there).ok()?;
                (new, own)
            }
        };
        Some(Canonical {
            name: short_ref(&full),
            full,
            new,
            own,
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
                Some(dropped) => PullPlan::Rewritten {
                    mine,
                    theirs,
                    dropped,
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
        let theirs: std::collections::HashSet<Change> = against
            .iter()
            .filter_map(|oid| self.change_of(*oid))
            .collect();
        commits
            .iter()
            .filter(|oid| !self.is_merge(**oid))
            .filter(|oid| {
                self.change_of(**oid)
                    .is_none_or(|key| !theirs.contains(&key))
            })
            .count()
    }

    fn is_merge(&self, oid: Oid) -> bool {
        self.repo
            .find_commit(oid)
            .is_ok_and(|commit| commit.parent_count() > 1)
    }

    /// Whether the commits `there` has that `tip` lacks are `branch`'s own
    /// from before it was rewritten here, and if so how many of them make
    /// a change `tip`'s own commits do not — the ones a push replacing
    /// them drops. Its own when either holds:
    ///
    /// - every one of them makes the same change as a commit `tip` has
    ///   that `there` lacks (git's patch-id, as `git cherry` compares) —
    ///   a rebase, a reword, an amend that kept the change; merges are
    ///   passed over, having no change of their own to compare;
    /// - `there` is a commit `branch` itself has pointed at (its reflog):
    ///   everything there came through this branch, whatever was done to
    ///   it since.
    ///
    /// `None` when neither holds: someone else's commits, which are for a
    /// rebase to take. A rewrite the agent made in a checkout in a VM
    /// reaches the folder's reflog as the IDE moving the branch, so it
    /// reads the same here.
    pub fn rewrite_dropping(&self, branch: &str, tip: Oid, there: Oid) -> Option<usize> {
        let (theirs, mine) = self.diverged(tip, there)?;
        if theirs.is_empty() || mine.is_empty() {
            return None;
        }
        let held = self
            .repo
            .reflog(&format!("refs/heads/{branch}"))
            .ok()
            .is_some_and(|log| log.iter().any(|entry| entry.id_new() == there));
        // A merge there is someone's — a host's "Update branch", a
        // maintainer's — and has no change of its own to find here, so
        // only the branch having pointed at it makes it this branch's
        // (review, 2026-10-20: a merge alone read as "nothing is lost").
        if !held && theirs.iter().any(|oid| self.is_merge(*oid)) {
            return None;
        }
        let dropped = self.unmatched(&theirs, &mine);
        (dropped == 0 || held).then_some(dropped)
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
        status.own_ahead = status.ahead;
        if let Some(canonical) = &status.canonical {
            let canonical_tip = self
                .repo
                .find_reference(&canonical.full)
                .ok()
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
            status.own_ahead = walk.filter(Result::is_ok).count();
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
        // The submodule commits the branch records go first, as
        // [`GitWorkspace::push_command`]'s do, and on this repository's
        // host in its scheme ([`crate::transport`]).
        let mut args = self.align_args();
        args.extend(["push", crate::PUSH_SUBMODULES].map(String::from));
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
