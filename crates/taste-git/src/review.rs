//! The environment branch of record, and the one mergedness fact.
//!
//! **Strictly one branch per environment.** An environment is a clone, a
//! container, an agent session and a unit of review; giving it more than one
//! branch means the thing the user merges is no longer the thing they
//! destroy. So the branch name is *derived* from the environment id and
//! nothing chooses it — [`env_branch`] is the whole naming policy, and
//! publishing twice moves that one ref rather than growing a second.
//!
//! This replaces the `agents/<env>/<topic>` generation, in which one
//! environment could publish any number of topic branches and review was a
//! list of refs rather than a list of environments. Those branches are a
//! **dead generation**: nothing writes them, nothing migrates them, and
//! [`GitWorkspace::dead_generation_branches`] reports them so a user with
//! some left over is told rather than silently ignored (alpha rules — see
//! docs/ENVIRONMENTS.md).
//!
//! The second thing here is [`Mergedness`]: "is this branch's work already
//! in the merge target". It is one query with two callers that must never
//! disagree — the issue close gate and the review lifecycle — so it is one
//! function, not two implementations of `ahead == 0`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use git2::Oid;

use crate::mediate::{PublishMode, PublishOutcome};
use crate::refs::{BranchInfo, BranchRelation};
use crate::GitWorkspace;

/// Where environment branches of record live. One prefix, everywhere.
pub const ENV_BRANCH_PREFIX: &str = "agents/";

/// The branch of record for an environment: `agents/<env>`.
///
/// Deterministic and total — there is no per-publish naming decision to
/// make, which is exactly the property that lets the review lifecycle key
/// on the environment instead of on whatever an agent felt like calling its
/// work today.
pub fn env_branch(env: &str) -> String {
    format!("{ENV_BRANCH_PREFIX}{env}")
}

/// ...as a full ref name.
pub fn env_branch_ref(env: &str) -> String {
    format!("refs/heads/{}", env_branch(env))
}

/// The environment a branch of record belongs to, or `None` when the name
/// is not one.
///
/// A nested name (`agents/calm-1/topic`) answers `None` on purpose: that is
/// the dead generation, and calling it "calm-1's branch" would let a stale
/// ref masquerade as the one the review lifecycle tracks.
pub fn env_of_branch(branch: &str) -> Option<&str> {
    let rest = branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .strip_prefix(ENV_BRANCH_PREFIX)?;
    (!rest.is_empty() && !rest.contains('/')).then_some(rest)
}

/// Reject an environment id that could not be half of a ref name.
///
/// Environment ids are already validated where they are minted
/// (`taste_core::EnvironmentId`), but this crate takes strings and refuses
/// to build a ref name it has not looked at.
fn check_env(env: &str) -> Result<()> {
    if env.is_empty() {
        bail!("an environment branch needs an environment id");
    }
    if env.contains('/') {
        bail!(
            "environment id {env:?} contains '/', which would nest it under another env's branch"
        );
    }
    if !git2::Reference::is_valid_name(&env_branch_ref(env)) {
        bail!("environment id {env:?} does not make a valid branch name");
    }
    Ok(())
}

/// One branch checked against a merge target — the whole of "is this work
/// already in".
///
/// The single mergedness fact in the codebase. The issue close gate asks it
/// about a claiming environment's branch; the review lifecycle asks it
/// about the same branch to decide whether a flagged environment has landed.
/// Two answers to that question is one answer too many.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mergedness {
    /// The branch this is about, as a short name.
    pub branch: String,
    /// What was actually compared: the branch's tip, or the recorded tip
    /// when the branch itself is gone.
    pub checked: Option<Oid>,
    /// Commits the branch has that the target does not.
    pub ahead: usize,
    /// `ahead == 0` — everything on it is reachable from the target.
    ///
    /// Note what this does NOT assume: a target that is force-moved
    /// backwards stops containing the work, and this goes back to `false`.
    /// Mergedness is a query about the repository as it is now, never a
    /// latch.
    pub merged: bool,
    /// Why the answer is what it is, when that needs saying.
    pub note: Option<String>,
}

