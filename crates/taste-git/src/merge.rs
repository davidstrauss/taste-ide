//! Merging a published branch into the one the user is standing on.
//!
//! The review inbox's one non-destructive bulk op. Two rules shape all of
//! it, both from docs/ENVIRONMENTS.md ("Review reuses the git-in-the-tree
//! UI") and ARCHITECTURE.md ("Conflicts are a first-class view"):
//!
//! - **A merge that would conflict changes nothing.** The merge is computed
//!   in memory first; conflicts come back as an *outcome* naming the files,
//!   and the working tree, the index and HEAD are exactly as they were. The
//!   IDE has one conflict surface — the Conflicts filter over a paused
//!   rebase — and reviewing published work is not the place to grow a
//!   second one.
//! - **A dirty working tree is refused, not merged over.** Every clean path
//!   below writes the working tree; discovering uncommitted work by
//!   overwriting it is not a merge strategy.
//!
//! libgit2 throughout, so no repository's hooks run — the same reason
//! [`crate::mediate`] gives.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use git2::Oid;

use crate::{FileState, GitWorkspace};

/// What a merge did, or refused to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    /// The base already contains the branch. Nothing was written.
    AlreadyUpToDate,
    /// The base was an ancestor: HEAD moved to the branch tip, no merge
    /// commit.
    FastForward,
    /// A merge commit was created with both tips as parents.
    Merged,
    /// The trees conflict. **Nothing was written** — not HEAD, not the
    /// index, not the working tree.
    Conflicted,
}

/// The result of one merge, in enough detail for the panel to say what
/// happened without asking git again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The branch that was merged, as the caller named it.
    pub branch: String,
    pub status: MergeStatus,
    /// Where the current branch points afterwards. Unchanged on
    /// [`MergeStatus::Conflicted`] and [`MergeStatus::AlreadyUpToDate`].
    pub head: Oid,
    /// Conflicting paths, sorted. Empty unless the status is
    /// [`MergeStatus::Conflicted`].
    pub conflicts: Vec<PathBuf>,
}

impl MergeOutcome {
    /// Whether the merge landed (or had nothing to do).
    pub fn clean(&self) -> bool {
        self.status != MergeStatus::Conflicted
    }

    /// Whether this merge moved the current branch.
    pub fn advanced(&self) -> bool {
        matches!(self.status, MergeStatus::FastForward | MergeStatus::Merged)
    }
}

