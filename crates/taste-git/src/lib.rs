//! Git as a property of the file tree, not a separate interface.
//!
//! The file tree asks this crate for per-path status, toggles staging on row
//! click, and drives commit/push from its header bar. That is the entire git
//! UI surface by design (see docs/ARCHITECTURE.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Repository, Status, StatusOptions};

pub mod clone;

pub use clone::{clone_local, unpublished_work, UnpublishedBranch};

/// The user's git identity from the host's config chain (global/system/
/// XDG), for inheriting into containers. A fresh container has no
/// `user.name`/`user.email`, so every commit in it — terminals, hooks,
/// agents — fails with "Author identity unknown" until someone types the
/// two `git config` commands; the IDE knows the answer and should supply
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

/// Both halves or nothing: half an identity still cannot commit, and
/// injecting it would only mask which half is missing.
pub fn host_identity() -> Option<GitIdentity> {
    let config = git2::Config::open_default().ok()?.snapshot().ok()?;
    let name = config.get_string("user.name").ok()?;
    let email = config.get_string("user.email").ok()?;
    (!name.trim().is_empty() && !email.trim().is_empty()).then_some(GitIdentity { name, email })
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    /// Ignored: rewrites HOME, which races parallel tests. Run alone:
    /// `cargo test -p taste-git -- --ignored`.
    #[test]
    #[ignore = "mutates HOME"]
    fn host_identity_reads_the_global_config_chain() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = Test Person\n\temail = test@example.com\n",
        )
        .unwrap();
        std::env::set_var("HOME", home.path());
        std::env::remove_var("XDG_CONFIG_HOME");
        let identity = host_identity().expect("identity should be found");
        assert_eq!(identity.name, "Test Person");
        assert_eq!(identity.email, "test@example.com");

        // Anonymous host: no identity is invented.
        std::fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = Only Half\n",
        )
        .unwrap();
        assert!(host_identity().is_none());
    }
}

/// Simplified per-file state, chosen for what a file-tree row can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Clean,
    /// Changed in the working tree, not staged.
    Modified,
    /// Staged (possibly with further unstaged edits on top).
    Staged,
    /// Untracked by git.
    Untracked,
    Conflicted,
    Ignored,
}

impl FileState {
    fn from_status(s: Status) -> Self {
        if s.is_conflicted() {
            FileState::Conflicted
        } else if s.is_ignored() {
            FileState::Ignored
        } else if s.intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE,
        ) {
            FileState::Staged
        } else if s.contains(Status::WT_NEW) {
            FileState::Untracked
        } else if s.intersects(
            Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED | Status::WT_TYPECHANGE,
        ) {
            FileState::Modified
        } else {
            FileState::Clean
        }
    }

    /// Whether the row's toggle action is "stage" (vs "unstage").
    pub fn stageable(self) -> bool {
        matches!(
            self,
            FileState::Modified | FileState::Untracked | FileState::Conflicted
        )
    }
}

pub struct GitWorkspace {
    repo: Repository,
    workdir: PathBuf,
}

impl GitWorkspace {
    /// Open the repository containing `root`, if any.
    /// Initialize a fresh repository at `root` (the not-a-repo button).
    pub fn init(root: &Path) -> Result<()> {
        Repository::init(root).with_context(|| format!("git init in {}", root.display()))?;
        Ok(())
    }