impl GitWorkspace {
    /// Is `branch`'s work already reachable from `target`?
    ///
    /// `recorded_tip` is the fallback for a branch that has been deleted —
    /// the honest workflow merges and then deletes, and a check that could
    /// not survive that would make every merged-and-tidied issue unclosable
    /// forever. When the branch exists it wins; the recorded tip is only
    /// consulted when there is nothing else to look at.
    pub fn mergedness(
        &self,
        branch: &str,
        recorded_tip: Option<Oid>,
        target: &str,
    ) -> Result<Mergedness> {
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        let exists = self
            .repo
            .find_branch(short, git2::BranchType::Local)
            .is_ok();
        let (rev, checked, note) = if exists {
            (
                Some(short.to_string()),
                self.read_ref(&as_head_ref(short))?,
                None,
            )
        } else if let Some(tip) = recorded_tip.filter(|oid| self.repo.find_commit(*oid).is_ok()) {
            (
                Some(tip.to_string()),
                Some(tip),
                Some(format!(
                    "the branch is gone; checked the tip it had when it was recorded ({})",
                    short_oid(tip)
                )),
            )
        } else {
            (
                None,
                None,
                Some(
                    "the branch is gone and the tip it had when it was recorded is no longer \
                     in the repository, so its mergedness cannot be verified"
                        .to_string(),
                ),
            )
        };

        let Some(rev) = rev else {
            return Ok(Mergedness {
                branch: short.to_string(),
                checked: None,
                ahead: 0,
                merged: false,
                note,
            });
        };
        let ahead = self
            .ahead_behind(&rev, target)
            .with_context(|| format!("comparing {short} with {target}"))?
            .0;
        Ok(Mergedness {
            branch: short.to_string(),
            checked,
            ahead,
            merged: ahead == 0,
            // A note explaining a fallback is noise once the answer is yes.
            note: note.filter(|_| ahead != 0),
        })
    }

    /// An environment's branch of record, checked against `target`. `None`
    /// when the environment has never published.
    pub fn env_mergedness(&self, env: &str, target: &str) -> Result<Option<Mergedness>> {
        check_env(env)?;
        let branch = env_branch(env);
        if self.read_ref(&env_branch_ref(env))?.is_none() {
            return Ok(None);
        }
        self.mergedness(&branch, None, target).map(Some)
    }
}

/// One environment's published work, as the review list renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvBranch {
    /// The environment id, from the branch name.
    pub env: String,
    pub branch: BranchInfo,
    pub relation: BranchRelation,
}

impl EnvBranch {
    /// Nothing on this branch the target does not already have.
    pub fn merged(&self) -> bool {
        self.relation.ahead == 0
    }
}

impl GitWorkspace {
    /// Every environment branch of record in this checkout, with its
    /// relation to `target` — the review list, as data.
    ///
    /// Nested `agents/<x>/<y>` names are not here: they are the dead
    /// generation, reported by [`GitWorkspace::dead_generation_branches`].
    pub fn env_branches(&self, target: &str) -> Result<Vec<EnvBranch>> {
        let mut out = Vec::new();
        for branch in self.branches_matching(ENV_BRANCH_PREFIX)? {
            let Some(env) = env_of_branch(&branch.name) else {
                continue;
            };
            let env = env.to_string();
            let relation = self
                .relation_to(&branch.name, target)
                .with_context(|| format!("comparing {} with {target}", branch.name))?;
            out.push(EnvBranch {
                env,
                branch,
                relation,
            });
        }
        out.sort_by(|a, b| a.env.cmp(&b.env));
        Ok(out)
    }

    /// Branches left over from the `agents/<env>/<topic>` generation.
    ///
    /// Alpha rules: nothing migrates these. They are reported so a user who
    /// has some is told what they are and can merge or delete them by hand,
    /// rather than finding refs that no view accounts for.
    pub fn dead_generation_branches(&self) -> Result<Vec<BranchInfo>> {
        Ok(self
            .branches_matching(ENV_BRANCH_PREFIX)?
            .into_iter()
            .filter(|b| env_of_branch(&b.name).is_none())
            .collect())
    }

