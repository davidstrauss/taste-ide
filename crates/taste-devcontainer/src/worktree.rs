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
use taste_git::{FileState, GitWorkspace};

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
    pub fn run_git(&self, args: &[&str], envs: &[(String, String)]) -> Result<ExecOutput> {
        match self {
            Worktree::Local(path) => {
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(path)
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
                let mut argv: Vec<String> = vec!["git".into()];
                argv.extend(args.iter().map(|s| s.to_string()));
                let mut env: Vec<(String, String)> = taste_git::non_interactive_env();
                env.extend(envs.iter().cloned());
                // The keeper's exec takes an environment when it is one
                // (`Keeper::exec_with_env`); through `Files` it takes none,
                // so git's own `-c` carries what matters.
                let _ = env;
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
            bail!("{last}");
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
                Ok(taste_git::status_from_porcelain(&out))
            }
        }
    }

    /// The branch HEAD is on, or `None` when detached or unborn.
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

    /// Discard one path's changes: HEAD's version back in the working
    /// tree. A path HEAD does not have is left alone, as libgit2 leaves it.
    pub fn restore_file(&self, rel: &Path) -> Result<()> {
        match self {
            Worktree::Local(_) => self.local()?.restore_file(rel),
            Worktree::Remote { .. } => {
                let out = self.run_git(
                    &["checkout", "-q", "HEAD", "--", &rel.display().to_string()],
                    &[],
                )?;
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

    /// The paths every stash entry holds, entry by entry, newest first.
    pub fn stash_entries(&self) -> Result<Vec<HashSet<PathBuf>>> {
        match self {
            Worktree::Local(_) => self.local()?.stash_entries(),
            Worktree::Remote { .. } => {
                let list = self.git_ok(&["stash", "list", "--format=%gd"])?;
                let mut entries = Vec::new();
                for entry in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let names = self.git_ok(&[
                        "stash",
                        "show",
                        "--name-only",
                        "--include-untracked",
                        "--format=",
                        entry,
                    ])?;
                    entries.push(
                        names
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(PathBuf::from)
                            .collect(),
                    );
                }
                Ok(entries)
            }
        }
    }

    /// Every path any stash entry holds.
    pub fn stashed_paths(&self) -> Result<HashSet<PathBuf>> {
        Ok(self.stash_entries()?.into_iter().flatten().collect())
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

    /// Bring one path back out of stash entry `index`: tracked content is
    /// in the stash commit, untracked in its third parent, and the first
    /// that has the path wins.
    pub fn unstash_file(&self, index: usize, rel: &Path) -> Result<()> {
        let rel = rel.display().to_string();
        let mut last = String::new();
        for source in [format!("stash@{{{index}}}"), format!("stash@{{{index}}}^3")] {
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

    pub fn stash_drop(&self, index: usize) -> Result<()> {
        self.git_ok(&["stash", "drop", "--quiet", &format!("stash@{{{index}}}")])?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
            entries[0].contains(Path::new("s1.txt")) && entries[0].contains(Path::new("s2.txt"))
        );
        remote.unstash_file(0, Path::new("s2.txt")).unwrap();
        assert!(root.join("s2.txt").exists());
        remote.stash_drop(0).unwrap();
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
        assert!(entries[0].contains(Path::new("new.txt")));
        assert_eq!(
            remote.stashed_paths().unwrap(),
            local.stashed_paths().unwrap()
        );
        remote.unstash_file(0, Path::new("new.txt")).unwrap();
        assert!(root.join("new.txt").exists());
        remote.stash_drop(0).unwrap();
        assert!(remote.stash_entries().unwrap().is_empty());
        assert!(!remote.rebase_in_progress());
        assert!(!local.rebase_in_progress());
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
