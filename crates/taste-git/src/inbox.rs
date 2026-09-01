//! The review inbox, as data — the PREVIOUS generation.
//!
//! Review is now a state each environment is in, not a list of published
//! branches: see [`crate::review`] for the branch of record and the
//! mergedness fact, and docs/ENVIRONMENTS.md, "The review lifecycle". What
//! survives here and is not going away is [`GitWorkspace::changed_since_base`]
//! — a branch's changed files against the merge base, which is still how a
//! branch is *read* however it is listed.
//!
//! [`GitWorkspace::review_inbox`] remains for the file tree's Inbox filter
//! until that filter is replaced. It is a rendering of what this module
//! returns, kept in one pure, testable place rather than in a GTK
//! callback.
//!
//! Nothing here writes. Reviewing is reading; merging and deleting are
//! separate, explicit calls the user makes ([`crate::merge`],
//! [`crate::GitWorkspace::delete_ref`]).

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::refs::{BranchInfo, BranchRelation};
use crate::GitWorkspace;

/// Where published agent work lands. One prefix, everywhere: the publish
/// tool builds ref names from it and the inbox reads them back.
pub const AGENT_BRANCH_PREFIX: &str = "agents/";

/// One review-inbox row: a published branch and how it stands against the
/// branch the user is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    pub branch: BranchInfo,
    pub relation: BranchRelation,
}

impl InboxEntry {
    /// The environment that published this.
    ///
    /// Handles both generations: `agents/<env>` — the branch of record, and
    /// the only thing anything writes now — and the dead
    /// `agents/<env>/<topic>` names, which the inbox still shows rather
    /// than hides, because a ref the user can see is better than a ref they
    /// cannot.
    pub fn environment(&self) -> Option<&str> {
        if let Some(env) = crate::review::env_of_branch(&self.branch.name) {
            return Some(env);
        }
        let rest = self.branch.name.strip_prefix(AGENT_BRANCH_PREFIX)?;
        let (env, topic) = rest.split_once('/')?;
        (!env.is_empty() && !topic.is_empty()).then_some(env)
    }

    /// The part after the environment, or the whole name when there is no
    /// environment segment.
    pub fn topic(&self) -> &str {
        match self
            .branch
            .name
            .strip_prefix(AGENT_BRANCH_PREFIX)
            .and_then(|rest| rest.split_once('/'))
        {
            Some((_, topic)) if !topic.is_empty() => topic,
            _ => &self.branch.name,
        }
    }

    /// Nothing on this branch that the base does not already have. This is
    /// the mergedness question phase 7 needs before it may close an issue —
    /// a query, never an assumption.
    pub fn merged(&self) -> bool {
        self.relation.ahead == 0
    }
}

/// What one file did between the merge base and a branch's tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Other,
}

impl ChangeKind {
    /// One-character badge, matching the changed-files list's vocabulary.
    pub fn badge(self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Other => "",
        }
    }
}

/// One row of a published branch's changed-files list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative path, as the branch leaves it (the new side of
    /// a rename).
    pub path: PathBuf,
    pub kind: ChangeKind,
}

impl GitWorkspace {
    /// Every branch under `prefix`, newest first, each with its relation to
    /// `base` — the whole review inbox in one call.
    ///
    /// `base` is any revision (`HEAD`, a branch name, an id). The branch the
    /// caller is standing on is left out when it happens to match the
    /// prefix: an inbox row offering to merge a branch into itself is noise.
    pub fn review_inbox(&self, prefix: &str, base: &str) -> Result<Vec<InboxEntry>> {
        let current = self.branch_name();
        let mut out = Vec::new();
        for branch in self.branches_matching(prefix)? {
            if current.as_deref() == Some(branch.name.as_str()) {
                continue;
            }
            let relation = self
                .relation_to(&branch.name, base)
                .with_context(|| format!("comparing {} with {base}", branch.name))?;
            out.push(InboxEntry { branch, relation });
        }
        Ok(out)
    }

