//! **Working-tree git, wherever the working tree is.**
//!
//! The file tree is the git interface (ARCHITECTURE.md), and every one of
//! its verbs — status, stage, unstage, commit, discard, switch, stash — is
//! about a working tree. On this host that is libgit2 through
//! [`taste_git::GitWorkspace`], as it always was. For a checkout in a VM
//! there is no working tree here to open, so the same verbs run as `git`
//! beside the files, through the files service, and only their output
//! comes back. [`Worktree`] is one value for both, so the panes ask one
//! question and never know which arm answered.
//!
//! # What stays on the peer
//!
//! Everything about **refs and history** — branches, ahead/behind, the
//! upstream, issues, review, publish, push, fetch — reads the peer on this
//! host, where libgit2 has always read it and where the user's remote and
//! keys are. A remote working tree changes refs (a commit, a switch), so a
//! [`Worktree::Remote`] carries `after_ref_change`: what the registry does
//! to bring the peer up to date when one of those has happened, which is a
//! fetch from the VM and a fast-forward of the host folder when it is
//! clean.
//!
//! # Blocking
//!
//! Every method blocks, like the `GitWorkspace` calls they replace, and for
//! the same reason belongs on `spawn_blocking` rather than the GTK thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use taste_core::environment::Checkout;
use taste_core::files::{ExecOutput, Files};
use taste_git::{FileState, GitWorkspace, StashEntry};

/// git ran beside the files and said no — as distinct from the files
/// service not answering, which is an error of its own kind. The file tree
/// tells the two apart: a VM that is not answering is one thing to say, and
/// git refusing for a moment while a container's hook rewrites a submodule
/// under it is another (2026-10-05).
#[derive(Debug)]
pub struct GitFailed(pub String);

impl std::fmt::Display for GitFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitFailed {}

/// The working tree of one environment, on this host or in a VM.
#[derive(Clone)]
pub enum Worktree {
    /// On this host, at this path: libgit2.
    Local(PathBuf),
    /// In a VM, at this path there: `git` through the files service.
    Remote {
        files: Files,
        path: PathBuf,
        /// Run after a commit, a switch, or anything else that moves a ref
        /// over there, so the peer on this host learns of it.
        after_ref_change: Option<Arc<dyn Fn() + Send + Sync>>,
    },
}

impl std::fmt::Debug for Worktree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Worktree::Local(path) => write!(f, "Worktree::Local({})", path.display()),
            Worktree::Remote { path, files, .. } => {
                write!(
                    f,
                    "Worktree::Remote({} via {})",
                    path.display(),
                    files.describe()
                )
            }
        }
    }
}

impl Worktree {
    /// The working tree of a checkout, reached through `files`.
    pub fn for_checkout(checkout: &Checkout, files: Files) -> Self {
        match checkout {
            Checkout::Local(path) => Worktree::Local(path.clone()),
            Checkout::Remote { path, .. } => Worktree::Remote {
                files,
                path: path.clone(),
                after_ref_change: None,
            },
        }
    }

    /// [`Self::for_checkout`], with the peer sync a remote tree runs after
    /// it moves a ref.
    pub fn with_after_ref_change(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        if let Worktree::Remote {
            after_ref_change, ..
        } = &mut self
        {
            *after_ref_change = Some(hook);
        }
        self
    }

    /// The working tree's path in its own world.
    pub fn root(&self) -> &Path {
        match self {
            Worktree::Local(path) | Worktree::Remote { path, .. } => path,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Worktree::Local(_))
    }

    /// The repository's working directory: the root, unless a local root
    /// is a folder inside a larger repository, in which case that one's.
    /// What relative paths are relative to.
    pub fn workdir(&self) -> Option<PathBuf> {
        match self {
            Worktree::Local(_) => self.local().ok().map(|git| git.workdir().to_path_buf()),
            Worktree::Remote { path, .. } => Some(path.clone()),
        }
    }

    /// Stash several paths as one entry, untracked ones included.
    pub fn stash_paths(&self, rels: &[PathBuf], message: &str) -> Result<()> {
        let mut args = vec!["stash", "push", "--include-untracked", "-m", message, "--"];
        let rels: Vec<String> = rels.iter().map(|r| r.display().to_string()).collect();
        args.extend(rels.iter().map(String::as_str));
        self.git_ok(&args)?;
        Ok(())
    }

    /// Take one side of a conflict for several paths: `--ours` or
    /// `--theirs`, as git names them (a rebase inverts the meaning; the
    /// caller maps meaning to flag).
    pub fn checkout_side(&self, side: &str, rels: &[PathBuf]) -> Result<()> {
        let mut args = vec!["checkout", side, "--"];
        let rels: Vec<String> = rels.iter().map(|r| r.display().to_string()).collect();
        args.extend(rels.iter().map(String::as_str));
        self.git_ok(&args)?;
        Ok(())
    }

