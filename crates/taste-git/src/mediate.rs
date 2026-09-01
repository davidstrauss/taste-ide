//! Mediated publish: moving refs between two local repositories, host-side.
//!
//! The invariants this module exists to hold, all of them load-bearing (see
//! docs/ENVIRONMENTS.md, "Git topology: mediated publish"):
//!
//! - **Every inter-repo flow is host-side, IDE-run, between local paths.**
//!   No container ever writes git it does not own, and nothing here talks to
//!   a real remote. The only thing in this crate that reaches GitHub is
//!   [`GitWorkspace::push_command`], which the *user* triggers.
//! - **libgit2, never a `git` subprocess.** libgit2 runs no hooks, so
//!   fetching from an untrusted repository — which every repository here is —
//!   executes none of its code. A `git fetch` against the same paths would
//!   run the other side's hooks; that is precisely the host-boundary crossing
//!   the design refuses. Shelling out from this module is a design change.
//! - **The main checkout is the hub.** env → hub is publish
//!   ([`GitWorkspace::publish_from`], called on the hub); hub → env is update
//!   ([`GitWorkspace::update_refs_from`], called on the env clone). There is
//!   no env→env direction, mediated or otherwise.
//! - **Neither direction touches a working tree, an index, or HEAD.**
//!   Publish refuses to move the hub's checked-out branch; update refuses any
//!   refspec that would land under `refs/heads/`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use git2::Oid;

use crate::GitWorkspace;

/// The hub→env refspec set the orchestrator integration flow depends on.
///
/// `agents/<env>` refs are ordinary branches in the hub, so this one mapping
/// carries both the user's branches and every environment's branch of record
/// down into the env clone as remote-tracking refs — a Phase 3 requirement,
/// not an orchestrator afterthought. `origin` is the name
/// [`crate::clone_local`] already gave the hub in every env clone.
pub const HUB_UPDATE_REFSPECS: &[&str] = &["+refs/heads/*:refs/remotes/origin/*"];

/// Whether the caller has already warned the user about a clobber.
///
/// Publish is fast-forward by default and *reports* divergence instead of
/// destroying it; [`PublishMode::Force`] is what the UI passes after the
/// human has seen what would be lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    FastForward,
    Force,
}

/// What a publish did to the destination ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStatus {
    /// The ref did not exist; it does now.
    Created,
    /// The ref existed and the new tip descends from it.
    FastForward,
    /// The ref already pointed at this tip. Nothing was written.
    Unchanged,
    /// The new tip does not descend from the old one, and
    /// [`PublishMode::FastForward`] was in force: **the ref was not moved.**
    Diverged,
    /// Divergence overwritten because the caller passed
    /// [`PublishMode::Force`].
    Forced,
}

/// The result of one publish, in enough detail for the UI to say what
/// happened and for the caller to offer a force retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    /// Full destination ref name in the hub, e.g.
    /// `refs/heads/agents/<env>/<topic>`.
    pub dest_ref: String,
    pub status: PublishStatus,
    /// Where the destination ref pointed before, if it existed.
    pub old: Option<Oid>,
    /// The source branch tip. On [`PublishStatus::Diverged`] this is what
    /// *would* have been written, not what the ref points at.
    pub new: Oid,
}

impl PublishOutcome {
    /// Whether the destination ref actually moved.
    pub fn updated(&self) -> bool {
        matches!(
            self.status,
            PublishStatus::Created | PublishStatus::FastForward | PublishStatus::Forced
        )
    }

    /// Whether the caller should offer a force retry (after warning).
    pub fn needs_force(&self) -> bool {
        self.status == PublishStatus::Diverged
    }
}

/// One remote-tracking ref an update moved. Refs that were already current
/// do not appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name in *this* repository, e.g. `refs/remotes/origin/main`.
    pub name: String,
    /// `None` when the ref was created by this update.
    pub old: Option<Oid>,
    /// The zero oid when the ref was pruned away.
    pub new: Oid,
}