    pub fn discover(root: &Path) -> Option<Self> {
        let repo = Repository::discover(root).ok()?;
        let workdir = repo.workdir()?.to_path_buf();
        Some(Self { repo, workdir })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Full status snapshot: repo-relative path → state. The file tree joins
    /// this against its rows; directories aggregate their children.
    pub fn status(&self) -> Result<HashMap<PathBuf, FileState>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        let statuses = self.repo.statuses(Some(&mut opts))?;
        let mut map = HashMap::new();
        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                map.insert(PathBuf::from(path), FileState::from_status(entry.status()));
            }
        }
        Ok(map)
    }

    /// Discard working-tree changes to one tracked file: check out its
    /// HEAD state over the working copy.
    pub fn restore_file(&self, rel_path: &Path) -> Result<()> {
        let mut builder = git2::build::CheckoutBuilder::new();
        builder.force().path(rel_path);
        self.repo.checkout_head(Some(&mut builder))?;
        Ok(())
    }

    /// argv for stashing a single file (the CLI, because libgit2's stash
    /// has no pathspec support).
    pub fn stash_file_command(&self, rel_path: &Path, message: &str) -> (String, Vec<String>) {
        (
            "git".into(),
            vec![
                "-C".into(),
                self.workdir.display().to_string(),
                "stash".into(),
                "push".into(),
                "--include-untracked".into(),
                "-m".into(),
                message.into(),
                "--".into(),
                rel_path.display().to_string(),
            ],
        )
    }

    /// Per-stash-entry touched paths, newest first (index 0 = newest),
    /// for "which stash holds this file" lookups.
    pub fn stash_entries(&self) -> Result<Vec<std::collections::HashSet<PathBuf>>> {
        let mut scratch = Repository::open(&self.workdir)?;
        let mut oids = Vec::new();
        scratch.stash_foreach(|_, _, oid| {
            oids.push(*oid);
            true
        })?;
        let mut entries = Vec::new();
        for oid in oids {
            let mut paths = std::collections::HashSet::new();
            let commit = self.repo.find_commit(oid)?;
            let stash_tree = commit.tree()?;
            if let Ok(base) = commit.parent(0) {
                let base_tree = base.tree()?;
                let diff =
                    self.repo
                        .diff_tree_to_tree(Some(&base_tree), Some(&stash_tree), None)?;
                for delta in diff.deltas() {
                    if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path())
                    {
                        paths.insert(path.to_path_buf());
                    }
                }
            }
            if let Ok(untracked) = commit.parent(2) {
                let tree = untracked.tree()?;
                tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                    if entry.kind() == Some(git2::ObjectType::Blob) {
                        if let Some(name) = entry.name() {
                            paths.insert(PathBuf::from(format!("{dir}{name}")));
                        }
                    }
                    git2::TreeWalkResult::Ok
                })?;
            }
            entries.push(paths);
        }
        Ok(entries)
    }

    /// argv to restore one file's content from a stash entry (brings the
    /// change back to the working tree — "unstash this file").
    pub fn unstash_file_command(
        &self,
        stash_index: usize,
        rel_path: &Path,
    ) -> (String, Vec<String>) {
        (
            "git".into(),
            vec![
                "-C".into(),
                self.workdir.display().to_string(),
                "checkout".into(),
                format!("stash@{{{stash_index}}}"),
                "--".into(),
                rel_path.display().to_string(),
            ],
        )
    }

    /// Paths (relative to workdir) touched by any stash entry. A stash
    /// commit's first parent is the base; a third parent, when present,
    /// carries the untracked files captured by `stash -u`.
    pub fn stashed_paths(&self) -> Result<std::collections::HashSet<PathBuf>> {
        // stash_foreach needs &mut Repository; use a scratch handle so the
        // shared one stays immutable.
        let mut scratch = Repository::open(&self.workdir)?;
        let mut oids = Vec::new();
        scratch.stash_foreach(|_, _, oid| {
            oids.push(*oid);
            true
        })?;
        let mut paths = std::collections::HashSet::new();
        for oid in oids {
            let commit = self.repo.find_commit(oid)?;
            let stash_tree = commit.tree()?;
            if let Ok(base) = commit.parent(0) {
                let base_tree = base.tree()?;
                let diff =
                    self.repo
                        .diff_tree_to_tree(Some(&base_tree), Some(&stash_tree), None)?;
                for delta in diff.deltas() {
                    if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path())
                    {
                        paths.insert(path.to_path_buf());
                    }
                }
            }
            if let Ok(untracked) = commit.parent(2) {
                let tree = untracked.tree()?;
                tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                    if entry.kind() == Some(git2::ObjectType::Blob) {
                        if let Some(name) = entry.name() {
                            paths.insert(PathBuf::from(format!("{dir}{name}")));
                        }
                    }
                    git2::TreeWalkResult::Ok
                })?;
            }
        }
        Ok(paths)
    }

    /// Content of `rel_path` at HEAD — the baseline for "changes since
    /// last commit" views. None for untracked files (no baseline) and
    /// non-UTF-8 blobs.
    pub fn head_content(&self, rel_path: &Path) -> Option<String> {
        let tree = self.repo.head().ok()?.peel_to_tree().ok()?;
        let entry = tree.get_path(rel_path).ok()?;
        let blob = self.repo.find_blob(entry.id()).ok()?;
        String::from_utf8(blob.content().to_vec()).ok()
    }

    /// Stage one path (file add/update or deletion).
    pub fn stage(&self, rel_path: &Path) -> Result<()> {
        let mut index = self.repo.index()?;
        if self.workdir.join(rel_path).exists() {
            index.add_path(rel_path)?;
        } else {
            index.remove_path(rel_path)?;
        }
        index.write()?;
        Ok(())
    }

    /// Unstage one path (reset its index entry to HEAD).
    pub fn unstage(&self, rel_path: &Path) -> Result<()> {
        let head = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        match head {
            Some(commit) => self
                .repo
                .reset_default(Some(commit.as_object()), [rel_path])?,
            None => {
                // Unborn branch: unstaging means removing from the index.
                let mut index = self.repo.index()?;
                index.remove_path(rel_path)?;
                index.write()?;
            }
        }
        Ok(())
    }

    /// True when the index differs from HEAD (i.e. commit would do something).
    pub fn has_staged_changes(&self) -> Result<bool> {
        let head_tree = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let index = self.repo.index()?;
        let diff = self
            .repo
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)?;
        Ok(diff.deltas().len() > 0)
    }

    /// The staged diff as patch text (for AI commit-message suggestions),
    /// capped so huge diffs stay promptable.
    pub fn staged_diff(&self, max_bytes: usize) -> Result<String> {
        let head_tree = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let index = self.repo.index()?;
        let diff = self
            .repo
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)?;
        let mut text = String::new();
        let mut truncated = false;
        diff.print(git2::DiffFormat::Patch, |_, _, line| {
            if text.len() >= max_bytes {
                truncated = true;
                return false; // stop printing
            }
            text.push(line.origin());
            text.push_str(&String::from_utf8_lossy(line.content()));
            true
        })
        .ok(); // stopping early surfaces as an error; the text is still good
        if truncated {
            text.push_str("\n… (diff truncated)\n");
        }
        Ok(text)
    }

    /// Commit the index with the given message, using the repo's configured
    /// signature.
    pub fn commit(&self, message: &str) -> Result<git2::Oid> {
        let sig = self
            .repo
            .signature()
            .context("git identity not configured (user.name / user.email)")?;
        let mut index = self.repo.index()?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let parent = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
        Ok(oid)
    }

    /// Name of the current branch, for the header-bar indicator.
    /// Local branch names, current first.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        let current = self.branch_name();
        let mut names: Vec<String> = self
            .repo
            .branches(Some(git2::BranchType::Local))?
            .filter_map(|b| b.ok())
            .filter_map(|(branch, _)| branch.name().ok().flatten().map(str::to_string))
            .collect();
        names.sort_by_key(|name| (Some(name) != current.as_ref(), name.clone()));
        Ok(names)
    }

    /// Check out an existing local branch. Fails (rather than clobbers)
    /// when working-tree changes conflict with the target.
    pub fn switch_branch(&self, name: &str) -> Result<()> {
        let reference = format!("refs/heads/{name}");
        let obj = self.repo.revparse_single(&reference)?;
        self.repo.checkout_tree(&obj, None)?;
        self.repo.set_head(&reference)?;
        Ok(())
    }

    /// Create a branch at HEAD and switch to it.
    pub fn create_branch(&self, name: &str) -> Result<()> {
        let head = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(name, &head, false)?;
        self.switch_branch(name)
    }

    pub fn branch_name(&self) -> Option<String> {
        let head = self.repo.head().ok()?;
        head.shorthand().map(str::to_owned)
    }

    /// Push the current branch to its upstream (or origin by default).
    ///
    /// Uses the git CLI rather than libgit2 so that the user's existing
    /// credential helpers, SSH agent, and remote helpers all just work.
    /// Push is a *user* action only: it is never exposed to agents (their
    /// sandbox blocks push at the git layer) nor over MCP.
    pub fn push_command(&self) -> (String, Vec<String>) {
        self.git_command(&["push"])
    }

    fn git_command(&self, args: &[&str]) -> (String, Vec<String>) {
        (
            "git".to_string(),
            ["-C", &self.workdir.display().to_string()]
                .iter()
                .map(|s| s.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect(),
        )
    }

    // --- sync with the remote tip (fetch + rebase, never merge) ----------

    /// How the current branch relates to its upstream tip.
    pub fn sync_status(&self) -> Result<SyncStatus> {
        let head = self.repo.head().ok().and_then(|h| h.target());
        let branch_name = match self.branch_name() {
            Some(name) => name,
            None => return Ok(SyncStatus::no_upstream()),
        };
        let branch = self
            .repo
            .find_branch(&branch_name, git2::BranchType::Local)?;
        let Ok(upstream) = branch.upstream() else {
            return Ok(SyncStatus::no_upstream());
        };
        let upstream_name = upstream
            .name()?
            .map(str::to_owned)
            .unwrap_or_else(|| "upstream".into());
        let (Some(local), Some(remote)) = (head, upstream.get().target()) else {
            return Ok(SyncStatus::no_upstream());
        };
        let (ahead, behind) = self.repo.graph_ahead_behind(local, remote)?;
        Ok(SyncStatus {
            upstream: Some(upstream_name),
            ahead,
            behind,
        })
    }

    /// Fetch from the branch's remote (read-only remote operation).
    pub fn fetch_command(&self) -> (String, Vec<String>) {
        self.git_command(&["fetch", "--prune"])
    }

    /// Rebase local work onto the upstream tip. `--autostash` keeps dirty
    /// working trees from blocking the sync.
    pub fn rebase_command(&self) -> (String, Vec<String>) {
        self.git_command(&["rebase", "--autostash", "@{upstream}"])
    }

    pub fn rebase_abort_command(&self) -> (String, Vec<String>) {
        self.git_command(&["rebase", "--abort"])
    }

    /// Resume a conflicted rebase after the conflicts were resolved and
    /// staged. `git` itself enforces that precondition and says why not.
    pub fn rebase_continue_command(&self) -> (String, Vec<String>) {
        // GIT_EDITOR=true: keep the original commit messages; an editor
        // prompt would hang a headless subprocess.
        self.git_command(&["-c", "core.editor=true", "rebase", "--continue"])
    }

    /// True while a conflicted rebase is waiting for resolution.
    pub fn rebase_in_progress(&self) -> bool {
        let git_dir = self.repo.path();
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
    }
}