    /// Which of `rels` git ignores, by its own rules. Tracked paths are
    /// never reported, which is what a listing wants.
    pub fn ignored(&self, rels: &[PathBuf]) -> Result<HashSet<PathBuf>> {
        if rels.is_empty() {
            return Ok(HashSet::new());
        }
        // One name per line, unquoted (`-z` is for `--stdin`, which the
        // service has no channel for): a name with a newline in it is the
        // one shape this misreads, and the tree has never shown one.
        let mut args = vec!["-c", "core.quotePath=false", "check-ignore", "--"];
        let names: Vec<String> = rels.iter().map(|r| r.display().to_string()).collect();
        args.extend(names.iter().map(String::as_str));
        let out = self.run_git(&args, &[])?;
        // 0: some ignored; 1: none; anything else is an error.
        if out.status != 0 && out.status != 1 {
            bail!("{}", out.stderr_utf8().trim());
        }
        Ok(out
            .stdout_utf8()
            .lines()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    /// How this tree's files are reached.
    pub fn files(&self) -> Files {
        match self {
            Worktree::Local(_) => Files::Local,
            Worktree::Remote { files, .. } => files.clone(),
        }
    }

    fn local(&self) -> Result<GitWorkspace> {
        match self {
            Worktree::Local(path) => GitWorkspace::discover(path)
                .with_context(|| format!("{} is not a git working tree", path.display())),
            Worktree::Remote { .. } => bail!("not a local working tree"),
        }
    }

    /// Run `git <args>` in the working tree, wherever it is. On this host
    /// through `std::process`; in the VM through the files service. The
    /// IDE's own git, so nothing that could ask a question.
    ///
    /// Always `--no-optional-locks`, which git documents for exactly this:
    /// a process in the background that must not contend with the
    /// repository's own users. Without it the file tree's `git status`,
    /// run on every change, refreshed the index as a side effect and took
    /// `index.lock` to do it, and the agent working in the same checkout
    /// met "index.lock: File exists" in the middle of a rebase
    /// (2026-10-02). What genuinely writes the index — a stage, a commit —
    /// still locks it.
    pub fn run_git(&self, args: &[&str], envs: &[(String, String)]) -> Result<ExecOutput> {
        match self {
            Worktree::Local(path) => {
                let output = std::process::Command::new("git")
                    .args(taste_git::private::cli_prefix(path))
                    .arg("--no-optional-locks")
                    .args(args)
                    .envs(taste_git::non_interactive_env())
                    .envs(envs.iter().cloned())
                    .stdin(std::process::Stdio::null())
                    .output()
                    .context("running git")?;
                Ok(ExecOutput {
                    status: output.status.code().unwrap_or(-1),
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Worktree::Remote { files, path, .. } => {
                // `Files::exec` takes no environment, so what the caller
                // asked for rides in through `env(1)`. It is not optional:
                // a `GIT_INDEX_FILE` dropped here pointed the stash rebuild's
                // read-tree and update-index at the checkout's REAL index,
                // and a Discard left 457 files staged for deletion
                // (2026-09-28). The background's non-interactive settings
                // are the files service's own business and stay out.
                let mut argv: Vec<String> = Vec::new();
                if !envs.is_empty() {
                    argv.push("env".into());
                    argv.extend(envs.iter().map(|(key, value)| format!("{key}={value}")));
                }
                argv.push("git".into());
                argv.push("--no-optional-locks".into());
                argv.extend(args.iter().map(|s| s.to_string()));
                let out = files
                    .exec(path, &argv)
                    .with_context(|| format!("running git in {}", files.describe()))?;
                Ok(out)
            }
        }
    }

    /// `run_git`, failing on a non-zero status with git's last line.
    fn git_ok(&self, args: &[&str]) -> Result<String> {
        let out = self.run_git(args, &[])?;
        if !out.success() {
            let stderr = out.stderr_utf8();
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("git failed")
                .to_string();
            return Err(GitFailed(last).into());
        }
        Ok(out.stdout_utf8())
    }

    fn ref_changed(&self) {
        if let Worktree::Remote {
            after_ref_change: Some(hook),
            ..
        } = self
        {
            hook();
        }
    }

    /// What `git status` shows, keyed by path relative to the root.
    pub fn status(&self) -> Result<HashMap<PathBuf, FileState>> {
        match self {
            Worktree::Local(_) => self.local()?.status(),
            Worktree::Remote { .. } => {
                let out =
                    self.git_ok(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
                let mut status = taste_git::status_from_porcelain(&out);
                // And the files inside each submodule, which the parent's
                // status names only as the submodule, modified — so only
                // asked when something is. One that cannot be read leaves
                // the parent's answer as it is.
                if status.values().any(|s| *s == FileState::Modified) {
                    if let Ok(subs) = self.git_ok(&taste_git::SUBMODULE_STATUS_ARGS) {
                        let subs = taste_git::status_from_submodule_porcelain(&subs);
                        // A submodule whose files are listed is not listed
                        // itself as well: that row is a folder posing as a
                        // file, which opened as "EISDIR" from the Dirty list.
                        status.retain(|path, _| {
                            !subs
                                .keys()
                                .any(|inner| inner != path && inner.starts_with(path))
                        });
                        status.extend(subs);
                    }
                }
                Ok(status)
            }
        }
    }

    /// Each submodule of this checkout, by its path, and where it is: the
    /// branch its own HEAD is on, else a tag at that exact commit, else the
    /// commit, short — or, not checked out, the commit the parent records —
    /// and how that stands to the commit the parent records. What the file
    /// tree writes after a submodule's name, and what it warns about
    /// (David, 2026-10-06: "I'd also like them to show in the file tree as
    /// [folder] @ [commit-ish]"; "Rather than the 'M' label on the
    /// submodule if it's out of sync with the pin, we should probably show
    /// an indicator of an actual problem"). Empty for a checkout with none,
    /// and when git cannot say.
    pub fn submodule_heads(&self) -> HashMap<PathBuf, SubmoduleHead> {
        let Ok(status) = self.run_git(&["submodule", "status"], &[]) else {
            return HashMap::new();
        };
        if !status.success() {
            return HashMap::new();
        }
        let mut heads = submodule_status_heads(&status.stdout_utf8());
        if heads
            .values()
            .any(|head| head.pin != SubmodulePin::NotCheckedOut)
        {
            if let Ok(named) = self.run_git(&SUBMODULE_NAMES, &[]) {
                if named.success() {
                    for (path, head) in submodule_names(&named.stdout_utf8()) {
                        // A conflict is the parent's to say, and stays.
                        if heads.get(&path).map(|h| &h.pin) != Some(&SubmodulePin::Conflicted) {
                            heads.insert(path, head);
                        }
                    }
                }
            }
        }
        heads
    }

    /// The branch HEAD is on, or `None` when detached or unborn.    /// The branch HEAD is on, or `None` when detached or unborn.
    pub fn branch_name(&self) -> Option<String> {
        match self {
            Worktree::Local(_) => self.local().ok()?.branch_name(),
            Worktree::Remote { .. } => self
                .git_ok(&["symbolic-ref", "--short", "-q", "HEAD"])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// Whether a rebase is paused in this tree.
    pub fn rebase_in_progress(&self) -> bool {
        match self {
            Worktree::Local(_) => self
                .local()
                .map(|git| git.rebase_in_progress())
                .unwrap_or(false),
            Worktree::Remote { files, path, .. } => {
                let git_dir = self
                    .git_ok(&["rev-parse", "--git-dir"])
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| ".git".into());
                let git_dir = if Path::new(&git_dir).is_absolute() {
                    PathBuf::from(git_dir)
                } else {
                    path.join(git_dir)
                };
                files.exists(&git_dir.join("rebase-merge"))
                    || files.exists(&git_dir.join("rebase-apply"))
            }
        }
    }

    /// Stage one path — an addition, a change, or a deletion.
    pub fn stage(&self, rel: &Path) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.stage(rel),
            Worktree::Remote { .. } => {
                self.git_ok(&["add", "-A", "--", &rel.display().to_string()])?;
                Ok(())
            }
        }
    }

    /// Take one path back out of the index.
    pub fn unstage(&self, rel: &Path) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.unstage(rel),
            Worktree::Remote { .. } => {
                let rel = rel.display().to_string();
                if self.git_ok(&["reset", "-q", "--", &rel]).is_err() {
                    // An unborn branch has no HEAD to reset to; unstaging
                    // means leaving the index.
                    self.git_ok(&["rm", "-q", "--cached", "--", &rel])?;
                }
                Ok(())
            }
        }
    }

    /// Discard one path's changes: the index's version back in the
    /// working tree (`git restore --worktree`), so what is staged stays
    /// staged and only the edits on top of it go — the words the Discard
    /// panel uses. A conflicted path has no one version in the index, and
    /// `restore` refuses it; there HEAD's version goes back over the index
    /// and the file both, which resolves it (`git checkout HEAD -- path`,
    /// which every Discard used to be). A path the index does not have is
    /// left alone, as libgit2 leaves it.
    pub fn restore_file(&self, rel: &Path) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.restore_file(rel),
            Worktree::Remote { .. } => {
                let rel = rel.display().to_string();
                let unmerged = !self
                    .git_ok(&["ls-files", "-u", "--", &rel])?
                    .trim()
                    .is_empty();
                let args: &[&str] = if unmerged {
                    &["checkout", "-q", "HEAD", "--", &rel]
                } else {
                    &["restore", "--worktree", "--", &rel]
                };
                let out = self.run_git(args, &[])?;
                if !out.success() && !out.stderr_utf8().contains("did not match") {
                    bail!("{}", out.stderr_utf8().trim());
                }
                Ok(())
            }
        }
    }

    /// Commit the index. Returns the new commit's id.
    pub fn commit(&self, message: &str) -> Result<String> {
        let id = match self {
            Worktree::Local(_) => self.local()?.commit(message)?.to_string(),
            Worktree::Remote { .. } => {
                self.git_ok(&["commit", "-q", "-m", message])?;
                self.git_ok(&["rev-parse", "HEAD"])?.trim().to_string()
            }
        };
        self.ref_changed();
        Ok(id)
    }

    /// HEAD's content of one path, when it is text.
    pub fn head_content(&self, rel: &Path) -> Option<String> {
        match self {
            Worktree::Local(_) => self.local().ok()?.head_content(rel),
            Worktree::Remote { .. } => {
                let out = self
                    .run_git(&["show", &format!("HEAD:{}", rel.display())], &[])
                    .ok()?;
                if !out.success() {
                    return None;
                }
                String::from_utf8(out.stdout).ok()
            }
        }
    }

    /// The staged diff, as a patch, cut at `max_bytes`.
    pub fn staged_diff(&self, max_bytes: usize) -> Result<String> {
        match self {
            Worktree::Local(_) => self.local()?.staged_diff(max_bytes),
            Worktree::Remote { .. } => {
                let mut text = self.git_ok(&["diff", "--cached", "--no-color"])?;
                if text.len() > max_bytes {
                    let mut cut = max_bytes;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    text.push_str("\n… (diff truncated)\n");
                }
                Ok(text)
            }
        }
    }

    pub fn switch_branch(&self, name: &str) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.switch_branch(name)?,
            Worktree::Remote { .. } => {
                self.git_ok(&["switch", "-q", name])?;
            }
        }
        self.ref_changed();
        Ok(())
    }

    pub fn create_branch(&self, name: &str) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.create_branch(name)?,
            Worktree::Remote { .. } => {
                self.git_ok(&["switch", "-q", "-c", name])?;
            }
        }
        self.ref_changed();
        Ok(())
    }

    /// The stash entries, newest first, each named by its commit and
    /// holding its paths. Every stash verb here takes that id rather than
    /// a position: `stash@{n}` is true only at the moment it is read, and
    /// an entry pushed in between — the user's terminal, an agent's shell
    /// in the same checkout — moves every one below it down.
    pub fn stash_entries(&self) -> Result<Vec<StashEntry>> {
        match self {
            Worktree::Local(_) => self.local()?.stash_entries(),
            Worktree::Remote { .. } => {
                let list = self.git_ok(&["stash", "list", "--format=%H"])?;
                let mut entries = Vec::new();
                for id in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    // `stash show` takes any commit shaped like a stash.
                    let names = self.git_ok(&[
                        "stash",
                        "show",
                        "--name-only",
                        "--include-untracked",
                        "--format=",
                        id,
                    ])?;
                    entries.push(StashEntry {
                        id: id.to_string(),
                        paths: names
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(PathBuf::from)
                            .collect(),
                    });
                }
                Ok(entries)
            }
        }
    }

    /// Every path any stash entry holds.
    pub fn stashed_paths(&self) -> Result<HashSet<PathBuf>> {
        Ok(self
            .stash_entries()?
            .into_iter()
            .flat_map(|entry| entry.paths)
            .collect())
    }

    /// Stash one path, tracked or not, under `message`.
    pub fn stash_file(&self, rel: &Path, message: &str) -> Result<()> {
        self.git_ok(&[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            message,
            "--",
            &rel.display().to_string(),
        ])?;
        Ok(())
    }

    /// Bring one path back out of the stash entry `id` names: tracked
    /// content is in the stash commit, untracked in its third parent, and
    /// the first that has the path wins. `git restore --worktree`, not
    /// `git checkout <tree> -- <path>`, which writes the index too and so
    /// handed a stashed file back as staged (David, 2026-09-08:
    /// "unstash/unstage might be buggy"). A path the entry holds as a
    /// deletion comes back as one: `restore` takes a tracked path the
    /// source lacks out of the working tree.
    pub fn unstash_file(&self, id: &str, rel: &Path) -> Result<()> {
        let rel = rel.display().to_string();
        let mut last = String::new();
        for source in [id.to_string(), format!("{id}^3")] {
            match self.git_ok(&[
                "restore",
                &format!("--source={source}"),
                "--worktree",
                "--",
                &rel,
            ]) {
                Ok(_) => return Ok(()),
                Err(e) => last = format!("{e:#}"),
            }
        }
        bail!("{last}")
    }

    /// Take one path out of the stash entry `id` names, leaving the entry
    /// holding the rest — or dropping it, when that path was all it held.
    ///
    /// A stash entry is three commits (the working tree's, the index's,
    /// and the untracked files'), and none of them can be edited, so the
    /// entry is rebuilt: each tree with the path put back to the entry's
    /// base (or out of the untracked tree), the three commits made again,
    /// and the new entry stored in the old one's place with its message.
    /// Git's own plumbing throughout, through a throwaway index, so the
    /// working tree and the real index are never touched. The rebuilt
    /// entry lands at the top of the stash list, as `git stash store`
    /// puts it, and under a new id: a caller with more to do re-reads the
    /// list. Everything is read from the commit `id` names; only the drop
    /// needs a position, and reads it last (`stash_drop`).
    pub fn remove_from_stash(&self, id: &str, rel: &Path) -> Result<()> {
        let paths = self
            .stash_entries()?
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.paths)
            .ok_or_else(|| anyhow::anyhow!("stash entry {id} is no longer in the list"))?;
        if paths.len() <= 1 {
            return self.stash_drop(id);
        }
        let rel = rel.display().to_string();
        let rev = |spec: &str| -> Option<String> {
            self.git_ok(&["rev-parse", "-q", "--verify", spec])
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let stash = id.to_string();
        let base = rev(&format!("{stash}^1")).ok_or_else(|| anyhow::anyhow!("{id} has no base"))?;
        let staged = rev(&format!("{stash}^2"));
        let untracked = rev(&format!("{stash}^3"));
        // Its message, as the list shows it, off the reflog line that
        // names this commit.
        let message = self
            .git_ok(&["stash", "list", "--format=%H%x09%gs"])?
            .lines()
            .find_map(|line| line.strip_prefix(&stash)?.strip_prefix('\t'))
            .unwrap_or_default()
            .trim()
            .to_string();
        let git_dir = self.git_ok(&["rev-parse", "--absolute-git-dir"])?;
        let index_file = format!("{}/taste-stash-edit.index", git_dir.trim());
        let with_index = [("GIT_INDEX_FILE".to_string(), index_file)];
        let ok = |args: &[&str]| -> Result<String> {
            let out = self.run_git(args, &with_index)?;
            if !out.success() {
                bail!("{}", out.stderr_utf8().trim());
            }
            Ok(out.stdout_utf8().trim().to_string())
        };
        // The tree of `commit` with `rel` as `base` has it, or gone.
        let without = |commit: &str, from_base: bool| -> Result<String> {
            ok(&["read-tree", commit])?;
            let listed = if from_base {
                ok(&["ls-tree", &base, "--", &rel])?
            } else {
                String::new()
            };
            match listed.split_once('\t') {
                Some((meta, _)) => {
                    let mut meta = meta.split_whitespace();
                    let (mode, _, blob) = (meta.next(), meta.next(), meta.next());
                    let (Some(mode), Some(blob)) = (mode, blob) else {
                        bail!("reading {rel} in the stash's base");
                    };
                    ok(&[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("{mode},{blob},{rel}"),
                    ])?;
                }
                None => {
                    ok(&["update-index", "--force-remove", "--", &rel])?;
                }
            }
            ok(&["write-tree"])
        };
        let work_tree = without(&stash, true)?;
        let mut parents = vec![base.clone()];
        if let Some(staged) = &staged {
            let tree = without(staged, true)?;
            parents.push(ok(&[
                "commit-tree",
                &tree,
                "-p",
                &base,
                "-m",
                &format!("index on {message}"),
            ])?);
        }
        if let Some(untracked) = &untracked {
            let tree = without(untracked, false)?;
            let empty = ok(&["hash-object", "-t", "tree", "/dev/null"])?;
            if tree != empty {
                parents.push(ok(&[
                    "commit-tree",
                    &tree,
                    "-m",
                    &format!("untracked files on {message}"),
                ])?);
            }
        }
        let mut args: Vec<String> = vec!["commit-tree".into(), work_tree];
        for parent in &parents {
            args.push("-p".into());
            args.push(parent.clone());
        }
        args.push("-m".into());
        args.push(message.clone());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let rebuilt = ok(&args)?;
        self.stash_drop(id)?;
        self.git_ok(&["stash", "store", "-q", "-m", &message, &rebuilt])?;
        Ok(())
    }

    /// Drop the stash entry `id` names. Git drops by position, and a
    /// position is true only at the moment it is read, so it is read here,
    /// from the list, right before the drop — and an entry the list no
    /// longer has is refused, never guessed at. The window between that
    /// read and the drop is git's own: it has no drop-by-commit.
    pub fn stash_drop(&self, id: &str) -> Result<()> {
        let list = self.git_ok(&["stash", "list", "--format=%H"])?;
        let position = list
            .lines()
            .map(str::trim)
            .position(|line| line == id)
            .ok_or_else(|| anyhow::anyhow!("stash entry {id} is no longer in the list"))?;
        self.git_ok(&["stash", "drop", "--quiet", &format!("stash@{{{position}}}")])?;
        Ok(())
    }

    /// Rebase the current branch onto `onto`, autostashing. The peer is
    /// what knows the upstream; the caller names the ref it wants — in a
    /// VM that is a `refs/remotes/*` the registry pushed over.
    pub fn rebase_onto(&self, onto: &str) -> Result<()> {
        self.git_ok(&["rebase", "--autostash", onto])?;
        self.ref_changed();
        Ok(())
    }

    pub fn rebase_abort(&self) -> Result<()> {
        self.git_ok(&["rebase", "--abort"])?;
        self.ref_changed();
        Ok(())
    }

    pub fn rebase_continue(&self) -> Result<()> {
        self.git_ok(&["-c", "core.editor=true", "rebase", "--continue"])?;
        self.ref_changed();
        Ok(())
    }
}