    /// Publish: move `env`'s branch of record in this checkout (the hub) to
    /// what the environment's clone has.
    ///
    /// This is the whole of publish. There is no topic and no second
    /// branch: the destination is [`env_branch_ref`] and every publish from
    /// this environment moves that one ref. `source_branch` is the branch in
    /// the clone to take (`None` = whatever its HEAD is on), and the
    /// fast-forward/divergence/force semantics are
    /// [`GitWorkspace::publish_from`]'s unchanged — host-side libgit2, no
    /// hooks, no working tree touched on either side.
    pub fn publish_env(
        &self,
        source_repo: &Path,
        source_branch: Option<&str>,
        env: &str,
        mode: PublishMode,
    ) -> Result<PublishOutcome> {
        check_env(env)?;
        let dest = env_branch_ref(env);

        // A leftover `agents/<env>/<topic>` makes `agents/<env>` unwritable
        // — git cannot hold a ref and a directory of the same name — so say
        // that in words rather than letting libgit2 report a lock failure.
        let nested = format!("{}/", env_branch(env));
        let blocking: Vec<String> = self
            .branches_matching(&nested)?
            .into_iter()
            .map(|b| b.name)
            .collect();
        if !blocking.is_empty() {
            bail!(
                "cannot publish to {dest}: this checkout still holds {} from the \
                 agents/<env>/<topic> generation, and git cannot have both a branch and a \
                 directory of that name. Those branches are dead — merge or delete them \
                 (git branch -D {}) and publish again.",
                blocking.join(", "),
                blocking.join(" "),
            );
        }

        let branch = match source_branch {
            Some(branch) => branch.to_string(),
            None => head_branch_of(source_repo)?,
        };
        self.publish_from(source_repo, &branch, &dest, mode)
    }
}

/// The branch a repository has checked out, for a publish that was not told
/// which one to take.
fn head_branch_of(repo_path: &Path) -> Result<String> {
    let repo = git2::Repository::open(repo_path)
        .with_context(|| format!("opening {}", repo_path.display()))?;
    let head = repo
        .find_reference("HEAD")
        .context("this checkout has no HEAD")?;
    head.symbolic_target().map(str::to_string).with_context(|| {
        format!(
            "{} has a detached HEAD, so there is no branch to publish — say which \
                 branch, or check one out",
            repo_path.display()
        )
    })
}

fn as_head_ref(short: &str) -> String {
    if short.starts_with("refs/") {
        short.to_string()
    } else {
        format!("refs/heads/{short}")
    }
}

