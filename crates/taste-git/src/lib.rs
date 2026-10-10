//! Git as a property of the file tree, not a separate interface.
//!
//! The file tree asks this crate for per-path status, toggles staging on row
//! click, and drives commit/push from its header bar. That is the entire git
//! UI surface by design (see docs/ARCHITECTURE.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Repository, Status, StatusOptions};

pub mod beneath;
pub mod clone;
pub mod issues;
pub mod mediate;
pub mod merge;
/// Object ids, for callers that name commits without depending on git2.
pub use git2::Oid;
pub mod mirror;
pub mod presence;
pub mod private;
pub mod refs;
mod relation;
pub use relation::{record_remote_tip, PushTarget};
pub mod review;
pub mod snapshot;

pub use clone::strip_worktree;
pub use clone::{clone_local, unpublished_work, unshare_inodes, UnpublishedBranch};
pub use issues::{
    is_issue_id, random_issue_id, random_slug, Attachment, Claim, Comment, Issue, IssueChange,
    IssueLink, IssueMove, IssueState, IssueSync, LinkCheck, NewAttachment, Resolution,
    StartOutcome, ISSUES_ORDER_PATH, ISSUES_PUSH_REFSPEC, ISSUES_REF, ISSUES_TRACKING_REF,
};
pub use mediate::{PublishMode, PublishOutcome, PublishStatus, RefUpdate, HUB_UPDATE_REFSPECS};
pub use merge::{MergeOutcome, MergePolicy, MergeStatus};
pub use refs::{BranchInfo, BranchRelation, RefFile, RefTree, RefTreeEntry};
pub use review::{
    env_branch, env_branch_ref, env_of_branch, ChangeKind, ChangedFile, EnvBranch, Mergedness,
    PublishReadiness, RebasedPublish, ReviewBlobs, ENV_BRANCH_PREFIX,
};
pub use snapshot::{snapshot_ref, RestoreMode, Restored, Snapshot, SNAPSHOT_REF_PREFIX};

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
/// Who started an issue, as the issue store records it: the committer's
/// email from the host's git config, at this machine's hostname — the
/// two facts that tell one person's two machines apart, which is what the
/// record is for (a second machine starting the same issue is refused by
/// name). Falls back honestly when either is unknown.
pub fn starter_identity() -> String {
    let who = host_identity()
        .map(|id| id.email)
        .filter(|email| !email.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string());
    format!("{who}@{host}")
}

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

/// How the user's push treats submodules: push any commit the branch
/// records that a submodule's remote lacks, first, or push nothing
/// ([`GitWorkspace::push_command`]).
pub const PUSH_SUBMODULES: &str = "--recurse-submodules=on-demand";

/// `git status --porcelain=v1 -z --untracked-files=all`, as the same map
/// [`GitWorkspace::status`] gives — for a working copy this process cannot
/// open, where `git` runs beside the files and only its output comes here.
///
/// The two-letter code: `??` untracked, `!!` ignored, the conflict pairs
/// conflicted, anything staged in the index (`X`) staged whatever the
/// working tree adds, and a working-tree change alone (`Y`) modified. A
/// rename or copy carries the original path as the following record and it
/// is skipped: the state belongs to the new path.
pub fn status_from_porcelain(z_output: &str) -> HashMap<PathBuf, FileState> {
    let mut out = HashMap::new();
    let mut records = z_output.split('\0').filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let (code, path) = record.split_at(3);
        let code = &code[..2];
        let mut chars = code.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
            // The original path is the next record; the new one is here.
            let _ = records.next();
        }
        let state = match code {
            "??" => FileState::Untracked,
            "!!" => FileState::Ignored,
            "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU" => FileState::Conflicted,
            _ if x != ' ' => FileState::Staged,
            _ if y != ' ' => FileState::Modified,
            _ => continue,
        };
        out.insert(PathBuf::from(path), state);
    }
    out
}

/// What [`SUBMODULE_STATUS_ARGS`] prints, as the paths of the files inside
/// each submodule — `sub/dir/file.typ`, keyed from the parent's root as the
/// parent's own status is — so a file changed in a submodule is shown
/// where it is rather than only as its submodule, modified (David,
/// 2026-10-05: changes in a submodule "don't show up as detailed 'M'
/// items in the file viewer"). Each submodule's records follow a marker
/// record, `@@ <path>`, which no porcelain record can be: those begin
/// with a two-letter code and a space.
pub fn status_from_submodule_porcelain(z_output: &str) -> HashMap<PathBuf, FileState> {
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    for record in z_output.split('\0').filter(|r| !r.is_empty()) {
        match record.strip_prefix("@@ ") {
            Some(sub) => sections.push((sub, Vec::new())),
            None => {
                if let Some((_, records)) = sections.last_mut() {
                    records.push(record);
                }
            }
        }
    }
    let mut out = HashMap::new();
    for (sub, records) in sections {
        for (path, state) in status_from_porcelain(&records.join("\0")) {
            out.insert(Path::new(sub).join(path), state);
        }
    }
    out
}

/// `git` arguments that print every checked-out submodule's status, nested
/// ones included, for [`status_from_submodule_porcelain`].
pub const SUBMODULE_STATUS_ARGS: [&str; 5] = [
    "submodule",
    "foreach",
    "--quiet",
    "--recursive",
    r#"printf '@@ %s\0' "$displaypath"; git status --porcelain=v1 -z --untracked-files=all"#,
];

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