/// Where a submodule is ([`Worktree::submodule_heads`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleHead {
    /// The branch its HEAD is on, else a tag at that exact commit, else
    /// the commit, short.
    pub at: String,
    /// The commit the parent records for it — the pin — short. What the
    /// file tree leads with, whatever branch happens to be there (David,
    /// 2026-10-06: "I want to see what it's pinned to, even if it's
    /// equivalent to a branch").
    pub pinned: String,
    /// How that stands to the commit the parent records.
    pub pin: SubmodulePin,
}

/// A submodule's commit against the one its parent records — the pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmodulePin {
    /// At it.
    Same,
    /// Past it, by this many commits: work in the submodule the parent has
    /// not recorded yet, which is the ordinary state of working in one.
    Ahead(u32),
    /// Short of it, by this many: the parent expects commits it has not got.
    Behind(u32),
    /// Neither contains the other.
    Diverged,
    /// The recorded commit is not in the submodule's clone: not fetched.
    Missing,
    /// Not checked out at all.
    NotCheckedOut,
    /// The parent's own merge or rebase disagrees about it.
    Conflicted,
}

impl SubmodulePin {
    /// Whether this is something to fix rather than the way things are
    /// while working: at the pin, or past it, is not.
    pub fn is_problem(&self) -> bool {
        !matches!(self, SubmodulePin::Same | SubmodulePin::Ahead(_))
    }