/// Ahead/behind relation to the upstream tip, for the sync indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// Upstream ref name (e.g. `origin/main`), if the branch has one.
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

impl SyncStatus {
    fn no_upstream() -> Self {
        Self {
            upstream: None,
            ahead: 0,
            behind: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_repo() -> (tempfile::TempDir, GitWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(repo);
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn branch_create_switch_list() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let original = ws.branch_name().unwrap();
        ws.create_branch("feature/x").unwrap();
        assert_eq!(ws.branch_name().as_deref(), Some("feature/x"));
        let branches = ws.local_branches().unwrap();
        assert_eq!(branches[0], "feature/x"); // current sorts first
        assert!(branches.contains(&original));
        ws.switch_branch(&original).unwrap();
        assert_eq!(ws.branch_name(), Some(original));
    }

    #[test]
    fn restore_file_discards_working_changes() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        ws.restore_file(Path::new("a.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(ws.status().unwrap().is_empty());
    }

    #[test]
    fn stashed_paths_cover_tracked_and_untracked() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        assert!(ws.stashed_paths().unwrap().is_empty());

        // Modify a tracked file and add an untracked one, stash both.
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();
        {
            let mut repo = Repository::open(dir.path()).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            repo.stash_save(&sig, "wip", Some(git2::StashFlags::INCLUDE_UNTRACKED))
                .unwrap();
        }
        let stashed = ws.stashed_paths().unwrap();
        assert!(stashed.contains(Path::new("a.txt")), "{stashed:?}");
        assert!(stashed.contains(Path::new("new.txt")), "{stashed:?}");
        // The working tree is clean again — only the stash knows them.
        let status = ws.status().unwrap();
        assert!(!status.contains_key(Path::new("new.txt")));
    }

    #[test]
    fn untracked_then_staged_then_committed() {
        let (dir, ws) = temp_repo();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hi\n").unwrap();

        let status = ws.status().unwrap();
        assert_eq!(status[Path::new("hello.txt")], FileState::Untracked);

        ws.stage(Path::new("hello.txt")).unwrap();
        let status = ws.status().unwrap();
        assert_eq!(status[Path::new("hello.txt")], FileState::Staged);
        assert!(ws.has_staged_changes().unwrap());

        ws.commit("first").unwrap();
        assert!(!ws.has_staged_changes().unwrap());
        assert!(ws.status().unwrap().is_empty());
    }

    #[test]
    fn unstage_returns_file_to_modified() {
        let (dir, ws) = temp_repo();
        let file = dir.path().join("a.txt");
        fs::write(&file, "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("base").unwrap();

        fs::write(&file, "two\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        assert_eq!(ws.status().unwrap()[Path::new("a.txt")], FileState::Staged);

        ws.unstage(Path::new("a.txt")).unwrap();
        assert_eq!(
            ws.status().unwrap()[Path::new("a.txt")],
            FileState::Modified
        );
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-git perf_ -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_status_on_large_repo() {
        let (dir, ws) = temp_repo();
        for i in 0..1000 {
            let sub = dir.path().join(format!("mod{}", i % 25));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("f{i}.rs")), "fn a() {}\n").unwrap();
        }
        let start = std::time::Instant::now();
        let status = ws.status().unwrap();
        println!(
            "git status: 1000 untracked files → {} entries in {:?}",
            status.len(),
            start.elapsed()
        );
    }

    #[test]
    fn unstage_on_unborn_branch() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.unstage(Path::new("a.txt")).unwrap();
        assert_eq!(
            ws.status().unwrap()[Path::new("a.txt")],
            FileState::Untracked
        );
    }
}
