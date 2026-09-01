//! Arbitrary-ref read/write, and the branch data a review inbox renders.
//!
//! Two things live here, both plumbing for docs/ENVIRONMENTS.md:
//!
//! - **The `refs/taste/*` substrate.** Issue tracking is "a ref, not a
//!   service": one file per issue on `refs/taste/issues` in the main
//!   checkout. So the IDE must be able to commit a tree to a ref it chooses
//!   **without touching HEAD, the index, or the working tree** — the user is
//!   working in that checkout, and bookkeeping must be invisible to them.
//!   Ref updates are compare-and-swap against the tip the new commit's parent
//!   was read from, so two writers racing produce a chain, never a silent
//!   overwrite.
//! - **Branch enumeration by prefix**, plus merge base and ahead/behind — the
//!   per-row data of the published-`agents/*` review inbox.
//!
//! Everything is libgit2 and runs no hooks (see [`crate::mediate`]), takes
//! its repository as a parameter, and blocks: callers wrap it in
//! `spawn_blocking`.

use anyhow::{bail, Context, Result};
use git2::Oid;

use crate::GitWorkspace;

/// One change to a ref's tree. `content: None` deletes the path; deleting a
/// path that is not there is a no-op, so callers can be idempotent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefFile {
    /// Repository-relative path, forward slashes. Nested paths are fine.
    pub path: String,
    pub content: Option<Vec<u8>>,
}

impl RefFile {
    pub fn write(path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            content: Some(content.into()),
        }
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: None,
        }
    }
}

/// One blob in the tree at a ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTreeEntry {
    /// Full path from the tree root, forward slashes.
    pub path: String,
    pub oid: Oid,
    pub size: usize,
}

/// The file listing at a ref: what `issue_list` walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTree {
    /// The commit the ref points at.
    pub commit: Oid,
    /// Blobs, sorted by path.
    pub entries: Vec<RefTreeEntry>,
}

impl RefTree {
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.path.as_str())
    }
}

/// A branch as a review-inbox row wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    /// Shorthand name, e.g. `agents/env-1/topic`.
    pub name: String,
    pub oid: Oid,
    /// Committer time of the tip, seconds since the epoch.
    pub last_commit_time: i64,
    /// Subject line of the tip commit.
    pub summary: String,
}

/// How a branch stands against a base — the whole of a review row's
/// "3 ahead, 1 behind, forked at abc1234".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchRelation {
    /// `None` when the histories are unrelated.
    pub merge_base: Option<Oid>,
    /// Commits on the branch that the base does not have.
    pub ahead: usize,
    /// Commits on the base that the branch does not have.
    pub behind: usize,
}

impl GitWorkspace {
    /// Where a ref points, or `None` when it does not exist. Symbolic refs
    /// resolve.
    pub fn read_ref(&self, name: &str) -> Result<Option<Oid>> {
        match self.repo.find_reference(name) {
            Ok(reference) => Ok(reference.resolve()?.target()),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {name}")),
        }
    }