    /// The sentence for it, after the submodule's name.
    pub fn describe(&self) -> String {
        let commits = |n: u32| {
            if n == 1 {
                "1 commit".to_string()
            } else {
                format!("{n} commits")
            }
        };
        match self {
            SubmodulePin::Same => "at the commit the parent records".into(),
            SubmodulePin::Ahead(n) => format!(
                "{} past the commit the parent records — committing the parent records it",
                commits(*n)
            ),
            SubmodulePin::Behind(n) => format!(
                "{} short of the commit the parent records — the parent expects work this \
                 checkout does not have",
                commits(*n)
            ),
            SubmodulePin::Diverged => "diverged from the commit the parent records — \
                 neither contains the other"
                .into(),
            SubmodulePin::Missing => "the commit the parent records is not in this clone — \
                 fetch the submodule"
                .into(),
            SubmodulePin::NotCheckedOut => "not checked out".into(),
            SubmodulePin::Conflicted => "conflicted in the parent's merge or rebase".into(),
        }
    }
}

/// `git submodule foreach` over the checked-out submodules: each one's
/// path, what its HEAD is — the branch, else a tag at that exact commit,
/// else the commit, short — and how it stands to the commit the parent
/// records (`$sha1`), tab-separated.
const SUBMODULE_NAMES: [&str; 4] = [
    "submodule",
    "foreach",
    "--quiet",
    r#"head=$(git rev-parse HEAD)
if [ "$head" = "$sha1" ]; then pin=same
elif ! git cat-file -e "$sha1^{commit}" 2>/dev/null; then pin=missing
elif git merge-base --is-ancestor "$sha1" HEAD; then pin="ahead $(git rev-list --count "$sha1..HEAD")"
elif git merge-base --is-ancestor HEAD "$sha1"; then pin="behind $(git rev-list --count "HEAD..$sha1")"
else pin=diverged
fi
printf '%s\t%s\t%s\t%s\n' "$sm_path" "$(git symbolic-ref -q --short HEAD || git describe --tags --exact-match HEAD 2>/dev/null || git rev-parse --short HEAD)" "$pin" "$(printf %.7s "$sha1")""#,
];