impl GitWorkspace {
    /// Merge `branch` into the checked-out branch.
    ///
    /// `branch` is a shorthand (`agents/env/topic`) or a full ref name.
    /// Fast-forwards when it can, creates a merge commit when it must, and
    /// reports conflicts without writing anything.
    ///
    /// Errors (rather than outcomes) for: a detached or unborn HEAD, a
    /// branch that does not resolve, merging a branch into itself,
    /// unrelated histories, and a working tree with uncommitted changes.
    pub fn merge_branch(&self, branch: &str) -> Result<MergeOutcome> {
        let head_ref = self
            .head_ref_name()
            .context("HEAD is detached — check out a branch before merging")?;
        let theirs_ref = if branch.starts_with("refs/") {
            branch.to_string()
        } else {
            format!("refs/heads/{branch}")
        };
        if theirs_ref == head_ref {
            bail!("{branch} is the branch you are on");
        }
        let ours = self
            .repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .context("the current branch has no commits yet")?;
        let theirs = self
            .repo
            .find_reference(&theirs_ref)
            .and_then(|r| r.peel_to_commit())
            .with_context(|| format!("{branch} does not name a commit"))?;

        let unchanged = |status| MergeOutcome {
            branch: branch.to_string(),
            status,
            head: ours.id(),
            conflicts: Vec::new(),
        };
        if ours.id() == theirs.id() || self.repo.graph_descendant_of(ours.id(), theirs.id())? {
            return Ok(unchanged(MergeStatus::AlreadyUpToDate));
        }
        if self.repo.merge_base(ours.id(), theirs.id()).is_err() {
            bail!("{branch} shares no history with the current branch");
        }
        if self.dirty() {
            bail!(
                "commit or stash your changes first — merging writes the working tree, \
                 and there is uncommitted work in it"
            );
        }

        // The whole merge, computed in the object database. Conflicts are
        // discovered here, where discovering them costs nothing.
        let mut merged = self
            .repo
            .merge_commits(&ours, &theirs, None)
            .with_context(|| format!("merging {branch}"))?;
        if merged.has_conflicts() {
            let mut conflicts: Vec<PathBuf> = merged
                .conflicts()?
                .flatten()
                .filter_map(|c| c.our.or(c.their).or(c.ancestor))
                .filter_map(|entry| String::from_utf8(entry.path).ok())
                .map(PathBuf::from)
                .collect();
            conflicts.sort();
            conflicts.dedup();
            return Ok(MergeOutcome {
                branch: branch.to_string(),
                status: MergeStatus::Conflicted,
                head: ours.id(),
                conflicts,
            });
        }

        let short = theirs_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&theirs_ref)
            .to_string();
        // Fast-forward: their tip descends from ours, so the merge commit
        // would only add noise. Move the ref and check it out.
        let status = if self.repo.graph_descendant_of(theirs.id(), ours.id())? {
            self.repo
                .reference(
                    &head_ref,
                    theirs.id(),
                    true,
                    &format!("merge {short}: fast-forward"),
                )
                .with_context(|| format!("advancing {head_ref}"))?;
            MergeStatus::FastForward
        } else {
            let tree_oid = merged
                .write_tree_to(&self.repo)
                .context("writing the merged tree")?;
            let tree = self.repo.find_tree(tree_oid)?;
            // The user's identity: this is the user's merge, in the user's
            // repository, exactly like a commit from the composer.
            let sig = self
                .repo
                .signature()
                .context("git needs user.name and user.email to record a merge")?;
            self.repo
                .commit(
                    Some(&head_ref),
                    &sig,
                    &sig,
                    &format!("Merge branch '{short}'"),
                    &tree,
                    &[&ours, &theirs],
                )
                .context("recording the merge commit")?;
            MergeStatus::Merged
        };