fn short_oid(oid: Oid) -> String {
    oid.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clone_local;
    use std::fs;
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

    #[test]
    fn the_branch_name_is_derived_and_reversible() {
        assert_eq!(env_branch("calm-1"), "agents/calm-1");
        assert_eq!(env_branch_ref("calm-1"), "refs/heads/agents/calm-1");
        assert_eq!(env_of_branch("agents/calm-1"), Some("calm-1"));
        assert_eq!(env_of_branch("refs/heads/agents/calm-1"), Some("calm-1"));
        // The dead generation is not an environment branch.
        assert_eq!(env_of_branch("agents/calm-1/topic"), None);
        assert_eq!(env_of_branch("agents/"), None);
        assert_eq!(env_of_branch("main"), None);
        assert!(check_env("has/slash").is_err());
        assert!(check_env("").is_err());
    }

    /// The property the whole redesign rests on: publishing twice moves ONE
    /// ref rather than leaving two behind.
    #[test]
    fn publishing_twice_moves_one_branch() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        let first = commit(&clone, "work", "one\n");

        let created = hub
            .publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();
        assert_eq!(created.dest_ref, "refs/heads/agents/calm-1");
        assert_eq!(created.new, first);
        assert_eq!(
            hub.read_ref("refs/heads/agents/calm-1").unwrap(),
            Some(first)
        );

        let second = commit(&clone, "work", "two\n");
        let moved = hub
            .publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();
        assert_eq!(moved.dest_ref, created.dest_ref, "the same ref, moved");
        assert_eq!(
            hub.read_ref("refs/heads/agents/calm-1").unwrap(),
            Some(second)
        );
        // ...and exactly one branch exists for this environment.
        let names: Vec<String> = hub
            .branches_matching(ENV_BRANCH_PREFIX)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(names, vec!["agents/calm-1".to_string()], "{names:?}");
    }

    /// libgit2 must still run no hooks: the publish path changed its
    /// destination, not its mechanism.
    #[test]
    fn publish_env_runs_no_hooks() {
        let (hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let marker = hub_dir.path().join("hook-ran");
        for repo_dir in [hub_dir.path(), clone_path.as_path()] {
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
                use std::os::unix::fs::PermissionsExt;
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
        let clone = git2::Repository::open(&clone_path).unwrap();
        commit(&clone, "work", "one\n");
        hub.publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();
        assert!(
            !marker.exists(),
            "publish must execute no repository's hooks"
        );
    }

    /// A leftover topic branch makes the env branch unwritable. Say so in
    /// words — this is the one place a user meets the dead generation.
    #[test]
    fn a_dead_generation_branch_is_reported_not_worked_around() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        commit(&clone, "work", "one\n");
        // A leftover from the previous generation, pointing at whatever the
        // hub already has.
        let base = hub
            .read_ref(&hub.head_ref_name().unwrap())
            .unwrap()
            .unwrap();
        hub.repo
            .reference("refs/heads/agents/calm-1/old", base, false, "legacy")
            .unwrap();

        let dead: Vec<String> = hub
            .dead_generation_branches()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(dead, vec!["agents/calm-1/old".to_string()]);
        // ...and it is not mistaken for an environment branch.
        assert!(hub.env_branches("HEAD").unwrap().is_empty());

        let refused = hub
            .publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("agents/calm-1/old"), "{refused}");
        assert!(refused.contains("dead"), "{refused}");
    }

    #[test]
    fn env_branches_pair_each_environment_with_its_relation() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        commit(&clone, "work", "one\n");
        hub.publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();
        commit(&clone, "more", "two\n");
        hub.publish_env(&clone_path, None, "spry-2", PublishMode::FastForward)
            .unwrap();

        let rows = hub.env_branches("HEAD").unwrap();
        let envs: Vec<&str> = rows.iter().map(|r| r.env.as_str()).collect();
        assert_eq!(envs, vec!["calm-1", "spry-2"]);
        assert_eq!(rows[0].relation.ahead, 1);
        assert_eq!(rows[1].relation.ahead, 2);
        assert!(!rows[0].merged());
    }

    /// Merged, not merged, and merged-then-un-merged by a target that moved
    /// backwards. The last one is why this is a query and not a latch.
    #[test]
    fn mergedness_is_a_query_about_the_repository_as_it_is_now() {
        let (hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let hub_repo = git2::Repository::open(hub_dir.path()).unwrap();
        let base = hub
            .read_ref("refs/heads/main")
            .unwrap()
            .or_else(|| hub.read_ref(&hub.head_ref_name().unwrap()).unwrap());
        let clone = git2::Repository::open(&clone_path).unwrap();
        let tip = commit(&clone, "work", "one\n");
        hub.publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();

        // A target that does not have it.
        let target = hub.head_ref_name().unwrap();
        let target_short = target.strip_prefix("refs/heads/").unwrap().to_string();
        let before = hub
            .mergedness("agents/calm-1", None, &target_short)
            .unwrap();
        assert!(!before.merged);
        assert_eq!(before.ahead, 1);
        assert_eq!(before.checked, Some(tip));

        // Merge it: now it is in.
        let merged = hub.merge_branch("agents/calm-1").unwrap();
        assert!(merged.advanced(), "{merged:?}");
        let after = hub
            .mergedness("agents/calm-1", None, &target_short)
            .unwrap();
        assert!(after.merged, "{after:?}");
        assert_eq!(after.note, None);

        // Force the target backwards, as a reset would: the same branch is
        // no longer in it, and the fact says so rather than staying true.
        let base = base.expect("the base commit");
        hub_repo
            .reference(&target, base, true, "reset the target")
            .unwrap();
        let hub = GitWorkspace::discover(hub_dir.path()).unwrap();
        let regressed = hub
            .mergedness("agents/calm-1", None, &target_short)
            .unwrap();
        assert!(!regressed.merged, "a force-moved target un-merges the work");
    }

    /// The branch is gone and the recorded tip is what is left: still
    /// answerable, and it says so.
    #[test]
    fn a_deleted_branch_falls_back_to_its_recorded_tip() {
        let (_hub_dir, hub, _clone_dir, clone_path) = hub_and_clone();
        let clone = git2::Repository::open(&clone_path).unwrap();
        let tip = commit(&clone, "work", "one\n");
        hub.publish_env(&clone_path, None, "calm-1", PublishMode::FastForward)
            .unwrap();
        let target = hub.head_ref_name().unwrap();
        let target_short = target.strip_prefix("refs/heads/").unwrap().to_string();
        hub.merge_branch("agents/calm-1").unwrap();
        hub.delete_ref("refs/heads/agents/calm-1").unwrap();

        let gone = hub
            .mergedness("agents/calm-1", None, &target_short)
            .unwrap();
        assert!(!gone.merged, "nothing to look at is not a yes");
        assert!(gone.note.unwrap().contains("cannot be verified"));

        let recorded = hub
            .mergedness("agents/calm-1", Some(tip), &target_short)
            .unwrap();
        assert!(recorded.merged, "the recorded tip is still reachable");
        assert_eq!(recorded.checked, Some(tip));

        // env_mergedness answers None for an environment that never
        // published — absent is not "not merged".
        assert_eq!(hub.env_mergedness("never-1", &target_short).unwrap(), None);
    }
}
