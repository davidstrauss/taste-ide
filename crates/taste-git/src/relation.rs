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

    fn tracking_ref(&self) -> Option<String> {
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
    fn config_string(&self, key: &str) -> Option<String> {
        self.repo.config().ok()?.get_string(key).ok()
    }

    fn is_named_remote(&self, remote: &str) -> bool {
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

    /// Fetch every configured remote: the base and a named push remote
    /// both, which in a fork's setup are two.
    pub fn fetch_all_command(&self) -> (String, Vec<String>) {
        self.git_command(&["fetch", "--all", "--prune"])
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
            ..SyncStatus::no_upstream()
        };
        match there {
            Some(there) => {
                let (ahead, behind_target) = self.repo.graph_ahead_behind(tip, there)?;
                status.ahead = ahead;
                status.behind = behind_target;
                if behind_target > 0 {
                    status.replaces = Some((there.to_string(), behind_target));
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
        let mut args = vec!["push".to_string()];
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

/// `refs/remotes/origin/main` → `origin/main`.
fn short_ref(name: &str) -> String {
    name.strip_prefix("refs/remotes/")
        .or_else(|| name.strip_prefix("refs/heads/"))
        .unwrap_or(name)
        .to_string()
}