/// `git submodule status`, read as each submodule's path and its commit,
/// short. A line is a state flag (` `, `-` not checked out, `+` at another
/// commit than the parent records, `U` conflicted), the commit, the path,
/// and, for one checked out, ` (describe)`.
fn submodule_status_heads(out: &str) -> HashMap<PathBuf, SubmoduleHead> {
    out.lines()
        .filter_map(|line| {
            let flag = line.chars().next()?;
            let rest = line.get(1..)?;
            let (sha, path) = rest.split_once(' ')?;
            let path = match path.rfind(" (") {
                Some(at) if path.ends_with(')') => &path[..at],
                _ => path,
            };
            let pin = match flag {
                '-' => SubmodulePin::NotCheckedOut,
                'U' => SubmodulePin::Conflicted,
                _ => SubmodulePin::Same,
            };
            // The commit shown is the submodule's own, which is the pin
            // only until it is checked out and moves; the walk over the
            // checked-out ones says the pin (`$sha1`).
            let short: String = sha.chars().take(7).collect();
            Some((
                PathBuf::from(path),
                SubmoduleHead {
                    at: short.clone(),
                    pinned: short,
                    pin,
                },
            ))
        })
        .collect()
}

/// [`SUBMODULE_NAMES`]'s output: each checked-out submodule's path, name,
/// and standing against its pin.
fn submodule_names(out: &str) -> HashMap<PathBuf, SubmoduleHead> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let (path, at, pin) = (fields.next()?, fields.next()?, fields.next()?);
            let pinned = fields.next().unwrap_or_default().trim().to_string();
            let count = |word: &str| {
                pin.strip_prefix(word)
                    .and_then(|n| n.trim().parse::<u32>().ok())
            };
            let pin = match pin.trim() {
                "same" => SubmodulePin::Same,
                "missing" => SubmodulePin::Missing,
                "diverged" => SubmodulePin::Diverged,
                _ => match (count("ahead "), count("behind ")) {
                    (Some(n), _) => SubmodulePin::Ahead(n),
                    (_, Some(n)) => SubmodulePin::Behind(n),
                    _ => SubmodulePin::Same,
                },
            };
            Some((
                PathBuf::from(path),
                SubmoduleHead {
                    at: at.trim().to_string(),
                    pinned,
                    pin,
                },
            ))
        })
        .filter(|(_, head)| !head.at.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standing against the pin, from git itself: past the recorded
    /// commit after a commit in the submodule, short of it once the parent
    /// records a commit the submodule has gone back from.
    #[test]
    fn a_submodule_is_measured_against_the_commit_its_parent_records() {
        let dir = std::env::temp_dir().join(format!("taste-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q", "-b", "main"]);
        std::fs::write(sub.join("a"), "1").unwrap();
        git(&sub, &["add", "-A"]);
        git(&sub, &["commit", "-q", "-m", "one"]);
        let parent = dir.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["init", "-q", "-b", "main"]);
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "tpl",
            ],
        );
        git(&parent, &["commit", "-q", "-m", "add"]);
        let tpl = parent.join("tpl");
        let worktree = Worktree::Local(parent.clone());
        let head = |w: &Worktree| w.submodule_heads()[Path::new("tpl")].clone();
        let at_pin = head(&worktree);
        assert_eq!(at_pin.pin, SubmodulePin::Same);
        let pinned = git(&parent, &["rev-parse", "--short=7", "HEAD:tpl"]);
        assert_eq!(
            (at_pin.at.as_str(), at_pin.pinned.as_str()),
            ("main", pinned.as_str())
        );

        std::fs::write(tpl.join("a"), "2").unwrap();
        git(&tpl, &["commit", "-q", "-am", "two"]);
        let ahead = head(&worktree);
        assert_eq!(
            (ahead.at.as_str(), &ahead.pin),
            ("main", &SubmodulePin::Ahead(1))
        );

        git(&parent, &["add", "tpl"]);
        git(&parent, &["commit", "-q", "-m", "record two"]);
        git(&tpl, &["checkout", "-q", "HEAD~1"]);
        assert_eq!(head(&worktree).pin, SubmodulePin::Behind(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A submodule's place, as the file tree writes it: its branch, a tag,
    /// or its commit; and one not checked out at the commit its parent
    /// records.
    #[test]
    fn submodule_heads_read_back_from_git() {
        let status =
            " a1d4e5a0c3b2f1e0d9c8b7a6f5e4d3c2b1a09f8e amutable-typst-template (heads/main)\n\
                      -97368d91cd4e37e94d0db0817fde54f9b713ac17 vendor/empty\n\
                      +ca2618d00000000000000000000000000000000 deps/my lib (v1.2)\n";
        let heads = submodule_status_heads(status);
        assert_eq!(heads[Path::new("amutable-typst-template")].at, "a1d4e5a");
        assert_eq!(heads[Path::new("vendor/empty")].at, "97368d9");
        assert_eq!(
            heads[Path::new("vendor/empty")].pin,
            SubmodulePin::NotCheckedOut
        );
        assert_eq!(heads[Path::new("deps/my lib")].at, "ca2618d");
        let names = submodule_names(
            "amutable-typst-template\tmain\tahead 1\t37cf0ad\ndeps/my lib\tv1.2\tbehind 3\tca2618d\nx\tabc1234\tmissing\t1111111\n",
        );
        let tpl = &names[Path::new("amutable-typst-template")];
        assert_eq!(tpl.pinned, "37cf0ad");
        assert_eq!(
            (tpl.at.as_str(), &tpl.pin),
            ("main", &SubmodulePin::Ahead(1))
        );
        assert!(
            !tpl.pin.is_problem(),
            "past the pin is working, not a problem"
        );
        assert_eq!(names[Path::new("deps/my lib")].pin, SubmodulePin::Behind(3));
        assert!(names[Path::new("x")].pin.is_problem());
    }

    fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn repo_with_commit(root: &Path) -> git2::Repository {
        let repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        {
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("t", "t@t").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
                .unwrap();
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "t").unwrap();
            config.set_str("user.email", "t@t").unwrap();
        }
        repo
    }

    #[test]
    fn one_file_leaves_a_stash_entry_and_the_rest_stay_in_it() {
        if !git_present() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_commit(root);
        // Both arms: the VM's is `git` through the files service.
        let worktree = Worktree::Remote {
            files: Files::Local,
            path: root.to_path_buf(),
            after_ref_change: None,
        };
        std::fs::write(root.join("a.txt"), "a changed\n").unwrap();
        std::fs::write(root.join("b.txt"), "b changed\n").unwrap();
        std::fs::write(root.join("c.txt"), "untracked\n").unwrap();
        // What the checkout is besides the stash — its index and its files —
        // none of which may move. A rebuild that ran against the real index
        // left every file staged for deletion, and a check of the stash and
        // of one file on disk did not see it.
        std::fs::write(root.join("d.txt"), "kept, untracked\n").unwrap();
        let state = |worktree: &Worktree| {
            (
                worktree.git_ok(&["status", "--porcelain"]).unwrap(),
                worktree.git_ok(&["ls-files", "--stage"]).unwrap(),
            )
        };
        worktree
            .stash_paths(
                &[
                    PathBuf::from("a.txt"),
                    PathBuf::from("b.txt"),
                    PathBuf::from("c.txt"),
                ],
                "taste-ide selection",
            )
            .unwrap();
        let held = |worktree: &Worktree| -> Vec<StashEntry> { worktree.stash_entries().unwrap() };
        assert_eq!(held(&worktree)[0].paths.len(), 3);
        let before = state(&worktree);

        // A tracked file out: the others stay, as stashed.
        worktree
            .remove_from_stash(&held(&worktree)[0].id, Path::new("a.txt"))
            .unwrap();
        let entries = held(&worktree);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].paths,
            [PathBuf::from("b.txt"), PathBuf::from("c.txt")]
                .into_iter()
                .collect()
        );
        assert_eq!(
            worktree.git_ok(&["show", "stash@{0}:b.txt"]).unwrap(),
            "b changed\n"
        );
        assert_eq!(state(&worktree), before, "the index or the files moved");
        // Its message kept, and the working tree untouched.
        assert!(worktree
            .git_ok(&["stash", "list"])
            .unwrap()
            .contains("taste-ide selection"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");

        // An untracked file out, then the last: the entry goes.
        worktree
            .remove_from_stash(&held(&worktree)[0].id, Path::new("c.txt"))
            .unwrap();
        assert_eq!(
            held(&worktree)[0].paths,
            [PathBuf::from("b.txt")].into_iter().collect()
        );
        worktree
            .remove_from_stash(&held(&worktree)[0].id, Path::new("b.txt"))
            .unwrap();
        assert!(held(&worktree).is_empty());
        assert_eq!(state(&worktree), before, "the index or the files moved");
    }

    /// The two arms agree, verb by verb, on one working tree: the remote
    /// arm here is the local files service running `git`, which is the
    /// keeper's shape with the VM taken out.
    #[test]
    fn the_remote_arm_answers_like_libgit2() {
        if !git_present() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_commit(root);
        let local = Worktree::Local(root.to_path_buf());
        let remote = Worktree::Remote {
            files: Files::Local,
            path: root.to_path_buf(),
            after_ref_change: None,
        };
        assert_eq!(local.branch_name(), remote.branch_name());
        assert!(local.branch_name().is_some());

        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        assert_eq!(local.status().unwrap(), remote.status().unwrap());
        assert_eq!(
            remote.status().unwrap()[Path::new("a.txt")],
            FileState::Modified
        );
        assert_eq!(
            remote.status().unwrap()[Path::new("new.txt")],
            FileState::Untracked
        );
        assert_eq!(
            remote.status().unwrap()[Path::new("b.txt")],
            FileState::Modified
        );

        assert_eq!(remote.head_content(Path::new("a.txt")), Some("a\n".into()));
        assert_eq!(remote.head_content(Path::new("nope")), None);
        assert_eq!(local.workdir(), remote.workdir());

        // What git ignores, asked of a listing's names: the ignored one,
        // never a tracked one, and an empty ask is no ask.
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("build.log"), "x\n").unwrap();
        let names: Vec<PathBuf> = ["a.txt", "build.log", "new.txt"]
            .iter()
            .map(|n| root.join(n))
            .collect();
        let ignored = remote.ignored(&names).unwrap();
        assert_eq!(ignored, HashSet::from([root.join("build.log")]));
        assert!(remote.ignored(&[]).unwrap().is_empty());
        std::fs::remove_file(root.join(".gitignore")).unwrap();
        std::fs::remove_file(root.join("build.log")).unwrap();

        remote.stage(Path::new("a.txt")).unwrap();
        remote.stage(Path::new("b.txt")).unwrap();
        assert_eq!(
            remote.status().unwrap()[Path::new("a.txt")],
            FileState::Staged
        );
        assert_eq!(
            remote.status().unwrap()[Path::new("b.txt")],
            FileState::Staged
        );
        let diff = remote.staged_diff(10_000).unwrap();
        assert!(diff.contains("-a") && diff.contains("+changed"), "{diff}");
        assert!(remote.staged_diff(8).unwrap().contains("truncated"));

        remote.unstage(Path::new("b.txt")).unwrap();
        assert_eq!(
            remote.status().unwrap()[Path::new("b.txt")],
            FileState::Modified
        );
        remote.restore_file(Path::new("b.txt")).unwrap();
        assert!(root.join("b.txt").exists(), "HEAD's b.txt is back");
        remote.restore_file(Path::new("new.txt")).unwrap();
        assert!(
            root.join("new.txt").exists(),
            "an untracked file is left alone"
        );

        let changed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hooked = remote.clone().with_after_ref_change({
            let changed = changed.clone();
            Arc::new(move || {
                changed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        });
        let id = hooked.commit("staged a").unwrap();
        assert_eq!(id.len(), 40);
        assert_eq!(changed.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Several paths stashed as one entry, and back.
        std::fs::write(root.join("s1.txt"), "1\n").unwrap();
        std::fs::write(root.join("s2.txt"), "2\n").unwrap();
        remote
            .stash_paths(&[PathBuf::from("s1.txt"), PathBuf::from("s2.txt")], "two")
            .unwrap();
        assert!(!root.join("s1.txt").exists() && !root.join("s2.txt").exists());
        let entries = remote.stash_entries().unwrap();
        assert!(
            entries[0].paths.contains(Path::new("s1.txt"))
                && entries[0].paths.contains(Path::new("s2.txt"))
        );
        remote
            .unstash_file(&entries[0].id, Path::new("s2.txt"))
            .unwrap();
        assert!(root.join("s2.txt").exists());
        remote.stash_drop(&entries[0].id).unwrap();
        assert!(remote.stash_entries().unwrap().is_empty());
        let _ = std::fs::remove_file(root.join("s2.txt"));
        assert_eq!(
            local.status().unwrap()[Path::new("new.txt")],
            FileState::Untracked
        );
        assert!(!local.status().unwrap().contains_key(Path::new("a.txt")));

        hooked.create_branch("feature").unwrap();
        assert_eq!(remote.branch_name().as_deref(), Some("feature"));
        hooked
            .switch_branch(local.branch_name().unwrap().as_str())
            .unwrap();
        assert_eq!(changed.load(std::sync::atomic::Ordering::SeqCst), 3);

        // Stash: one path in, listed, back out, dropped.
        remote.stash_file(Path::new("new.txt"), "keep new").unwrap();
        assert!(!root.join("new.txt").exists());
        let entries = remote.stash_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].paths.contains(Path::new("new.txt")));
        assert_eq!(
            remote.stash_entries().unwrap(),
            local.stash_entries().unwrap()
        );
        remote
            .unstash_file(&entries[0].id, Path::new("new.txt"))
            .unwrap();
        assert!(root.join("new.txt").exists());
        remote.stash_drop(&entries[0].id).unwrap();
        assert!(remote.stash_entries().unwrap().is_empty());
        assert!(!remote.rebase_in_progress());
        assert!(!local.rebase_in_progress());
    }

    fn remote_at(root: &Path) -> Worktree {
        Worktree::Remote {
            files: Files::Local,
            path: root.to_path_buf(),
            after_ref_change: None,
        }
    }

    /// The list moved under the verb: an entry pushed after the list was
    /// read — from a terminal, from an agent's shell in the same checkout —
    /// sits above every entry read, and a drop by the position read first
    /// would take the wrong one. Named by commit, the verbs find theirs
    /// wherever it now sits, and refuse one that is gone.
    #[test]
    fn a_stash_pushed_meanwhile_does_not_move_the_drop() {
        if !git_present() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_commit(root);
        let worktree = remote_at(root);
        std::fs::write(root.join("a.txt"), "a changed\n").unwrap();
        std::fs::write(root.join("c.txt"), "untracked\n").unwrap();
        worktree
            .stash_paths(&[PathBuf::from("a.txt"), PathBuf::from("c.txt")], "first")
            .unwrap();
        let first = worktree.stash_entries().unwrap()[0].clone();
        // Read, then pushed over: `first` is stash@{1} now.
        std::fs::write(root.join("b.txt"), "b changed\n").unwrap();
        worktree.stash_file(Path::new("b.txt"), "second").unwrap();
        let second = worktree.stash_entries().unwrap()[0].clone();
        assert_ne!(first.id, second.id);

        // The rebuild finds its entry: `second` untouched, `first` rebuilt
        // without a.txt.
        worktree
            .remove_from_stash(&first.id, Path::new("a.txt"))
            .unwrap();
        let entries = worktree.stash_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|entry| entry.id == second.id
                && entry.paths == HashSet::from([PathBuf::from("b.txt")])));
        let rebuilt = entries
            .iter()
            .find(|entry| entry.id != second.id)
            .unwrap()
            .clone();
        assert_eq!(rebuilt.paths, HashSet::from([PathBuf::from("c.txt")]));
        assert!(worktree
            .git_ok(&["stash", "list"])
            .unwrap()
            .contains("first"));

        // The drop finds its entry, wherever it now sits.
        worktree.stash_drop(&rebuilt.id).unwrap();
        let entries = worktree.stash_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, second.id);
        // An entry the list no longer has is refused, not guessed at.
        assert!(worktree.stash_drop(&rebuilt.id).is_err());
        assert!(worktree
            .remove_from_stash(&first.id, Path::new("c.txt"))
            .is_err());
        assert_eq!(worktree.stash_entries().unwrap().len(), 1);
    }

    /// A path stashed as a deletion comes back as one.
    #[test]
    fn a_stashed_deletion_comes_back_as_a_deletion() {
        if !git_present() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_commit(root);
        let worktree = remote_at(root);
        std::fs::remove_file(root.join("a.txt")).unwrap();
        worktree.stash_file(Path::new("a.txt"), "gone").unwrap();
        assert!(root.join("a.txt").exists(), "the stash put HEAD's back");
        let entry = worktree.stash_entries().unwrap()[0].clone();
        assert_eq!(entry.paths, HashSet::from([PathBuf::from("a.txt")]));
        worktree
            .unstash_file(&entry.id, Path::new("a.txt"))
            .unwrap();
        assert!(!root.join("a.txt").exists(), "the deletion is back");
        worktree
            .remove_from_stash(&entry.id, Path::new("a.txt"))
            .unwrap();
        assert!(worktree.stash_entries().unwrap().is_empty());
    }

    /// Discard takes the edits on top of what is staged and leaves the
    /// staged part staged, in both arms — and on a conflicted path, which
    /// `git restore` refuses, puts HEAD's version back, resolved.
    #[test]
    fn discard_keeps_what_is_staged_and_resolves_a_conflict() {
        if !git_present() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_commit(root);
        let local = Worktree::Local(root.to_path_buf());
        let remote = remote_at(root);
        for (arm, file, committed) in [(&local, "a.txt", "a\n"), (&remote, "b.txt", "b\n")] {
            let path = root.join(file);
            std::fs::write(&path, "staged\n").unwrap();
            arm.stage(Path::new(file)).unwrap();
            std::fs::write(&path, "staged\nand more\n").unwrap();
            arm.restore_file(Path::new(file)).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "staged\n",
                "{arm:?}"
            );
            assert_eq!(
                arm.status().unwrap()[Path::new(file)],
                FileState::Staged,
                "{arm:?}"
            );
            // Nothing staged: HEAD's version, as before.
            arm.unstage(Path::new(file)).unwrap();
            arm.restore_file(Path::new(file)).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                committed,
                "{arm:?}"
            );
            assert!(!arm.status().unwrap().contains_key(Path::new(file)));
        }

        // A conflict on a.txt, once per arm.
        let main = local.branch_name().unwrap();
        for (arm, n) in [(&remote, 1), (&local, 2)] {
            let side = format!("side{n}");
            remote.git_ok(&["switch", "-q", "-c", &side]).unwrap();
            std::fs::write(root.join("a.txt"), format!("side {n}\n")).unwrap();
            remote.git_ok(&["commit", "-q", "-am", "side"]).unwrap();
            remote.git_ok(&["switch", "-q", &main]).unwrap();
            std::fs::write(root.join("a.txt"), format!("main {n}\n")).unwrap();
            remote.git_ok(&["commit", "-q", "-am", "main"]).unwrap();
            assert!(!remote.run_git(&["merge", &side], &[]).unwrap().success());
            assert_eq!(
                arm.status().unwrap()[Path::new("a.txt")],
                FileState::Conflicted,
                "{arm:?}"
            );
            arm.restore_file(Path::new("a.txt")).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                format!("main {n}\n"),
                "{arm:?}"
            );
            assert!(
                !arm.status().unwrap().contains_key(Path::new("a.txt")),
                "resolved, at HEAD: {arm:?}"
            );
            remote.git_ok(&["merge", "--abort"]).unwrap();
        }
    }

    #[test]
    fn a_worktree_is_made_from_a_checkout() {
        let local = Worktree::for_checkout(&Checkout::Local("/w".into()), Files::Local);
        assert!(local.is_local());
        assert_eq!(local.root(), Path::new("/w"));
        let remote = Worktree::for_checkout(
            &Checkout::Remote {
                vm: "taste-x".into(),
                path: "/var/home/core/taste/x/primary".into(),
            },
            Files::unavailable("not connected"),
        );
        assert!(!remote.is_local());
        assert_eq!(remote.root(), Path::new("/var/home/core/taste/x/primary"));
        assert!(remote.status().is_err(), "no service, no answer");
    }
}
