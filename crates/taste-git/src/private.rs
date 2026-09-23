//! A repository for a folder that has none of its own.
//!
//! Everything the IDE does with a checkout — placing it in a VM, the
//! snapshots, the mirror that keeps the folder and the checkout in step
//! (`crate::mirror`) — travels as git, so a folder without git could not
//! be opened at all (David, 2026-09-23: "I should be able to just open up
//! Taste, edit and save some files, and close it … and maybe even if git
//! isn't set up?"). Such a folder gets a repository of the IDE's own, kept
//! in the IDE's state directory with the folder as its working tree
//! (`core.worktree`): the folder never gains a `.git`, and every git path
//! the IDE has works on it unchanged — [`crate::GitWorkspace::discover`]
//! finds it, and [`cli_prefix`] is how a `git` command line names it.
//!
//! **Cleaned up** (David: "be sure there's cleanup of junk ones"):
//! [`sweep`] removes a private repository whose folder is gone, or has
//! since been given a `.git` of its own, which then speaks for it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The most files a folder may have for the IDE to start tracking it: a
/// home directory opened by mistake must not be walked into a repository.
pub const MAX_TRACKED_FILES: usize = 50_000;

/// What a private repository leaves out from the start, as `info/exclude`
/// — build output and caches a folder without a `.gitignore` still has.
const DEFAULT_EXCLUDES: &str = "\
# The IDE's defaults for a folder it tracks privately (taste-git::private).
node_modules/
target/
.cache/
__pycache__/
.venv/
dist/
build/
.DS_Store
*.swp
";

/// Where private repositories live.
fn root() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("taste-ide/folders"))
}

/// The private repository for `folder`, whether or not it exists: named for
/// the folder, with a hash of its canonical path so two folders of one
/// name do not meet.
pub fn private_dir(folder: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let canonical = folder.canonicalize().ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    Some(root()?.join(format!("{name}-{:016x}.git", hasher.finish())))
}

/// The private repository that tracks `path` or a folder above it, if one
/// does.
pub fn find_private(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .filter_map(private_dir)
        .find(|dir| dir.join("HEAD").exists())
}

/// How a `git` command line names the repository for `folder`: `-C
/// <folder>` for a folder with its own, `--git-dir <private> --work-tree
/// <folder>` for one tracked privately.
pub fn cli_prefix(folder: &Path) -> Vec<String> {
    let own = git2::Repository::discover(folder).is_ok();
    match (own, find_private(folder)) {
        (false, Some(dir)) => vec![
            "--git-dir".to_string(),
            dir.display().to_string(),
            "--work-tree".to_string(),
            folder.display().to_string(),
        ],
        _ => vec!["-C".to_string(), folder.display().to_string()],
    }
}

/// Make sure `folder` has a repository the IDE can work with: its own, or
/// a private one made now, its first commit the folder as it is. `Ok(true)`
/// when one was made. Refused for a folder with more than
/// [`MAX_TRACKED_FILES`] files outside the default excludes.
pub fn ensure_repository(folder: &Path) -> Result<bool> {
    if git2::Repository::discover(folder).is_ok() || find_private(folder).is_some() {
        return Ok(false);
    }
    let dir = private_dir(folder).context("no state directory to keep a repository in")?;
    let folder = folder
        .canonicalize()
        .with_context(|| format!("resolving {}", folder.display()))?;
    std::fs::create_dir_all(dir.parent().unwrap_or(&dir))?;
    // Made bare and then given the folder as its working tree by config,
    // not by libgit2's own `workdir_path`, which put a `.git` in the folder
    // even when told not to — the one thing this exists to avoid.
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    let bare = git2::Repository::init_opts(&dir, &options)
        .with_context(|| format!("making a repository for {}", folder.display()))?;
    {
        let mut config = bare.config()?;
        config.set_bool("core.bare", false)?;
        config.set_str("core.worktree", &folder.to_string_lossy())?;
    }
    drop(bare);
    let repo = git2::Repository::open(&dir)
        .with_context(|| format!("opening the repository for {}", folder.display()))?;
    if repo.workdir().is_none() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "the repository for {} has no working tree",
            folder.display()
        );
    }
    std::fs::create_dir_all(dir.join("info"))?;
    std::fs::write(dir.join("info/exclude"), DEFAULT_EXCLUDES)?;
    let made = (|| -> Result<()> {
        let mut index = repo.index()?;
        let statuses = repo.statuses(Some(
            git2::StatusOptions::new()
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .include_ignored(false),
        ))?;
        if statuses.len() > MAX_TRACKED_FILES {
            bail!(
                "{} has {} files outside the usual build output; the IDE tracks at most {} in \
                 a folder without git of its own. Run git init there with a .gitignore to \
                 open it",
                folder.display(),
                statuses.len(),
                MAX_TRACKED_FILES
            );
        }
        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                index.add_path(Path::new(path))?;
            }
        }
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("taste-ide", "taste-ide@localhost")?;
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "The folder as it was when Taste first opened it",
            &tree,
            &[],
        )?;
        Ok(())
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(true)
}