impl RefUpdate {
    pub fn created(&self) -> bool {
        self.old.is_none() && !self.new.is_zero()
    }

    pub fn pruned(&self) -> bool {
        self.new.is_zero()
    }
}

/// Everything the staged half of a publish needs: what to fetch, where to
/// park it, and the decision inputs for moving the destination ref.
struct PublishPlan<'a> {
    source_path: &'a Path,
    source_ref: &'a str,
    /// Private ref the fetched tip lands on before any decision is made.
    staging: &'a str,
    dest_ref: &'a str,
    /// Destination tip as read before the fetch.
    old: Option<Oid>,
    /// Source tip as read before the fetch; a mismatch afterwards means the
    /// agent committed mid-publish.
    source_tip: Oid,
    mode: PublishMode,
}

/// Staging refs are per-call so two publishes running on separate blocking
/// threads cannot collide on one name.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

fn staging_ref() -> String {
    format!(
        "refs/taste/staging/{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Accept either a bare branch name or a full ref name.
fn as_branch_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    }
}

impl GitWorkspace {
    /// Publish: fetch exactly one branch from a local repository into this
    /// one (the hub) at `dest_ref`.
    ///
    /// The mechanism under the `publish` MCP tool — the agent never pushes
    /// anywhere; the IDE fetches from its clone. `source_repo` is a path on
    /// this host, `source_branch` is either `topic` or `refs/heads/topic`,
    /// and `dest_ref` is a full ref name.
    ///
    /// Callers publishing an *environment's* work want
    /// [`GitWorkspace::publish_env`], which derives the destination from the
    /// environment rather than taking one: the branch of record is policy,
    /// and policy with a parameter beside it is policy that gets bypassed.
    /// This stays public because the ref-moving is the reusable half.
    ///
    /// Semantics:
    /// - Missing destination ref → [`PublishStatus::Created`].
    /// - Same tip → [`PublishStatus::Unchanged`], nothing written.
    /// - New tip descends from the old → [`PublishStatus::FastForward`].
    /// - Otherwise → [`PublishStatus::Diverged`] **without moving the ref**,
    ///   unless [`PublishMode::Force`] was passed ([`PublishStatus::Forced`]).
    ///
    /// Errors (rather than outcomes) for: a source repo that will not open, a
    /// source branch that does not exist, an invalid or non-`refs/`
    /// destination, and a destination that is this repository's checked-out
    /// branch — moving that would leave the user's index and working tree
    /// describing a revert nobody asked for.
    pub fn publish_from(
        &self,
        source_repo: &Path,
        source_branch: &str,
        dest_ref: &str,
        mode: PublishMode,
    ) -> Result<PublishOutcome> {
        if !git2::Reference::is_valid_name(dest_ref) || !dest_ref.starts_with("refs/") {
            bail!("{dest_ref} is not a valid destination ref name");
        }
        if let Some(head) = self.head_ref_name() {
            if head == dest_ref {
                bail!("refusing to publish onto the checked-out branch {dest_ref}");
            }
        }

        let source_path = source_repo
            .canonicalize()
            .with_context(|| format!("resolving {}", source_repo.display()))?;
        let source = git2::Repository::open(&source_path)
            .with_context(|| format!("opening source repository {}", source_path.display()))?;
        let source_ref = as_branch_ref(source_branch);
        let source_tip = source
            .find_reference(&source_ref)
            .and_then(|r| r.peel_to_commit())
            .with_context(|| {
                format!(
                    "{source_ref} does not name a commit in {}",
                    source_path.display()
                )
            })?
            .id();

        let old = self.read_ref(dest_ref)?;
        if old == Some(source_tip) {
            // Nothing to fetch: the hub already has this tip under this name.
            return Ok(PublishOutcome {
                dest_ref: dest_ref.to_string(),
                status: PublishStatus::Unchanged,
                old,
                new: source_tip,
            });
        }

        // Fetch into a private staging ref first, so the destination only
        // ever moves after the fast-forward decision is made.
        let staging = staging_ref();
        let result = self.publish_staged(&PublishPlan {
            source_path: &source_path,
            source_ref: &source_ref,
            staging: &staging,
            dest_ref,
            old,
            source_tip,
            mode,
        });
        if let Ok(mut reference) = self.repo.find_reference(&staging) {
            let _ = reference.delete();
        }
        result
    }