    /// Commit `changes` onto `name`'s tip, without touching HEAD, the index,
    /// or the working tree.
    ///
    /// The parent is whatever `name` points at right now (no parent when it
    /// does not exist yet), and the tree is that parent's tree with `changes`
    /// overlaid. The ref update is compare-and-swap against that parent: if
    /// another writer moved the ref in between, this fails rather than
    /// dropping their commit, and the caller can retry on the new tip.
    ///
    /// Returns the new commit's id.
    ///
    /// Refuses `name`s that are not full ref names and the repository's
    /// checked-out branch (committing there behind the user's back would
    /// leave their working tree looking like an uncommitted revert).
    pub fn commit_to_ref(&self, name: &str, changes: &[RefFile], message: &str) -> Result<Oid> {
        if !git2::Reference::is_valid_name(name) || !name.starts_with("refs/") {
            bail!("{name} is not a valid ref name");
        }
        if self.head_ref_name().as_deref() == Some(name) {
            bail!("refusing to commit onto the checked-out branch {name}");
        }

        let parent = match self.read_ref(name)? {
            Some(oid) => Some(
                self.repo
                    .find_commit(oid)
                    .with_context(|| format!("{name} does not point at a commit"))?,
            ),
            None => None,
        };
        let base_tree = match &parent {
            Some(commit) => commit.tree()?,
            None => {
                let empty = self.repo.treebuilder(None)?.write()?;
                self.repo.find_tree(empty)?
            }
        };

        let mut builder = git2::build::TreeUpdateBuilder::new();
        for change in changes {
            if change.path.is_empty() {
                bail!("empty path in a ref write");
            }
            match &change.content {
                Some(bytes) => {
                    let blob = self.repo.blob(bytes)?;
                    builder.upsert(&change.path, blob, git2::FileMode::Blob);
                }
                None => {
                    // Idempotent: only remove what is actually there, since
                    // removing an absent path is an error in libgit2.
                    if base_tree
                        .get_path(std::path::Path::new(&change.path))
                        .is_ok()
                    {
                        builder.remove(&change.path);
                    }
                }
            }
        }
        let tree_id = builder
            .create_updated(&self.repo, &base_tree)
            .with_context(|| format!("building the tree for {name}"))?;
        let tree = self.repo.find_tree(tree_id)?;

        let signature = self.ref_signature()?;
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        // `None` for the ref: this commit lands nowhere until the
        // compare-and-swap below says it may.
        let commit = self
            .repo
            .commit(None, &signature, &signature, message, &tree, &parents)
            .with_context(|| format!("committing to {name}"))?;

        match &parent {
            Some(old) => {
                self.repo
                    .reference_matching(name, commit, true, old.id(), message)
                    .with_context(|| format!("{name} moved under this write; retry on its tip"))?;
            }
            None => {
                self.repo
                    .reference(name, commit, false, message)
                    .with_context(|| format!("{name} was created under this write; retry"))?;
            }
        }
        Ok(commit)
    }