/// Remove the private repositories nothing needs: their folder is gone
/// from a parent that is still there, or has a `.git` of its own now. What
/// was removed, for the log.
pub fn sweep() -> Vec<PathBuf> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.extension().is_none_or(|e| e != "git") {
            continue;
        }
        // Read from its config rather than by opening it: libgit2 will not
        // open a repository whose working tree is gone, which is the very
        // case being looked for.
        let folder = git2::Config::open(&dir.join("config"))
            .ok()
            .and_then(|config| config.get_path("core.worktree").ok());
        // Only a verdict the disk can give for certain removes a history:
        // a folder on a drive that is not mounted right now has lost its
        // parent too, and a repository whose config will not read this
        // moment (a lock, a partly written file) is not thereby nobody's.
        // Both are left for a sweep that can tell.
        let junk = match &folder {
            None => false,
            Some(folder) => {
                folder.join(".git").exists()
                    || (!folder.exists() && folder.parent().is_some_and(Path::exists))
            }
        };
        if junk && std::fs::remove_dir_all(&dir).is_ok() {
            removed.push(dir);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// XDG_STATE_HOME is process-wide: the tests that set it take turns.
    static STATE: Mutex<()> = Mutex::new(());

    #[test]
    fn a_folder_without_git_is_tracked_privately_and_swept_when_gone() {
        let _turn = STATE.lock().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", state.path());
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("notes.txt"), "hello\n").unwrap();
        std::fs::create_dir_all(folder.path().join("node_modules/x")).unwrap();
        std::fs::write(folder.path().join("node_modules/x/i.js"), "//\n").unwrap();

        assert!(ensure_repository(folder.path()).unwrap());
        assert!(
            !folder.path().join(".git").exists(),
            "the folder gains no .git"
        );
        assert!(!ensure_repository(folder.path()).unwrap(), "made once");
        let ws = crate::GitWorkspace::discover(folder.path()).expect("discovered privately");
        assert_eq!(
            ws.workdir().canonicalize().unwrap(),
            folder.path().canonicalize().unwrap()
        );
        assert!(
            ws.status().unwrap().is_empty(),
            "the first commit is the folder as it was"
        );
        let head = git2::Repository::open(private_dir(folder.path()).unwrap()).unwrap();
        let tree = head.head().unwrap().peel_to_tree().unwrap();
        assert!(tree.get_path(Path::new("notes.txt")).is_ok());
        assert!(
            tree.get_path(Path::new("node_modules")).is_err(),
            "default excludes"
        );
        let prefix = cli_prefix(folder.path());
        assert_eq!(prefix[0], "--git-dir");

        let dir = private_dir(folder.path()).unwrap();
        drop(folder);
        assert_eq!(sweep(), vec![dir.clone()]);
        assert!(!dir.exists());
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn a_folder_that_gets_its_own_git_frees_the_private_one() {
        let _turn = STATE.lock().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", state.path());
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("a"), "a\n").unwrap();
        ensure_repository(folder.path()).unwrap();
        let dir = private_dir(folder.path()).unwrap();
        git2::Repository::init(folder.path()).unwrap();
        assert_eq!(sweep(), vec![dir]);
        assert_eq!(cli_prefix(folder.path())[0], "-C");
        std::env::remove_var("XDG_STATE_HOME");
    }
}