    /// What `branch` changed since it forked from `base`: the file list a
    /// review row opens.
    ///
    /// Against the merge base, not against `base`'s tip — otherwise every
    /// commit the user made in the meantime would show up as the agent
    /// having deleted their work. Unrelated histories diff against the
    /// empty tree, which is the honest reading of "everything on it is new".
    pub fn changed_since_base(&self, branch: &str, base: &str) -> Result<Vec<ChangedFile>> {
        let tip = self
            .repo
            .revparse_single(branch)
            .with_context(|| format!("resolving {branch}"))?
            .peel_to_commit()
            .with_context(|| format!("{branch} does not name a commit"))?;
        let merge_base = self.merge_base(branch, base)?;
        let from = match merge_base {
            Some(oid) => Some(self.repo.find_commit(oid)?.tree()?),
            None => None,
        };
        let to = tip.tree()?;
        let diff = self
            .repo
            .diff_tree_to_tree(from.as_ref(), Some(&to), None)
            .context("diffing the branch against its merge base")?;

        // A BTreeSet keys the list by path: a rename reports two deltas in
        // some configurations, and the review list wants one row per file.
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for delta in diff.deltas() {
            let kind = match delta.status() {
                git2::Delta::Added | git2::Delta::Copied => ChangeKind::Added,
                git2::Delta::Deleted => ChangeKind::Deleted,
                git2::Delta::Renamed => ChangeKind::Renamed,
                git2::Delta::Modified | git2::Delta::Typechange => ChangeKind::Modified,
                _ => ChangeKind::Other,
            };
            let file = if delta.status() == git2::Delta::Deleted {
                delta.old_file()
            } else {
                delta.new_file()
            };
            let Some(path) = file.path().map(PathBuf::from) else {
                continue;
            };
            if seen.insert(path.clone()) {
                out.push(ChangedFile { path, kind });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A repo checked out on `work`, with `main` beside it at the same base
    /// commit — so tests can advance `main` through `commit_to_ref`, which
    /// (rightly) refuses to write the checked-out branch.
    fn hub() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        // Name the branch rather than inherit init.defaultBranch: the tests
        // say `main` and `work`, and the host's config does not vote.
        repo.set_head("refs/heads/work").unwrap();
        std::fs::write(dir.path().join("base.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("base.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("T", "t@example.invalid").unwrap();
        let base = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        repo.reference("refs/heads/main", base, false, "base")
            .unwrap();
        drop(tree);
        drop(index);
        let git = GitWorkspace::discover(dir.path()).unwrap();
        (dir, git)
    }

    /// Commit `files` onto `ref_name`, parented on its tip (or HEAD's, when
    /// the ref is new). Never touches the working tree.
    fn commit_on(git: &GitWorkspace, ref_name: &str, files: &[(&str, Option<&str>)]) {
        let changes: Vec<crate::refs::RefFile> = files
            .iter()
            .map(|(path, content)| match content {
                Some(text) => crate::refs::RefFile::write(*path, text.as_bytes().to_vec()),
                None => crate::refs::RefFile::delete(*path),
            })
            .collect();
        if git.read_ref(ref_name).unwrap().is_none() {
            let head = git.read_ref("refs/heads/main").unwrap();
            if let Some(oid) = head {
                git.repo
                    .reference(ref_name, oid, false, "branch off main")
                    .unwrap();
            }
        }
        git.commit_to_ref(ref_name, &changes, "work").unwrap();
    }

    #[test]
    fn inbox_pairs_every_published_branch_with_its_relation() {
        let (_dir, git) = hub();
        commit_on(&git, "refs/heads/agents/one/topic", &[("a.txt", Some("a"))]);
        commit_on(&git, "refs/heads/agents/two/topic", &[("b.txt", Some("b"))]);
        // Not published work: a plain branch of the user's.
        commit_on(&git, "refs/heads/scratch", &[("c.txt", Some("c"))]);

        let inbox = git.review_inbox(AGENT_BRANCH_PREFIX, "HEAD").unwrap();
        let names: Vec<&str> = inbox.iter().map(|e| e.branch.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"agents/one/topic"));
        assert!(names.contains(&"agents/two/topic"));
        assert!(!names.contains(&"scratch"), "only the prefix is the inbox");

        for entry in &inbox {
            assert_eq!(entry.relation.ahead, 1, "one commit past the base");
            assert_eq!(entry.relation.behind, 0);
            assert!(entry.relation.merge_base.is_some());
            assert!(!entry.merged());
            assert_eq!(entry.topic(), "topic");
        }
        assert!(inbox[0].environment().is_some());
    }

    #[test]
    fn an_ancestor_branch_reads_as_merged() {
        let (_dir, git) = hub();
        // Points at the base commit itself: nothing on it HEAD lacks.
        let base = git.read_ref("refs/heads/work").unwrap().unwrap();
        git.repo
            .reference("refs/heads/agents/done/topic", base, false, "at base")
            .unwrap();
        let inbox = git.review_inbox(AGENT_BRANCH_PREFIX, "HEAD").unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].merged(), "ahead == 0 is the mergedness answer");
        assert_eq!(inbox[0].relation.ahead, 0);
    }

    #[test]
    fn the_branch_you_are_standing_on_is_not_an_inbox_row() {
        let (dir, git) = hub();
        commit_on(&git, "refs/heads/agents/one/topic", &[("a.txt", Some("a"))]);
        git.switch_branch("agents/one/topic").unwrap();
        let git = GitWorkspace::discover(dir.path()).unwrap();
        assert!(
            git.review_inbox(AGENT_BRANCH_PREFIX, "HEAD")
                .unwrap()
                .is_empty(),
            "merging a branch into itself is not a review"
        );
    }

    #[test]
    fn changed_files_are_against_the_merge_base_not_the_base_tip() {
        let (_dir, git) = hub();
        commit_on(
            &git,
            "refs/heads/agents/one/topic",
            &[("added.txt", Some("new")), ("base.txt", Some("edited\n"))],
        );
        // The user moves on independently; that must not show up as the
        // agent having touched their file.
        commit_on(&git, "refs/heads/main", &[("mine.txt", Some("mine"))]);

        let changed = git.changed_since_base("agents/one/topic", "main").unwrap();
        let paths: Vec<String> = changed
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["added.txt", "base.txt"], "{paths:?}");
        assert_eq!(changed[0].kind, ChangeKind::Added);
        assert_eq!(changed[1].kind, ChangeKind::Modified);
    }

    #[test]
    fn deletions_report_the_path_that_went_away() {
        let (_dir, git) = hub();
        commit_on(&git, "refs/heads/agents/one/topic", &[("base.txt", None)]);
        let changed = git.changed_since_base("agents/one/topic", "main").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("base.txt"));
        assert_eq!(changed[0].kind, ChangeKind::Deleted);
    }
}