/// One stash entry, as the file tree's Stashed view and its verbs see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// The stash commit's id: how the entry is named to every verb, since
    /// its position in the list can move under a caller.
    pub id: String,
    /// The paths it holds, tracked changes and untracked files alike.
    pub paths: std::collections::HashSet<PathBuf>,
}

/// One commit whose message matched a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitHit {
    pub id: String,
    pub summary: String,
    pub message: String,
    pub when: i64,
}

/// The environment a git the IDE runs on its OWN account gets — the
/// background fetch — so it never stops to ask a question nobody is there
/// to answer (a git the user asked for gets [`interactive_env`]). A
/// background fetch over SSH
/// to a host whose key was held back fell through to password
/// authentication and put `root@host's password:` on the terminal the IDE
/// was launched from (David, 2026-09-16: "I also shouldn't get password
/// prompts in the console from the IDE"). `GIT_TERMINAL_PROMPT=0` is git's
/// own switch for its prompts; `BatchMode=yes` is ssh's, and it refuses
/// rather than asks — a failed step is a toast or a stale count, which is
/// what headless means. A `GIT_SSH_COMMAND` the user set is kept and the
/// option appended to it, since it is theirs.
pub fn non_interactive_env() -> Vec<(String, String)> {
    let ssh = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .filter(|command| !command.trim().is_empty())
        .unwrap_or_else(|| "ssh".to_string());
    vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        (
            "GIT_SSH_COMMAND".to_string(),
            format!("{ssh} -oBatchMode=yes"),
        ),
        ("SSH_ASKPASS_REQUIRE".to_string(), "never".to_string()),
    ]
}

/// The environment a git the USER asked for runs with: no terminal
/// prompt either — the IDE has no terminal a person is watching — but
/// every question routed to `askpass`, a program run with the prompt and
/// read for the answer, which the IDE provides as a dialog
/// (`taste_app::askpass`). ssh is told to use it even though it has no
/// tty to be denied, and is NOT put in BatchMode: a deliberate Pull may
/// ask (David, 2026-09-16: "I want it to prompt me if I'm explicitly
/// pushing/pulling and it's necessary"). A `GIT_SSH_COMMAND` the user set
/// is left alone here.
pub fn interactive_env(askpass: &Path, socket: Option<&Path>) -> Vec<(String, String)> {
    let helper = askpass.display().to_string();
    let mut env = vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GIT_ASKPASS".to_string(), helper.clone()),
        ("SSH_ASKPASS".to_string(), helper),
        ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
    ];
    // Where the helper finds the running IDE, so the question is asked in
    // the IDE's own strip rather than a window of the helper's.
    if let Some(socket) = socket {
        env.push((
            "TASTE_ASKPASS_SOCKET".to_string(),
            socket.display().to_string(),
        ));
    }
    env
}

impl GitWorkspace {
    /// Open the repository containing `root`, if any.
    /// Initialize a fresh repository at `root` (the not-a-repo button).
    pub fn init(root: &Path) -> Result<()> {
        Repository::init(root).with_context(|| format!("git init in {}", root.display()))?;
        Ok(())
    }