    fn publish_staged(&self, plan: &PublishPlan<'_>) -> Result<PublishOutcome> {
        let PublishPlan {
            source_path,
            source_ref,
            staging,
            dest_ref,
            old,
            source_tip,
            mode,
        } = *plan;
        let refspec = format!("+{source_ref}:{staging}");
        // Anonymous: an env clone must never accumulate remote config
        // pointing at host paths its container cannot see.
        let mut remote = self
            .repo
            .remote_anonymous(&source_path.to_string_lossy())
            .with_context(|| format!("anonymous remote for {}", source_path.display()))?;
        let mut options = git2::FetchOptions::new();
        options.download_tags(git2::AutotagOption::None);
        remote
            .fetch(&[refspec.as_str()], Some(&mut options), None)
            .with_context(|| format!("fetching {source_ref} from {}", source_path.display()))?;

        let new = self
            .repo
            .find_reference(staging)
            .and_then(|r| r.peel_to_commit())
            .context("fetched tip did not land in the staging ref")?
            .id();
        if new != source_tip {
            bail!("source branch moved during publish ({source_tip} → {new})");
        }

        let message = format!("taste publish from {}", source_path.display());
        let status = match old {
            None => {
                self.repo
                    .reference(dest_ref, new, false, &message)
                    .with_context(|| format!("creating {dest_ref}"))?;
                PublishStatus::Created
            }
            Some(old_oid) if self.repo.graph_descendant_of(new, old_oid)? => {
                self.repo
                    .reference_matching(dest_ref, new, true, old_oid, &message)
                    .with_context(|| format!("advancing {dest_ref}"))?;
                PublishStatus::FastForward
            }
            Some(old_oid) => match mode {
                PublishMode::FastForward => PublishStatus::Diverged,
                PublishMode::Force => {
                    self.repo
                        .reference_matching(dest_ref, new, true, old_oid, &message)
                        .with_context(|| format!("force-updating {dest_ref}"))?;
                    PublishStatus::Forced
                }
            },
        };
        Ok(PublishOutcome {
            dest_ref: dest_ref.to_string(),
            status,
            old,
            new,
        })
    }

    /// Update: fetch refs from the hub into *this* repository (an environment
    /// clone) as remote-tracking refs.
    ///
    /// This is the `update_from_main` direction. Pass
    /// [`HUB_UPDATE_REFSPECS`] for the standard set; every refspec is checked
    /// to land outside `refs/heads/`, so nothing here can move the branch the
    /// agent has checked out or disturb its working tree. Stale tracking refs
    /// are pruned, so a branch deleted in the hub stops showing up here.
    ///
    /// Returns only the refs that actually moved.
    pub fn update_refs_from(
        &self,
        source_repo: &Path,
        refspecs: &[&str],
    ) -> Result<Vec<RefUpdate>> {
        for spec in refspecs {
            check_tracking_refspec(spec)?;
        }
        let source_path = source_repo
            .canonicalize()
            .with_context(|| format!("resolving {}", source_repo.display()))?;
        // Opening it proves it is a repository before we fetch from it.
        git2::Repository::open(&source_path)
            .with_context(|| format!("opening source repository {}", source_path.display()))?;

        let mut updates = Vec::new();
        {
            let mut remote = self
                .repo
                .remote_anonymous(&source_path.to_string_lossy())
                .with_context(|| format!("anonymous remote for {}", source_path.display()))?;
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.update_tips(|name, old, new| {
                updates.push(RefUpdate {
                    name: name.to_string(),
                    old: (!old.is_zero()).then_some(old),
                    new,
                });
                true
            });
            let mut options = git2::FetchOptions::new();
            options.remote_callbacks(callbacks);
            options.prune(git2::FetchPrune::On);
            options.download_tags(git2::AutotagOption::None);
            remote
                .fetch(refspecs, Some(&mut options), None)
                .with_context(|| format!("fetching from {}", source_path.display()))?;
        }
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(updates)
    }