        // The ref moved; bring the index and working tree with it. Force is
        // safe precisely because the dirty check above passed.
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        self.repo
            .checkout_head(Some(&mut checkout))
            .context("checking out the merged tree")?;
        let head = self.repo.head()?.peel_to_commit()?.id();
        Ok(MergeOutcome {
            branch: branch.to_string(),
            status,
            head,
            conflicts: Vec::new(),
        })
    }

    /// Any uncommitted work in the tree (staged, modified, or conflicted).
    /// Untracked files do not count — a merge does not overwrite them, and
    /// refusing over a stray build artifact would make the op unusable.
    fn dirty(&self) -> bool {
        self.status().is_ok_and(|status| {
            status.values().any(|state| {
                matches!(
                    state,
                    FileState::Modified | FileState::Staged | FileState::Conflicted
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefFile;
    use std::path::Path;

    fn hub() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        // A merge commit is the user's authored work, so it needs the
        // user's identity — which a build host need not have.
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config
            .set_str("user.email", "test@example.invalid")
            .unwrap();
        std::fs::write(dir.path().join("base.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("base.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("T", "t@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        drop(tree);
        drop(index);
        let git = GitWorkspace::discover(dir.path()).unwrap();
        (dir, git)
    }

    fn branch_with(git: &GitWorkspace, name: &str, files: &[(&str, &str)]) {
        let head = git.read_ref("refs/heads/main").unwrap().unwrap();
        git.repo.reference(name, head, true, "branch").unwrap();
        let changes: Vec<RefFile> = files
            .iter()
            .map(|(p, c)| RefFile::write(*p, c.as_bytes().to_vec()))
            .collect();
        git.commit_to_ref(name, &changes, "agent work").unwrap();
    }

    #[test]
    fn a_clean_merge_fast_forwards_and_lands_the_files() {
        let (dir, git) = hub();
        branch_with(&git, "refs/heads/agents/one/topic", &[("new.txt", "hi\n")]);
        let outcome = git.merge_branch("agents/one/topic").unwrap();
        assert_eq!(outcome.status, MergeStatus::FastForward);
        assert!(outcome.advanced());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hi\n",
            "a fast-forward checks out the merged tree"
        );
        assert_eq!(git.branch_name().as_deref(), Some("main"));
    }

    #[test]
    fn diverged_but_compatible_branches_get_a_merge_commit() {
        let (dir, git) = hub();
        branch_with(
            &git,
            "refs/heads/agents/one/topic",
            &[("theirs.txt", "t\n")],
        );
        // The user moves main on independently, touching a different file.
        std::fs::write(dir.path().join("ours.txt"), "o\n").unwrap();
        git.stage(Path::new("ours.txt")).unwrap();
        git.commit("mine").unwrap();

        let outcome = git.merge_branch("agents/one/topic").unwrap();
        assert_eq!(outcome.status, MergeStatus::Merged);
        assert!(dir.path().join("theirs.txt").exists());
        assert!(dir.path().join("ours.txt").exists());
        let tip = git.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(tip.parent_count(), 2, "a merge commit has both parents");
    }

    #[test]
    fn a_conflicting_merge_changes_nothing() {
        let (dir, git) = hub();
        branch_with(
            &git,
            "refs/heads/agents/one/topic",
            &[("base.txt", "theirs\n")],
        );
        std::fs::write(dir.path().join("base.txt"), "ours\n").unwrap();
        git.stage(Path::new("base.txt")).unwrap();
        git.commit("mine").unwrap();
        let before = git.repo.head().unwrap().peel_to_commit().unwrap().id();

        let outcome = git.merge_branch("agents/one/topic").unwrap();
        assert_eq!(outcome.status, MergeStatus::Conflicted);
        assert!(!outcome.clean());
        assert_eq!(outcome.conflicts, vec![PathBuf::from("base.txt")]);
        assert_eq!(
            git.repo.head().unwrap().peel_to_commit().unwrap().id(),
            before,
            "HEAD must not move"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("base.txt")).unwrap(),
            "ours\n",
            "no conflict markers land in the working tree"
        );
        assert!(
            git.status()
                .unwrap()
                .values()
                .all(|s| *s != FileState::Conflicted),
            "the index is left clean"
        );
    }

    #[test]
    fn an_already_merged_branch_is_a_no_op() {
        let (_dir, git) = hub();
        let head = git.read_ref("refs/heads/main").unwrap().unwrap();
        git.repo
            .reference("refs/heads/agents/one/topic", head, false, "at base")
            .unwrap();
        let outcome = git.merge_branch("agents/one/topic").unwrap();
        assert_eq!(outcome.status, MergeStatus::AlreadyUpToDate);
        assert!(!outcome.advanced());
    }

    #[test]
    fn a_dirty_tree_is_refused_rather_than_merged_over() {
        let (dir, git) = hub();
        branch_with(&git, "refs/heads/agents/one/topic", &[("new.txt", "hi\n")]);
        std::fs::write(dir.path().join("base.txt"), "edited\n").unwrap();
        let error = git
            .merge_branch("agents/one/topic")
            .unwrap_err()
            .to_string();
        assert!(error.contains("commit or stash"), "{error}");
        assert!(!dir.path().join("new.txt").exists(), "nothing was written");
    }

    #[test]
    fn merging_the_current_branch_into_itself_is_refused() {
        let (_dir, git) = hub();
        let error = git.merge_branch("main").unwrap_err().to_string();
        assert!(error.contains("the branch you are on"), "{error}");
    }
}