    pub fn discover(root: &Path) -> Option<Self> {
        // The folder's own repository, else the IDE's private one for a
        // folder without git (`private`), whose working tree it is.
        let repo = Repository::discover(root)
            .ok()
            .or_else(|| Repository::open(private::find_private(root)?).ok())?;
        let workdir = repo.workdir()?.to_path_buf();
        Some(Self { repo, workdir })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// The repository's own directory: `.git`, or a private repository's.
    pub fn git_dir(&self) -> &Path {
        self.repo.path()
    }

    /// Whether git ignores this path — the question a directory walk asks
    /// before descending.
    ///
    /// The disk budget's measurement is the caller that matters
    /// (`taste_devcontainer::supervisor`): "the clone" and "the clone plus
    /// its build artifacts" differ by two orders of magnitude here, and the
    /// project's own `.gitignore` is the only statement of which files are
    /// which that does not have to be maintained separately. A path outside
    /// the work tree, or one git cannot answer for, is reported as *not*
    /// ignored: a walk that pruned on an error would silently under-count.
    pub fn ignores(&self, path: &Path) -> bool {
        self.repo.is_path_ignored(path).unwrap_or(false)
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

    /// Discard one tracked path's working-tree changes: the index's
    /// version back over the file, which is `git restore --worktree`, so
    /// what is staged stays staged and only the edits on top of it go —
    /// the words the Discard panel uses. A conflicted path has no one
    /// version in the index, and there HEAD's goes back over the index
    /// and the file both, which resolves it: `git checkout HEAD -- path`,
    /// which every Discard used to be. A path the index does not have is
    /// left alone, as libgit2 leaves it.
    pub fn restore_file(&self, rel_path: &Path) -> Result<()> {
        let mut builder = git2::build::CheckoutBuilder::new();
        builder.force().path(rel_path);
        if self.conflicted(rel_path)? {
            self.repo.checkout_head(Some(&mut builder))?;
        } else {
            self.repo.checkout_index(None, Some(&mut builder))?;
        }
        Ok(())
    }

    /// Whether the index holds `rel_path` as a conflict.
    fn conflicted(&self, rel_path: &Path) -> Result<bool> {
        let index = self.repo.index()?;
        if !index.has_conflicts() {
            return Ok(false);
        }
        let wanted = rel_path.to_string_lossy();
        for conflict in index.conflicts()? {
            let conflict = conflict?;
            let path = [&conflict.our, &conflict.their, &conflict.ancestor]
                .into_iter()
                .flatten()
                .next()
                .map(|entry| String::from_utf8_lossy(&entry.path).into_owned());
            if path.as_deref() == Some(wanted.as_ref()) {
                return Ok(true);
            }
        }
        Ok(false)
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

    /// The stash entries, newest first, each named by its commit — for
    /// "which stash holds this file", and for naming that entry to a later
    /// verb. By commit rather than by position (`stash@{n}`), because a
    /// position is true only at the moment it is read: an entry pushed in
    /// between moves every one below it down.
    pub fn stash_entries(&self) -> Result<Vec<StashEntry>> {
        // stash_foreach needs &mut Repository; use a scratch handle so the
        // shared one stays immutable.
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
            // The tracked changes: the stash commit against its base. A
            // third parent, when present, holds the untracked files
            // `stash -u` took.
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
            entries.push(StashEntry {
                id: oid.to_string(),
                paths,
            });
        }
        Ok(entries)
    }

    /// Every path any stash entry holds.
    pub fn stashed_paths(&self) -> Result<std::collections::HashSet<PathBuf>> {
        Ok(self
            .stash_entries()?
            .into_iter()
            .flat_map(|entry| entry.paths)
            .collect())
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
    /// Commit messages on HEAD's history that contain `needle`, newest
    /// first, walking at most `max_walk` commits and keeping at most
    /// `max_hits`. Case-insensitive unless the needle has an uppercase
    /// letter, like every other search in the IDE (docs/SEARCH.md).
    pub fn search_commits(
        &self,
        needle: &str,
        max_walk: usize,
        max_hits: usize,
    ) -> Result<Vec<CommitHit>> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let sensitive = needle.chars().any(char::is_uppercase);
        let folded = if sensitive {
            needle.to_string()
        } else {
            needle.to_lowercase()
        };
        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        walk.set_sorting(git2::Sort::TIME)?;
        let mut out = Vec::new();
        for oid in walk.take(max_walk) {
            let Ok(oid) = oid else { continue };
            let Ok(commit) = self.repo.find_commit(oid) else {
                continue;
            };
            let message = commit.message().unwrap_or_default();
            let hay = if sensitive {
                message.to_string()
            } else {
                message.to_lowercase()
            };
            if !hay.contains(&folded) {
                continue;
            }
            out.push(CommitHit {
                id: oid.to_string(),
                summary: commit.summary().unwrap_or_default().to_string(),
                message: message.to_string(),
                when: commit.time().seconds(),
            });
            if out.len() >= max_hits {
                break;
            }
        }
        Ok(out)
    }

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

    /// Put this repository — a fresh clone about to become an
    /// environment — on `branch` at `tip`, checked out. When the clone
    /// does not have `tip` (its source's copy of the branch lagged the
    /// commit it names), its own copy of `branch` is used; without either,
    /// it is left where the clone put it.
    pub fn start_on(&self, branch: &str, tip: git2::Oid) -> Result<()> {
        let local = format!("refs/heads/{branch}");
        let target = if self.repo.find_commit(tip).is_ok() {
            tip
        } else if let Some(own) = self.read_ref(&local)? {
            own
        } else {
            return Ok(());
        };
        self.set_ref(&local, target)?;
        self.repo.set_head(&local)?;
        self.repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .with_context(|| format!("checking out {branch}"))?;
        Ok(())
    }

    /// The submodules `.gitmodules` declares: each one's name and path,
    /// in the order the file gives them.
    ///
    /// Only paths plainly inside the working tree: `.gitmodules` is the
    /// repository's own text, and every caller joins the path onto a
    /// directory — the folder, where the IDE pushes a submodule's clone
    /// from with the user's keys, or the checkout. A declared
    /// `../other-repo` would have handed Personal a repository from
    /// outside the project, so a path that is absolute or steps up is not
    /// a submodule here, as git's own index would not take it.
    ///
    /// Nor one that reaches its place through a link in the folder, its
    /// own name included. The checkout's links are mirrored here with
    /// whatever target they carry, so `sub -> ../sibling` beside a
    /// `.gitmodules` naming `sub` is lexically inside and lands on another
    /// project on this machine — whose refs the sync would then hand the
    /// VM, and whose working tree it would mirror the checkout into.
    pub fn submodules(&self) -> Vec<(String, PathBuf)> {
        self.repo
            .submodules()
            .map(|subs| {
                subs.iter()
                    .filter_map(|sub| Some((sub.name()?.to_string(), sub.path().to_path_buf())))
                    .filter(|(_, path)| inside(path))
                    .filter(|(_, path)| !beneath::passes_a_link(&self.workdir, path))
                    .collect()
            })
            .unwrap_or_default()
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
    ///
    /// Submodules first, on demand: a submodule commit this push records
    /// that the submodule's remote does not have is pushed there before
    /// the branch, and when it cannot be, nothing is pushed — never a
    /// parent pointing at a commit only this machine has (David,
    /// 2026-10-06: "add submodule push to the IDE's push"). Git does this
    /// for the submodules it knows as active; with none, the flag changes
    /// nothing.
    pub fn push_command(&self) -> (String, Vec<String>) {
        self.git_command(&["push", PUSH_SUBMODULES])
    }

    /// The user's push, carrying extra refspecs alongside the branch — e.g.
    /// `refs/taste/issues:refs/taste/issues`, so the issues ref rides along
    /// on a push the human triggered (docs/ENVIRONMENTS.md, "Issues: a ref,
    /// not a service").
    ///
    /// Still argv only, still user-only: the IDE executes it host-side with
    /// the user's credential helpers. With no extra refspecs this is exactly
    /// [`GitWorkspace::push_command`]; with them, the remote has to be named
    /// (git takes refspecs only after a repository argument), so the branch
    /// becomes an explicit `HEAD:refs/heads/<upstream-or-current>` refspec
    /// first, extras after.
    ///
    /// Agent branches are never among the extras: publishing to the world
    /// stays a deliberate human act on the user's own branch.
    pub fn push_command_with(&self, extra_refspecs: &[&str]) -> (String, Vec<String>) {
        if extra_refspecs.is_empty() {
            return self.push_command();
        }
        let mut args = vec![
            "push".to_string(),
            PUSH_SUBMODULES.to_string(),
            self.push_remote(),
        ];
        if let Some(branch) = self.push_branch_refspec() {
            args.push(branch);
        }
        args.extend(extra_refspecs.iter().map(|s| (*s).to_string()));
        self.git_command_owned(args)
    }

    /// Remote the current branch pushes to: its upstream's remote, else
    /// `origin`, else the only configured remote.
    fn push_remote(&self) -> String {
        match self.branch_name() {
            Some(branch) => self.push_remote_of(&branch),
            None => self.default_remote(),
        }
    }

    /// Remote `branch` pushes to, as `git push` resolves it
    /// ([`GitWorkspace::push_target`]).
    fn push_remote_of(&self, branch: &str) -> String {
        self.push_target(branch).remote
    }

    /// `origin`, else the only configured remote.
    fn default_remote(&self) -> String {
        let remotes = self.remotes().unwrap_or_default();
        if remotes.iter().any(|r| r == "origin") || remotes.len() != 1 {
            "origin".to_string()
        } else {
            remotes[0].clone()
        }
    }

    /// `HEAD:refs/heads/<name>` for the branch this push should carry —
    /// the upstream's branch name when set, else the current branch's own.
    /// `None` when HEAD is detached.
    fn push_branch_refspec(&self) -> Option<String> {
        let branch = self.branch_name()?;
        let target = self
            .repo
            .branch_upstream_name(&format!("refs/heads/{branch}"))
            .ok()
            .and_then(|name| {
                // refs/remotes/<remote>/<branch> → <branch>
                let name = name.as_str()?.to_string();
                let rest = name.strip_prefix("refs/remotes/")?;
                let (_, branch) = rest.split_once('/')?;
                Some(branch.to_string())
            })
            .unwrap_or(branch);
        Some(format!("HEAD:refs/heads/{target}"))
    }

    fn git_command(&self, args: &[&str]) -> (String, Vec<String>) {
        self.git_command_owned(args.iter().map(|s| (*s).to_string()).collect())
    }

    fn git_command_owned(&self, args: Vec<String>) -> (String, Vec<String>) {
        (
            "git".to_string(),
            private::cli_prefix(&self.workdir)
                .into_iter()
                .chain(args)
                .collect(),
        )
    }

    // --- sync with the remote tip (fetch + rebase, never merge) ----------

    /// How the current branch relates to where it pushes and to what it
    /// is rebased onto: [`GitWorkspace::sync_status_of`] of HEAD's branch.
    pub fn sync_status(&self) -> Result<SyncStatus> {
        match self.branch_name() {
            Some(branch) => self.sync_status_of(&branch),
            None => Ok(SyncStatus::no_upstream()),
        }
    }

    /// The current branch's upstream, as a full ref name
    /// (`refs/remotes/origin/main`), or `None` without one. What a rebase
    /// somewhere this repository's config does not reach — a checkout in
    /// a VM, seeded with this repository's remote-tracking refs — is told
    /// to rebase onto.
    pub fn upstream_ref(&self) -> Option<String> {
        let branch = self.branch_name()?;
        let branch = self
            .repo
            .find_branch(&branch, git2::BranchType::Local)
            .ok()?;
        branch.upstream().ok()?.get().name().map(str::to_string)
    }

    /// [`GitWorkspace::upstream_ref`] of `branch` rather than of HEAD: what
    /// a checkout in a VM, on a branch this repository has not checked
    /// out, rebases onto.
    pub fn upstream_ref_of(&self, branch: &str) -> Option<String> {
        let branch = self
            .repo
            .find_branch(branch, git2::BranchType::Local)
            .ok()?;
        branch.upstream().ok()?.get().name().map(str::to_string)
    }

    /// The URL of the remote the current branch's upstream lives on, if
    /// the branch has one and the remote has a URL — what a fetch would
    /// reach for, and so what `presence::fetch_needs_presence` is asked
    /// about.
    pub fn upstream_remote_url(&self) -> Option<String> {
        let branch_name = self.branch_name()?;
        let branch = self
            .repo
            .find_branch(&branch_name, git2::BranchType::Local)
            .ok()?;
        let refname = branch.get().name()?.to_string();
        let remote = self.repo.branch_upstream_remote(&refname).ok()?;
        self.url_of_remote(remote.as_str()?)
    }

    /// The URL `branch`'s push goes to ([`GitWorkspace::push_target`]):
    /// what a push reaches for, and so what presence and the askpass are
    /// asked about.
    pub fn push_url_of(&self, branch: &str) -> Option<String> {
        self.url_of_remote(&self.push_target(branch).remote)
    }

    /// A configured remote's URL, or the remote itself when a branch names
    /// a URL directly (`gh pr checkout`).
    fn url_of_remote(&self, remote: &str) -> Option<String> {
        match self.repo.find_remote(remote) {
            Ok(found) => found.url().map(str::to_string),
            Err(_) => Some(remote.to_string()),
        }
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
    /// Where a push goes, as a person reads it (`origin/feature`, or a
    /// branch on a URL), if the branch pushes anywhere.
    pub upstream: Option<String>,
    /// Commits the push target does not have.
    pub ahead: usize,
    /// Commits on the base ([`SyncStatus::base`]) when there is one, else
    /// on the push target, that this branch does not have.
    pub behind: usize,
    /// The push target is where a push would CREATE the branch
    /// (`sync_status_of`): the remote does not have it yet.
    pub new_branch: bool,
    /// What Sync rebases onto when it is not the push target
    /// (`origin/main` for a branch pushed to a fork).
    pub base: Option<String>,
    /// The push target has commits this branch does not — a rebase
    /// rewrote them, or someone pushed: its tip, which a push replacing it
    /// is leased on, and how many of its commits it would drop.
    pub replaces: Option<(String, usize)>,
    /// The push target's tip is not known yet (a URL not yet asked), so
    /// `ahead` is a ceiling: what no remote's refs reach.
    pub push_unknown: bool,
}

impl SyncStatus {
    pub(crate) fn no_upstream() -> Self {
        Self {
            upstream: None,
            ahead: 0,
            behind: 0,
            new_branch: false,
            base: None,
            replaces: None,
            push_unknown: false,
        }
    }
}

/// Whether a repository-relative path names something inside the
/// working tree: every component an ordinary name, none of them `..`, the
/// root, or a prefix, and not empty.
fn inside(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submodule_path_must_stay_inside_the_working_tree() {
        for (path, ok) in [
            ("ext/amutablectl", true),
            ("lib", true),
            ("../other-repo", false),
            ("ext/../../x", false),
            ("/etc", false),
            ("./lib", false),
            ("", false),
        ] {
            assert_eq!(inside(Path::new(path)), ok, "{path}");
        }
    }

    /// A submodule `.gitmodules` names at a path that is a link in the
    /// folder is not one: the link could be the checkout's, pointing at
    /// another project on this machine.
    #[test]
    fn a_submodule_reached_through_a_link_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        git2::Repository::init(outside.path()).unwrap();
        git2::Repository::init(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"real\"]\n\tpath = real\n\turl = https://example.com/real.git\n\
             [submodule \"linked\"]\n\tpath = linked\n\turl = https://example.com/linked.git\n",
        )
        .unwrap();
        git2::Repository::init(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        let paths: Vec<PathBuf> = ws.submodules().into_iter().map(|(_, p)| p).collect();
        assert!(paths.contains(&PathBuf::from("real")), "{paths:?}");
        assert!(!paths.contains(&PathBuf::from("linked")), "{paths:?}");
    }

    /// Each submodule's files are keyed under its path, the nested ones
    /// under theirs, and a submodule with nothing changed adds nothing.
    #[test]
    fn submodule_status_is_keyed_under_each_submodule() {
        let z = "@@ template\0 M lib.typ\0?? new.typ\0@@ clean\0@@ template/inner\0M  a.txt\0";
        let status = status_from_submodule_porcelain(z);
        assert_eq!(status.len(), 3, "{status:?}");
        assert_eq!(status[Path::new("template/lib.typ")], FileState::Modified);
        assert_eq!(status[Path::new("template/new.typ")], FileState::Untracked);
        assert_eq!(status[Path::new("template/inner/a.txt")], FileState::Staged);
        assert!(status_from_submodule_porcelain("").is_empty());
    }

    /// Every porcelain code lands on the state the library would give.
    #[test]
    fn porcelain_codes_map_to_file_states() {
        let z = " M a.rs\0M  b.rs\0MM c.rs\0?? d.rs\0!! e.o\0UU f.rs\0R  new.rs\0old.rs\0A  g.rs\0 D h.rs\0";
        let status = status_from_porcelain(z);
        assert_eq!(status[Path::new("a.rs")], FileState::Modified);
        assert_eq!(status[Path::new("b.rs")], FileState::Staged);
        assert_eq!(status[Path::new("c.rs")], FileState::Staged);
        assert_eq!(status[Path::new("d.rs")], FileState::Untracked);
        assert_eq!(status[Path::new("e.o")], FileState::Ignored);
        assert_eq!(status[Path::new("f.rs")], FileState::Conflicted);
        assert_eq!(status[Path::new("new.rs")], FileState::Staged);
        assert!(
            !status.contains_key(Path::new("old.rs")),
            "the rename's origin is not a file"
        );
        assert_eq!(status[Path::new("g.rs")], FileState::Staged);
        assert_eq!(status[Path::new("h.rs")], FileState::Modified);
        assert!(status_from_porcelain("").is_empty());
    }
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

    /// A branch made off main that the remote does not have: its status is
    /// against `<remote>/<branch>`, counted as new there, whatever HEAD is,
    /// and its push names it and sets the upstream.
    #[test]
    fn a_clone_starts_on_the_branch_it_is_given() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        ws.create_branch("devcontainer").unwrap();
        fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("second").unwrap();
        let tip = Repository::open(dir.path())
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap();
        ws.switch_branch(&main).unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = clone_dir.path().join("env");
        crate::clone_local(dir.path(), &clone).unwrap();
        let env = GitWorkspace::discover(&clone).unwrap();
        assert_eq!(
            env.branch_name().as_deref(),
            Some(main.as_str()),
            "the clone's own start"
        );
        env.start_on("devcontainer", tip).unwrap();
        assert_eq!(env.branch_name().as_deref(), Some("devcontainer"));
        assert!(
            clone.join("b.txt").exists(),
            "checked out, not only pointed at"
        );
    }

    #[test]
    fn a_branch_without_an_upstream_pushes_to_its_own_name() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        repo.remote("origin", "https://example.invalid/repo.git")
            .unwrap();
        // origin has main, as a fetch would have left it.
        let tip = repo.head().unwrap().target().unwrap();
        repo.reference(&format!("refs/remotes/origin/{main}"), tip, true, "fetched")
            .unwrap();
        ws.create_branch("devcontainer").unwrap();
        fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("second").unwrap();
        // HEAD back on main, as a peer's is while the VM is on the branch.
        ws.switch_branch(&main).unwrap();

        let sync = ws.sync_status_of("devcontainer").unwrap();
        assert_eq!(sync.upstream.as_deref(), Some("origin/devcontainer"));
        assert_eq!((sync.ahead, sync.behind, sync.new_branch), (1, 0, true));

        let (_, args) = ws.push_branch_command("devcontainer", &[]);
        assert_eq!(
            &args[2..],
            [
                "push",
                "--set-upstream",
                "origin",
                "refs/heads/devcontainer:refs/heads/devcontainer"
            ]
        );
        // The branch HEAD is on is still measured the old way.
        assert!(!ws.sync_status_of(&main).unwrap().new_branch);
    }

    /// A repository with `origin/<main>` fetched, `origin/HEAD` pointing
    /// at it as `git clone` leaves it, and a branch `topic` one commit
    /// ahead of it. Returns the main branch's name and `origin`'s tip.
    fn cloned_with_topic() -> (tempfile::TempDir, GitWorkspace, String, Oid) {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let main = ws.branch_name().unwrap();
        let repo = Repository::open(dir.path()).unwrap();
        repo.remote("origin", "https://example.invalid/upstream.git")
            .unwrap();
        let tip = repo.head().unwrap().target().unwrap();
        repo.reference(&format!("refs/remotes/origin/{main}"), tip, true, "fetched")
            .unwrap();
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            &format!("refs/remotes/origin/{main}"),
            true,
            "clone",
        )
        .unwrap();
        ws.create_branch("topic").unwrap();
        fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        ws.stage(Path::new("b.txt")).unwrap();
        ws.commit("second").unwrap();
        (dir, ws, main, tip)
    }

    /// Upstream moves on: a commit on `origin/<main>` this branch lacks.
    fn upstream_moves(dir: &Path, main: &str, from: Oid) -> Oid {
        let repo = Repository::open(dir).unwrap();
        let parent = repo.find_commit(from).unwrap();
        let sig = git2::Signature::now("Upstream", "u@example.invalid").unwrap();
        let moved = repo
            .commit(
                None,
                &sig,
                &sig,
                "upstream",
                &parent.tree().unwrap(),
                &[&parent],
            )
            .unwrap();
        repo.reference(
            &format!("refs/remotes/origin/{main}"),
            moved,
            true,
            "fetched",
        )
        .unwrap();
        moved
    }

    /// What `gh pr checkout` writes for a pull request from a fork: the
    /// fork by URL, as both remotes, and the branch's own name. The push
    /// target's tip is learnt from the remote; until then the count is a
    /// ceiling and says so. The base is the default branch of the remote
    /// the repository was cloned from.
    #[test]
    fn a_gh_pr_checkout_pushes_to_the_fork_and_rebases_onto_upstream() {
        let (dir, ws, main, origin_tip) = cloned_with_topic();
        let repo = Repository::open(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        let fork = "https://example.invalid/fork.git";
        config.set_str("branch.topic.remote", fork).unwrap();
        config.set_str("branch.topic.pushremote", fork).unwrap();
        config
            .set_str("branch.topic.merge", "refs/heads/topic")
            .unwrap();
        upstream_moves(dir.path(), &main, origin_tip);

        // Not asked yet: a ceiling, flagged.
        let sync = ws.sync_status_of("topic").unwrap();
        assert!(sync.push_unknown, "{sync:?}");
        assert_eq!((sync.ahead, sync.new_branch), (1, false), "{sync:?}");
        assert_eq!(
            sync.base.as_deref(),
            Some(format!("origin/{main}").as_str())
        );
        assert_eq!(sync.behind, 1, "behind the base, not the fork: {sync:?}");
        assert_eq!(
            ws.rebase_target_of("topic").as_deref(),
            Some(format!("refs/remotes/origin/{main}").as_str())
        );

        // The fork has exactly this branch (the IDE asked): nothing to push.
        let topic = repo.head().unwrap().target().unwrap();
        let (_, ask, target) = ws.ask_push_tip_command("topic").unwrap();
        assert_eq!(
            &ask[ask.len() - 3..],
            ["ls-remote", fork, "refs/heads/topic"]
        );
        record_remote_tip(fork, "topic", &format!("{topic}\trefs/heads/topic\n"));
        assert!(ws.fetch_push_tip_command(&target).is_none(), "already here");
        let sync = ws.sync_status_of("topic").unwrap();
        assert_eq!(
            (sync.ahead, sync.push_unknown, sync.replaces.clone()),
            (0, false, None),
            "{sync:?}"
        );

        // The push goes to the fork by URL under the branch's name, and
        // leaves the configured upstream alone.
        let (_, args) = ws.push_branch_command("topic", &[]);
        assert_eq!(
            &args[args.len() - 3..],
            ["push", fork, "refs/heads/topic:refs/heads/topic"]
        );
        // After a rebase the fork's tip is not in the branch: the push
        // replaces it, leased on that tip.
        record_remote_tip(fork, "topic", &format!("{origin_tip}\trefs/heads/topic\n"));
        let sync = ws.sync_status_of("topic").unwrap();
        assert_eq!(sync.replaces, None, "an ancestor is not replaced: {sync:?}");
        let (_, leased) =
            ws.push_branch_command_leased("topic", Some(&origin_tip.to_string()), &[]);
        assert!(
            leased.contains(&format!("--force-with-lease=refs/heads/topic:{origin_tip}")),
            "{leased:?}"
        );
    }

    /// An environment's peer pulls from Personal: the branch's upstream is
    /// `personal/<branch>`, read from the folder's copy of Personal's
    /// branches, its push still goes to `origin`, and Sync counts and
    /// rebases against Personal.
    #[test]
    fn an_environment_branch_pulls_from_personal_and_pushes_where_it_did() {
        let (folder, folder_ws) = temp_repo();
        std::fs::write(folder.path().join("a.txt"), "one\n").unwrap();
        folder_ws.stage(Path::new("a.txt")).unwrap();
        folder_ws.commit("first").unwrap();
        let main = folder_ws.branch_name().unwrap();
        let peer = tempfile::tempdir().unwrap();
        Repository::clone(folder.path().to_str().unwrap(), peer.path()).unwrap();
        let ws = GitWorkspace::discover(peer.path()).unwrap();
        // Personal commits: the folder's copy of its branch moves, the
        // folder's own branch does not.
        std::fs::write(folder.path().join("b.txt"), "personal's\n").unwrap();
        folder_ws.stage(Path::new("b.txt")).unwrap();
        let second = folder_ws.commit("in Personal").unwrap();
        let first = folder_ws
            .repo
            .find_commit(second)
            .unwrap()
            .parent_id(0)
            .unwrap();
        folder_ws
            .set_ref(&format!("refs/taste/vm/{main}"), second)
            .unwrap();
        folder_ws
            .set_ref(&format!("refs/heads/{main}"), first)
            .unwrap();

        assert!(ws.track_personal(folder.path(), &main).unwrap());
        let sync = ws.sync_status_of(&main).unwrap();
        assert_eq!(
            sync.base.as_deref(),
            Some(format!("personal/{main}").as_str())
        );
        assert_eq!(sync.behind, 1, "{sync:?}");
        assert_eq!(
            ws.rebase_target_of(&main).as_deref(),
            Some(format!("refs/remotes/personal/{main}").as_str())
        );
        assert_eq!(ws.push_target(&main).remote, "origin");
        // Again: nothing changes, and a branch Personal lacks is left be.
        assert!(ws.track_personal(folder.path(), &main).unwrap());
        assert!(!ws.track_personal(folder.path(), "elsewhere").unwrap());
    }

    /// The named-remote fork setup git describes: upstream `origin/<main>`,
    /// push remote the fork. ↑ counts against the fork's tracking ref, ↓
    /// against upstream, and the push never rewrites the upstream.
    #[test]
    fn a_push_remote_apart_from_the_upstream_splits_the_counts() {
        let (dir, ws, main, origin_tip) = cloned_with_topic();
        let repo = Repository::open(dir.path()).unwrap();
        repo.remote("fork", "https://example.invalid/fork.git")
            .unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("branch.topic.remote", "origin").unwrap();
        config
            .set_str("branch.topic.merge", &format!("refs/heads/{main}"))
            .unwrap();
        config.set_str("branch.topic.pushRemote", "fork").unwrap();
        upstream_moves(dir.path(), &main, origin_tip);

        let sync = ws.sync_status_of("topic").unwrap();
        assert_eq!(sync.upstream.as_deref(), Some("fork/topic"));
        assert_eq!(
            sync.base.as_deref(),
            Some(format!("origin/{main}").as_str())
        );
        assert_eq!(
            (sync.ahead, sync.behind, sync.new_branch),
            (1, 1, true),
            "{sync:?}"
        );
        let (_, args) = ws.push_branch_command("topic", &[]);
        assert_eq!(
            &args[args.len() - 3..],
            ["push", "fork", "refs/heads/topic:refs/heads/topic"],
            "no --set-upstream over the base"
        );

        // Pushed once, then rebased onto upstream: the fork's copy is the
        // old one, which the next push replaces.
        let topic = repo.head().unwrap().target().unwrap();
        repo.reference("refs/remotes/fork/topic", topic, true, "pushed")
            .unwrap();
        let sync = ws.sync_status_of("topic").unwrap();
        assert_eq!((sync.ahead, sync.replaces.clone()), (0, None), "{sync:?}");
        let parent = repo.find_commit(topic).unwrap().parent_id(0).unwrap();
        repo.reference("refs/heads/topic-old", topic, true, "kept")
            .unwrap();
        let old = topic;
        repo.reference("refs/heads/topic", parent, true, "rewound")
            .unwrap();
        let sync = ws.sync_status_of("topic").unwrap();
        assert_eq!(sync.replaces, Some((old.to_string(), 1)), "{sync:?}");
    }

    /// `push.default` decides the name on the remote, as git's own push
    /// does: a branch made from `origin/main` under the default `simple`
    /// pushes under its own name, never into `main`.
    #[test]
    fn a_branch_made_from_main_does_not_push_into_main() {
        let (dir, ws, main, _) = cloned_with_topic();
        let repo = Repository::open(dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("branch.topic.remote", "origin").unwrap();
        config
            .set_str("branch.topic.merge", &format!("refs/heads/{main}"))
            .unwrap();
        assert_eq!(ws.push_target("topic").branch, "topic");
        config.set_str("push.default", "upstream").unwrap();
        assert_eq!(ws.push_target("topic").branch, main);
    }

    #[test]
    fn push_argv_carries_extra_refspecs_after_the_branch() {
        let (dir, ws) = temp_repo();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        ws.stage(Path::new("a.txt")).unwrap();
        ws.commit("first").unwrap();
        let branch = ws.branch_name().unwrap();
        let wd = ws.workdir().display().to_string();

        // No extras: byte-for-byte the push the user always got.
        assert_eq!(ws.push_command_with(&[]), ws.push_command());
        assert_eq!(
            ws.push_command(),
            (
                "git".into(),
                vec![
                    "-C".into(),
                    wd.clone(),
                    "push".into(),
                    PUSH_SUBMODULES.into()
                ]
            )
        );

        // With extras and no upstream: origin, this branch, extras last.
        let (program, args) = ws.push_command_with(&["refs/taste/issues:refs/taste/issues"]);
        assert_eq!(program, "git");
        assert_eq!(
            args,
            vec![
                "-C".to_string(),
                wd.clone(),
                "push".into(),
                PUSH_SUBMODULES.into(),
                "origin".into(),
                format!("HEAD:refs/heads/{branch}"),
                "refs/taste/issues:refs/taste/issues".into(),
            ]
        );

        // With an upstream on a differently-named remote and branch, both
        // are honoured.
        {
            let repo = Repository::open(dir.path()).unwrap();
            repo.remote("hub", "https://example.invalid/repo.git")
                .unwrap();
            let mut config = repo.config().unwrap();
            config
                .set_str(&format!("branch.{branch}.remote"), "hub")
                .unwrap();
            config
                .set_str(&format!("branch.{branch}.merge"), "refs/heads/trunk")
                .unwrap();
        }
        let ws = GitWorkspace::discover(dir.path()).unwrap();
        let (_, args) = ws.push_command_with(&["refs/taste/issues:refs/taste/issues"]);
        assert_eq!(
            args,
            vec![
                "-C".to_string(),
                wd,
                "push".into(),
                PUSH_SUBMODULES.into(),
                "hub".into(),
                "HEAD:refs/heads/trunk".into(),
                "refs/taste/issues:refs/taste/issues".into(),
            ]
        );
    }

    /// The user's push takes a submodule commit the branch records to the
    /// submodule's remote first: against real git, with local bare
    /// repositories for both remotes.
    #[test]
    fn a_push_takes_the_submodule_commit_it_records_along() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
                .env("GIT_CONFIG_VALUE_0", "always")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // The template and its remote.
        git(root, &["init", "-q", "--bare", "-b", "main", "tpl.git"]);
        git(root, &["clone", "-q", "tpl.git", "tpl-work"]);
        fs::write(root.join("tpl-work/a"), "1").unwrap();
        git(&root.join("tpl-work"), &["add", "-A"]);
        git(&root.join("tpl-work"), &["commit", "-q", "-m", "one"]);
        git(
            &root.join("tpl-work"),
            &["push", "-q", "origin", "HEAD:main"],
        );
        // The deck, its remote, and the template as its submodule.
        git(root, &["init", "-q", "--bare", "-b", "main", "deck.git"]);
        git(root, &["clone", "-q", "deck.git", "deck"]);
        let deck = root.join("deck");
        let tpl_url = root.join("tpl.git").display().to_string();
        git(&deck, &["submodule", "add", "-q", &tpl_url, "tpl"]);
        git(&deck, &["commit", "-q", "-m", "add the template"]);
        git(&deck, &["push", "-q", "-u", "origin", "HEAD:main"]);
        // A commit in the submodule, recorded in the deck, pushed by the
        // IDE's push of the deck alone.
        fs::write(deck.join("tpl/a"), "2").unwrap();
        git(&deck.join("tpl"), &["commit", "-q", "-am", "two"]);
        let recorded = git(&deck.join("tpl"), &["rev-parse", "HEAD"]);
        git(&deck, &["add", "tpl"]);
        git(&deck, &["commit", "-q", "-m", "record two"]);
        let ws = GitWorkspace::discover(&deck).unwrap();
        let (program, args) = ws.push_command();
        let out = std::process::Command::new(program)
            .args(&args)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
            .env("GIT_CONFIG_VALUE_0", "always")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            git(&root.join("tpl.git"), &["rev-parse", "main"]),
            recorded,
            "the submodule's commit reached its remote"
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