    /// Configured remote names (read-only; nothing here creates remotes).
    pub fn remotes(&self) -> Result<Vec<String>> {
        Ok(self
            .repo
            .remotes()?
            .iter()
            .flatten()
            .map(str::to_string)
            .collect())
    }

    /// URL of a configured remote, if it has one.
    pub fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let remote = self.repo.find_remote(name)?;
        Ok(remote.url().map(str::to_string))
    }

    /// Full ref name HEAD points at, for the "don't move the checked-out
    /// branch" guards. `None` when HEAD is detached or unborn-and-unnamed.
    pub(crate) fn head_ref_name(&self) -> Option<String> {
        let head = self.repo.find_reference("HEAD").ok()?;
        // Symbolic even on an unborn branch, which is exactly the case a
        // publish onto refs/heads/main must still refuse.
        head.symbolic_target().map(str::to_string)
    }
}

/// A fetch refspec is safe here only if it writes somewhere that is not a
/// local branch: this repository's checked-out branch and working tree are
/// the agent's, and an update must never move them under it.
fn check_tracking_refspec(spec: &str) -> Result<()> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let Some((_, dst)) = spec.split_once(':') else {
        bail!("refspec {spec} has no destination");
    };
    if dst.is_empty() {
        bail!("refspec {spec} has an empty destination");
    }
    if dst == "HEAD" || dst.starts_with("refs/heads/") {
        bail!("refspec {spec} would write a local branch; updates land in tracking refs only");
    }
    if !dst.starts_with("refs/") {
        bail!("refspec {spec} destination must be a full ref name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clone_local;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn commit(repo: &git2::Repository, name: &str, content: &str) -> Oid {
        let root = repo.workdir().unwrap().to_path_buf();
        fs::write(root.join(name), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, name, &tree, &parent_refs)
            .unwrap()
    }

    /// A hub with one commit, plus an env clone of it.
    fn hub_and_clone() -> (tempfile::TempDir, GitWorkspace, tempfile::TempDir, PathBuf) {
        let hub_dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(hub_dir.path()).unwrap();
        commit(&repo, "base", "base\n");
        drop(repo);
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("env/repo");
        clone_local(hub_dir.path(), &clone_path).unwrap();
        let hub = GitWorkspace::discover(hub_dir.path()).unwrap();
        (hub_dir, hub, clone_dir, clone_path)
    }

    const AGENT_REF: &str = "refs/heads/agents/env-1/topic";

    #[test]
    fn publish_creates_then_is_unchanged_then_fast_forwards() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        let first = commit(&clone, "work", "one\n");
        let branch = clone.head().unwrap().shorthand().unwrap().to_string();

        let created = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();
        assert_eq!(created.status, PublishStatus::Created);
        assert_eq!(created.old, None);
        assert_eq!(created.new, first);
        assert!(created.updated());
        assert_eq!(hub.read_ref(AGENT_REF).unwrap(), Some(first));

        let again = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();
        assert_eq!(again.status, PublishStatus::Unchanged);
        assert!(!again.updated());

        let second = commit(&clone, "work", "two\n");
        let ff = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();
        assert_eq!(ff.status, PublishStatus::FastForward);
        assert_eq!(ff.old, Some(first));
        assert_eq!(ff.new, second);
        assert_eq!(hub.read_ref(AGENT_REF).unwrap(), Some(second));
    }

    #[test]
    fn publish_reports_divergence_and_force_overwrites() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        let branch = clone.head().unwrap().shorthand().unwrap().to_string();
        let first = commit(&clone, "work", "one\n");
        hub.publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();

        // Rewrite history in the clone: reset to base, commit differently.
        let base = clone.find_commit(first).unwrap().parent(0).unwrap();
        clone
            .reset(base.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        let rewritten = commit(&clone, "work", "rewritten\n");
        assert_ne!(rewritten, first);

        let diverged = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();
        assert_eq!(diverged.status, PublishStatus::Diverged);
        assert!(diverged.needs_force());
        assert!(!diverged.updated());
        assert_eq!(
            hub.read_ref(AGENT_REF).unwrap(),
            Some(first),
            "divergence must not clobber"
        );

        let forced = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::Force)
            .unwrap();
        assert_eq!(forced.status, PublishStatus::Forced);
        assert_eq!(forced.old, Some(first));
        assert_eq!(hub.read_ref(AGENT_REF).unwrap(), Some(rewritten));
    }

    #[test]
    fn publish_rejects_a_missing_source_branch() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let err = hub
            .publish_from(
                &clone_path,
                "no-such-topic",
                AGENT_REF,
                PublishMode::FastForward,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("refs/heads/no-such-topic"), "{err}");
        assert_eq!(hub.read_ref(AGENT_REF).unwrap(), None);
    }

    #[test]
    fn publish_rejects_bad_destinations() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        let branch = clone.head().unwrap().shorthand().unwrap().to_string();
        commit(&clone, "work", "one\n");

        for dest in ["agents/env/topic", "HEAD", "refs/heads/bad name"] {
            assert!(
                hub.publish_from(&clone_path, &branch, dest, PublishMode::FastForward)
                    .is_err(),
                "{dest} should be refused"
            );
        }

        // The hub's own checked-out branch is the one ref publish must never
        // move: the user's index and worktree describe it.
        let head = hub.head_ref_name().unwrap();
        let err = hub
            .publish_from(&clone_path, &branch, &head, PublishMode::Force)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checked-out"), "{err}");
    }

    /// Plant every hook `git` would run on a fetch between these two repos,
    /// each touching `marker`, plus the `uploadpack.packObjectsHook` config
    /// escape hatch. libgit2 must run none of them: that is the property the
    /// whole mediated-publish design rests on.
    fn plant_hooks(repo_dir: &Path, marker: &Path) {
        let hooks = repo_dir.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let script = format!("#!/bin/sh\ntouch {}\n", marker.display());
        for hook in [
            "pre-receive",
            "update",
            "post-receive",
            "post-update",
            "reference-transaction",
            "post-upload-pack",
            "pre-push",
        ] {
            let path = hooks.join(hook);
            fs::write(&path, &script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let repo = git2::Repository::open(repo_dir).unwrap();
        repo.config()
            .unwrap()
            .set_str(
                "uploadpack.packObjectsHook",
                &format!("touch {}", marker.display()),
            )
            .unwrap();
    }

    #[test]
    fn publish_runs_no_hooks_and_leaves_no_staging_refs() {
        let (hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let marker = hub_dir.path().join("hook-ran");
        // Both sides: the destination we write, and the untrusted source we
        // read from.
        plant_hooks(hub_dir.path(), &marker);
        plant_hooks(&clone_path, &marker);

        let clone = git2::Repository::open(&clone_path).unwrap();
        let branch = clone.head().unwrap().shorthand().unwrap().to_string();
        commit(&clone, "work", "one\n");
        let outcome = hub
            .publish_from(&clone_path, &branch, AGENT_REF, PublishMode::FastForward)
            .unwrap();
        assert_eq!(outcome.status, PublishStatus::Created);

        assert!(
            !marker.exists(),
            "libgit2 must not execute the destination repository's hooks"
        );
        let repo = git2::Repository::open(hub_dir.path()).unwrap();
        let staging: Vec<String> = repo
            .references()
            .unwrap()
            .flatten()
            .filter_map(|r| r.name().map(str::to_string))
            .filter(|n| n.starts_with("refs/taste/staging/"))
            .collect();
        assert!(staging.is_empty(), "staging refs left behind: {staging:?}");
        // No remote config accumulated either: publish uses anonymous remotes.
        assert_eq!(hub.remotes().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn update_brings_hub_branches_and_agent_refs_without_touching_the_worktree() {
        let (hub_dir, _hub, _clone_dir, clone_path) = hub_and_clone();
        let hub_repo = git2::Repository::open(hub_dir.path()).unwrap();
        let env = GitWorkspace::discover(&clone_path).unwrap();

        // The agent has uncommitted work and a checked-out branch.
        let dirty = clone_path.join("scratch.txt");
        fs::write(&dirty, "half-finished\n").unwrap();
        let env_head_before = env.read_ref(&env.head_ref_name().unwrap()).unwrap();

        // The hub gains a branch and a published agent branch (from a second
        // environment, as the orchestrator flow has it).
        let hub_tip = commit(&hub_repo, "hub-work", "hub\n");
        let hub_branch = hub_repo.head().unwrap().shorthand().unwrap().to_string();
        hub_repo
            .reference(AGENT_REF, hub_tip, false, "test")
            .unwrap();

        let updates = env
            .update_refs_from(hub_dir.path(), HUB_UPDATE_REFSPECS)
            .unwrap();
        let names: Vec<&str> = updates.iter().map(|u| u.name.as_str()).collect();
        assert!(
            names.contains(&"refs/remotes/origin/agents/env-1/topic"),
            "agents/* must come along: {names:?}"
        );
        assert!(
            names.contains(&format!("refs/remotes/origin/{hub_branch}").as_str()),
            "{names:?}"
        );
        assert_eq!(
            env.read_ref("refs/remotes/origin/agents/env-1/topic")
                .unwrap(),
            Some(hub_tip)
        );

        // Untouched: checked-out branch, working tree, index.
        assert_eq!(
            env.read_ref(&env.head_ref_name().unwrap()).unwrap(),
            env_head_before,
            "the env's own branch must not move"
        );
        assert_eq!(fs::read_to_string(&dirty).unwrap(), "half-finished\n");
        assert_eq!(
            fs::read_to_string(clone_path.join("base")).unwrap(),
            "base\n"
        );

        // Second update with nothing new reports nothing.
        assert!(env
            .update_refs_from(hub_dir.path(), HUB_UPDATE_REFSPECS)
            .unwrap()
            .is_empty());

        // A branch deleted in the hub is pruned from the tracking refs.
        hub_repo
            .find_reference(AGENT_REF)
            .unwrap()
            .delete()
            .unwrap();
        let pruned = env
            .update_refs_from(hub_dir.path(), HUB_UPDATE_REFSPECS)
            .unwrap();
        assert!(
            pruned
                .iter()
                .any(|u| u.pruned() && u.name == "refs/remotes/origin/agents/env-1/topic"),
            "{pruned:?}"
        );
        assert_eq!(
            env.read_ref("refs/remotes/origin/agents/env-1/topic")
                .unwrap(),
            None
        );
    }

    #[test]
    fn update_refuses_refspecs_that_would_write_local_branches() {
        let (hub_dir, _hub, _clone_dir, clone_path) = hub_and_clone();
        let env = GitWorkspace::discover(&clone_path).unwrap();
        for spec in [
            "+refs/heads/*:refs/heads/*",
            "refs/heads/main:HEAD",
            "refs/heads/main",
            "+refs/heads/*:origin/*",
        ] {
            assert!(
                env.update_refs_from(hub_dir.path(), &[spec]).is_err(),
                "{spec} should be refused"
            );
        }
    }

    #[test]
    fn update_runs_no_hooks_in_the_source_repository() {
        let (hub_dir, _hub, _clone_dir, clone_path) = hub_and_clone();
        let marker = hub_dir.path().join("hook-ran");
        plant_hooks(hub_dir.path(), &marker);
        plant_hooks(&clone_path, &marker);
        let hub_repo = git2::Repository::open(hub_dir.path()).unwrap();
        commit(&hub_repo, "hub-work", "hub\n");

        let env = GitWorkspace::discover(&clone_path).unwrap();
        env.update_refs_from(hub_dir.path(), HUB_UPDATE_REFSPECS)
            .unwrap();
        assert!(!marker.exists(), "no hook may run on either side");
    }
}