    /// The file listing at a ref, or `None` when the ref does not exist.
    pub fn read_tree_at_ref(&self, name: &str) -> Result<Option<RefTree>> {
        let Some(oid) = self.read_ref(name)? else {
            return Ok(None);
        };
        let commit = self
            .repo
            .find_commit(oid)
            .with_context(|| format!("{name} does not point at a commit"))?;
        let tree = commit.tree()?;
        let mut entries = Vec::new();
        let mut walk_error = None;
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let Some(entry_name) = entry.name() else {
                return git2::TreeWalkResult::Ok;
            };
            match self.repo.find_blob(entry.id()) {
                Ok(blob) => entries.push(RefTreeEntry {
                    path: format!("{dir}{entry_name}"),
                    oid: entry.id(),
                    size: blob.content().len(),
                }),
                Err(e) => {
                    walk_error = Some(e);
                    return git2::TreeWalkResult::Abort;
                }
            }
            git2::TreeWalkResult::Ok
        })?;
        if let Some(e) = walk_error {
            return Err(e).with_context(|| format!("reading the tree at {name}"));
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Some(RefTree {
            commit: oid,
            entries,
        }))
    }

    /// One file's bytes at a ref, or `None` when either the ref or the path
    /// is absent.
    pub fn read_file_at_ref(&self, name: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(oid) = self.read_ref(name)? else {
            return Ok(None);
        };
        let tree = self.repo.find_commit(oid)?.tree()?;
        let Ok(entry) = tree.get_path(std::path::Path::new(path)) else {
            return Ok(None);
        };
        let Ok(blob) = self.repo.find_blob(entry.id()) else {
            return Ok(None);
        };
        Ok(Some(blob.content().to_vec()))
    }

    /// Raw bytes of a blob, for entries a listing already named.
    pub fn read_blob(&self, oid: Oid) -> Result<Vec<u8>> {
        Ok(self.repo.find_blob(oid)?.content().to_vec())
    }

    /// Delete a ref, ignoring one that is already gone.
    pub fn delete_ref(&self, name: &str) -> Result<()> {
        if self.head_ref_name().as_deref() == Some(name) {
            bail!("refusing to delete the checked-out branch {name}");
        }
        match self.repo.find_reference(name) {
            Ok(mut reference) => {
                reference.delete()?;
                Ok(())
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {name}")),
        }
    }

    /// Local branches whose shorthand starts with `prefix` (e.g. `agents/`),
    /// newest tip first — the review inbox's rows, in the order it shows
    /// them. An empty prefix means every local branch.
    pub fn branches_matching(&self, prefix: &str) -> Result<Vec<BranchInfo>> {
        let mut out = Vec::new();
        for branch in self.repo.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()?.map(str::to_string) else {
                continue; // non-UTF-8 branch name
            };
            if !name.starts_with(prefix) {
                continue;
            }
            let Ok(tip) = branch.get().peel_to_commit() else {
                continue;
            };
            out.push(BranchInfo {
                name,
                oid: tip.id(),
                last_commit_time: tip.time().seconds(),
                summary: tip.summary().unwrap_or("").to_string(),
            });
        }
        out.sort_by(|a, b| {
            b.last_commit_time
                .cmp(&a.last_commit_time)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    /// Merge base of two revisions (branch names, full refs, or ids).
    /// `None` when the histories are unrelated.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<Oid>> {
        let a = self.resolve_commit(a)?;
        let b = self.resolve_commit(b)?;
        match self.repo.merge_base(a, b) {
            Ok(oid) => Ok(Some(oid)),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e).context("computing the merge base"),
        }
    }

    /// Merge base of a revision with HEAD — the diff base a review row opens
    /// against.
    pub fn merge_base_with(&self, rev: &str) -> Result<Option<Oid>> {
        self.merge_base("HEAD", rev)
    }

    /// How many commits `rev` has that `base` does not, and vice versa.
    /// Cheap: libgit2 walks only the symmetric difference.
    pub fn ahead_behind(&self, rev: &str, base: &str) -> Result<(usize, usize)> {
        let rev = self.resolve_commit(rev)?;
        let base = self.resolve_commit(base)?;
        Ok(self.repo.graph_ahead_behind(rev, base)?)
    }

    /// Merge base and ahead/behind in one call — a whole review-inbox row.
    pub fn relation_to(&self, rev: &str, base: &str) -> Result<BranchRelation> {
        let (ahead, behind) = self.ahead_behind(rev, base)?;
        Ok(BranchRelation {
            merge_base: self.merge_base(rev, base)?,
            ahead,
            behind,
        })
    }

    fn resolve_commit(&self, rev: &str) -> Result<Oid> {
        Ok(self
            .repo
            .revparse_single(rev)
            .with_context(|| format!("resolving {rev}"))?
            .peel_to_commit()
            .with_context(|| format!("{rev} does not name a commit"))?
            .id())
    }

    /// Signature for IDE bookkeeping commits. The user's configured identity
    /// when there is one; otherwise the IDE signs as itself rather than
    /// failing an issue write on a host with no `user.name` — unlike
    /// [`GitWorkspace::commit`], nothing here is the user's authored work.
    fn ref_signature(&self) -> Result<git2::Signature<'static>> {
        match self.repo.signature() {
            Ok(sig) => Ok(sig.to_owned()),
            Err(_) => git2::Signature::now("taste-ide", "taste-ide@localhost")
                .context("building a signature"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    const ISSUES: &str = "refs/taste/issues";

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
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("base").unwrap();
        (dir, ws)
    }

    #[test]
    fn missing_refs_read_as_absent() {
        let (_dir, ws) = temp_repo();
        assert_eq!(ws.read_ref(ISSUES).unwrap(), None);
        assert_eq!(ws.read_tree_at_ref(ISSUES).unwrap(), None);
        assert_eq!(ws.read_file_at_ref(ISSUES, "issues/1.md").unwrap(), None);
        ws.delete_ref(ISSUES).unwrap(); // idempotent
    }

    #[test]
    fn commit_to_ref_chains_and_leaves_head_alone() {
        let (dir, ws) = temp_repo();
        let head_ref = ws.head_ref_name().unwrap();
        let head_oid = ws.read_ref(&head_ref).unwrap();
        assert!(head_oid.is_some());

        let first = ws
            .commit_to_ref(
                ISSUES,
                &[RefFile::write(
                    "issues/1.md",
                    "---\nstate: open\n---\nfix it\n",
                )],
                "issue 1 created",
            )
            .unwrap();
        assert_eq!(ws.read_ref(ISSUES).unwrap(), Some(first));

        let second = ws
            .commit_to_ref(
                ISSUES,
                &[RefFile::write("issues/2.md", "second\n")],
                "issue 2 created",
            )
            .unwrap();
        assert_ne!(first, second);

        // Parent chain, oldest last.
        let repo = git2::Repository::open(dir.path()).unwrap();
        let tip = repo.find_commit(second).unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), first);
        assert_eq!(tip.parent(0).unwrap().parent_count(), 0);

        // HEAD, index and working tree are exactly as they were.
        assert_eq!(ws.head_ref_name().unwrap(), head_ref);
        assert_eq!(ws.read_ref(&head_ref).unwrap(), head_oid);
        assert!(ws.status().unwrap().is_empty(), "working tree stayed clean");
        assert!(!ws.has_staged_changes().unwrap());
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec![".git".to_string(), "a.txt".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[test]
    fn sequential_writers_chain_on_each_others_tips() {
        let (dir, first_handle) = temp_repo();
        // A second handle on the same repository, as a second blocking task
        // would hold.
        let second_handle = GitWorkspace::discover(dir.path()).unwrap();

        let a = first_handle
            .commit_to_ref(ISSUES, &[RefFile::write("a", "a")], "a")
            .unwrap();
        let b = second_handle
            .commit_to_ref(ISSUES, &[RefFile::write("b", "b")], "b")
            .unwrap();
        let c = first_handle
            .commit_to_ref(ISSUES, &[RefFile::write("c", "c")], "c")
            .unwrap();

        let repo = git2::Repository::open(dir.path()).unwrap();
        let tip = repo.find_commit(c).unwrap();
        assert_eq!(tip.parent(0).unwrap().id(), b);
        assert_eq!(tip.parent(0).unwrap().parent(0).unwrap().id(), a);

        // Every write is still in the tree: nobody clobbered anybody.
        let tree = first_handle.read_tree_at_ref(ISSUES).unwrap().unwrap();
        let paths: Vec<&str> = tree.paths().collect();
        assert_eq!(paths, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_stale_parent_is_refused_rather_than_clobbering() {
        let (dir, ws) = temp_repo();
        let first = ws
            .commit_to_ref(ISSUES, &[RefFile::write("a", "a")], "a")
            .unwrap();

        // Someone else moves the ref between our read and our write. The
        // compare-and-swap is what makes that safe; here we drive it
        // directly, since the public API always reads the tip itself.
        let repo = git2::Repository::open(dir.path()).unwrap();
        let other = ws
            .commit_to_ref(ISSUES, &[RefFile::write("b", "b")], "b")
            .unwrap();
        assert!(
            repo.reference_matching(ISSUES, first, true, first, "stale")
                .is_err(),
            "a write expecting the old tip must fail"
        );
        assert_eq!(ws.read_ref(ISSUES).unwrap(), Some(other));
    }

    #[test]
    fn tree_round_trips_content_and_deletes() {
        let (_dir, ws) = temp_repo();
        let body = "---\nstate: open\nenv: env-1\n---\n\nUnicode: café 🍰\n";
        ws.commit_to_ref(
            ISSUES,
            &[
                RefFile::write("issues/1.md", body),
                RefFile::write("issues/nested/deep/2.md", "deep\n"),
                RefFile::write("index.json", vec![0u8, 159, 146, 150]), // non-UTF-8
            ],
            "seed",
        )
        .unwrap();

        let tree = ws.read_tree_at_ref(ISSUES).unwrap().unwrap();
        let paths: Vec<&str> = tree.paths().collect();
        assert_eq!(
            paths,
            vec!["index.json", "issues/1.md", "issues/nested/deep/2.md"]
        );
        assert_eq!(
            ws.read_file_at_ref(ISSUES, "issues/1.md").unwrap().unwrap(),
            body.as_bytes()
        );
        assert_eq!(
            ws.read_file_at_ref(ISSUES, "issues/nested/deep/2.md")
                .unwrap()
                .unwrap(),
            b"deep\n"
        );
        let binary = tree
            .entries
            .iter()
            .find(|e| e.path == "index.json")
            .unwrap();
        assert_eq!(binary.size, 4);
        assert_eq!(ws.read_blob(binary.oid).unwrap(), vec![0u8, 159, 146, 150]);

        // Update one file, delete another, and delete an absent one.
        ws.commit_to_ref(
            ISSUES,
            &[
                RefFile::write("issues/1.md", "closed\n"),
                RefFile::delete("issues/nested/deep/2.md"),
                RefFile::delete("issues/never-existed.md"),
            ],
            "update",
        )
        .unwrap();
        let paths: Vec<String> = ws
            .read_tree_at_ref(ISSUES)
            .unwrap()
            .unwrap()
            .paths()
            .map(str::to_string)
            .collect();
        assert_eq!(paths, vec!["index.json", "issues/1.md"]);
        assert_eq!(
            ws.read_file_at_ref(ISSUES, "issues/1.md").unwrap().unwrap(),
            b"closed\n"
        );
    }

    #[test]
    fn commit_to_ref_refuses_head_and_bad_names() {
        let (_dir, ws) = temp_repo();
        let head = ws.head_ref_name().unwrap();
        assert!(ws
            .commit_to_ref(&head, &[RefFile::write("x", "x")], "nope")
            .is_err());
        assert!(ws.delete_ref(&head).is_err());
        for name in ["taste/issues", "refs/taste/bad name", "HEAD"] {
            assert!(
                ws.commit_to_ref(name, &[RefFile::write("x", "x")], "nope")
                    .is_err(),
                "{name} should be refused"
            );
        }
    }

    #[test]
    fn branches_matching_prefix_newest_first() {
        let (dir, ws) = temp_repo();
        let repo = git2::Repository::open(dir.path()).unwrap();
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        let sig_time = |secs: i64| {
            git2::Signature::new("Test", "test@example.invalid", &git2::Time::new(secs, 0)).unwrap()
        };

        // Three branches with controlled commit times.
        let tree = base.tree().unwrap();
        let mut tips = Vec::new();
        for (i, name) in ["agents/env-1/alpha", "agents/env-2/beta", "feature/x"]
            .iter()
            .enumerate()
        {
            let sig = sig_time(1_700_000_000 + i as i64 * 100);
            let oid = repo
                .commit(
                    None,
                    &sig,
                    &sig,
                    &format!("work on {name}"),
                    &tree,
                    &[&base],
                )
                .unwrap();
            repo.reference(&format!("refs/heads/{name}"), oid, false, "test")
                .unwrap();
            tips.push(oid);
        }

        let agents = ws.branches_matching("agents/").unwrap();
        assert_eq!(agents.len(), 2, "{agents:?}");
        assert_eq!(agents[0].name, "agents/env-2/beta", "newest first");
        assert_eq!(agents[0].oid, tips[1]);
        assert_eq!(agents[0].summary, "work on agents/env-2/beta");
        assert_eq!(agents[0].last_commit_time, 1_700_000_100);
        assert_eq!(agents[1].name, "agents/env-1/alpha");

        assert_eq!(ws.branches_matching("agents/env-1/").unwrap().len(), 1);
        assert!(ws.branches_matching("nothing/").unwrap().is_empty());
        assert!(
            ws.branches_matching("").unwrap().len() >= 4,
            "empty prefix means all branches"
        );
    }

    #[test]
    fn merge_base_and_ahead_behind() {
        let (dir, ws) = temp_repo();
        let repo = git2::Repository::open(dir.path()).unwrap();
        let base_branch = ws.branch_name().unwrap();
        let fork_point = repo.head().unwrap().peel_to_commit().unwrap().id();

        // Two commits on an agent branch off the fork point.
        ws.create_branch("agents/env-1/topic").unwrap();
        for text in ["two\n", "three\n"] {
            fs::write(dir.path().join("a.txt"), text).unwrap();
            ws.stage(Path::new("a.txt")).unwrap();
            ws.commit(text.trim()).unwrap();
        }
        // One commit on the base branch after the fork.
        ws.switch_branch(&base_branch).unwrap();
        fs::write(dir.path().join("b.txt"), "base moved\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("base moved").unwrap();

        assert_eq!(
            ws.merge_base("agents/env-1/topic", &base_branch).unwrap(),
            Some(fork_point)
        );
        assert_eq!(
            ws.merge_base_with("agents/env-1/topic").unwrap(),
            Some(fork_point),
            "merge_base_with is against HEAD"
        );
        assert_eq!(
            ws.ahead_behind("agents/env-1/topic", &base_branch).unwrap(),
            (2, 1)
        );
        let relation = ws.relation_to("agents/env-1/topic", &base_branch).unwrap();
        assert_eq!(relation.merge_base, Some(fork_point));
        assert_eq!((relation.ahead, relation.behind), (2, 1));

        // Unrelated histories have no merge base and are all-ahead.
        let sig = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let orphan = repo
            .commit(
                None,
                &sig,
                &sig,
                "orphan",
                &repo.find_tree(empty).unwrap(),
                &[],
            )
            .unwrap();
        repo.reference("refs/heads/orphan", orphan, false, "test")
            .unwrap();
        assert_eq!(ws.merge_base("orphan", &base_branch).unwrap(), None);
        assert_eq!(ws.ahead_behind("orphan", &base_branch).unwrap().0, 1);

        assert!(ws.ahead_behind("no-such-branch", &base_branch).is_err());
    }
}
