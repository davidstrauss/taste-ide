//! The workspace watcher: what makes external edits visible.
//!
//! Self-hosting means agents, container builds, and terminals writing files
//! the IDE must reflect immediately. This watcher turns raw notify events
//! into debounced, deduplicated bus events:
//!
//! - content modifications → [`Event::FileChanged`] per path
//! - create/remove/rename → one [`Event::FileTreeChanged`]
//! - anything under `.git/` → [`Event::GitStatusChanged`] (an agent's
//!   `git commit` shows up in the tree like our own)
//!
//! Noise (build artifacts, caches) is filtered by path prefix.
//!
//! Watches are per-directory, added off the main thread: one recursive
//! watch dies wholesale on the first unreadable directory (a root-owned
//! path inside a flatpak-builder tree took the whole watcher down), and
//! its registration walk would block the GTK thread at startup.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};

use crate::{Event, EventBus};

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Directories whose churn is machine noise, not workspace edits.
const NOISE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".flatpak-builder",
    "build-aux/flatpak/.build",
    "build-aux/flatpak/build",
    "build-aux/flatpak/repo",
];

fn classify(root: &Path, path: &Path) -> Classification {
    let Ok(rel) = path.strip_prefix(root) else {
        return Classification::Noise;
    };
    if rel.starts_with(".git") {
        return Classification::Git;
    }
    for noise in NOISE_DIRS {
        // Component-wise: "target/…" is noise, "targets/…" is not.
        if rel.starts_with(noise) {
            return Classification::Noise;
        }
    }
    Classification::Workspace
}

enum Classification {
    Workspace,
    Git,
    Noise,
}

/// Keeps the underlying watcher alive for the life of the window.
pub struct WorkspaceWatcher {
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
}

/// Start watching. Returns immediately; watch registration (a full tree
/// walk) happens on the watcher thread.
pub fn start(root: PathBuf, events: EventBus) -> Result<WorkspaceWatcher> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            let _ = tx.send(event);
        }
    })?;
    let watcher = Arc::new(Mutex::new(watcher));
    let handle = WorkspaceWatcher {
        _watcher: watcher.clone(),
    };
    std::thread::Builder::new()
        .name("taste-workspace-watch".into())
        .spawn(move || {
            // .git gets shallow watches: index/HEAD/packed-refs and branch
            // tips cover "an agent committed"; object churn is noise.
            for git_dir in [".git", ".git/refs", ".git/refs/heads"] {
                let _ = watcher
                    .lock()
                    .unwrap()
                    .watch(&root.join(git_dir), RecursiveMode::NonRecursive);
            }
            add_dir_watches(&watcher, &root, root.clone());
            debounce_loop(root, events, rx, watcher);
        })?;
    Ok(handle)
}

/// Watch `start_dir` and everything beneath it, one directory at a time.
/// Noise and unreadable directories are skipped — one root-owned build
/// artifact must not cost the workspace its watcher.
fn add_dir_watches(watcher: &Mutex<notify::RecommendedWatcher>, root: &Path, start_dir: PathBuf) {
    let mut stack = vec![start_dir];
    while let Some(dir) = stack.pop() {
        if dir != *root && !matches!(classify(root, &dir), Classification::Workspace) {
            continue;
        }
        if watcher
            .lock()
            .unwrap()
            .watch(&dir, RecursiveMode::NonRecursive)
            .is_err()
        {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                // DirEntry::file_type does not follow symlinks: a link out
                // of the workspace stays unwatched.
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(entry.path());
                }
            }
        }
    }
}

fn debounce_loop(
    root: PathBuf,
    events: EventBus,
    rx: std::sync::mpsc::Receiver<notify::Event>,
    watcher: Arc<Mutex<notify::RecommendedWatcher>>,
) {
    while let Ok(first) = rx.recv() {
        let mut changed: HashSet<PathBuf> = HashSet::new();
        let mut created_dirs: Vec<PathBuf> = Vec::new();
        let mut structural = false;
        let mut git = false;
        let mut absorb = |event: notify::Event| {
            use notify::EventKind;
            let is_structural = matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_))
                || matches!(
                    event.kind,
                    EventKind::Modify(notify::event::ModifyKind::Name(_))
                );
            for path in event.paths {
                match classify(&root, &path) {
                    Classification::Noise => {}
                    Classification::Git => git = true,
                    Classification::Workspace => {
                        if is_structural {
                            structural = true;
                            // Per-directory watches don't extend to new
                            // directories on their own.
                            if matches!(event.kind, EventKind::Create(_)) && path.is_dir() {
                                created_dirs.push(path.clone());
                            }
                        } else {
                            changed.insert(path);
                        }
                    }
                }
            }
        };
        absorb(first);
        // Absorb the burst — but flush at least once a second: a sustained
        // producer (a build churning noise paths) must not make real edits
        // invisible until it quiesces.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = DEBOUNCE.min(remaining);
            if wait.is_zero() {
                break;
            }
            match rx.recv_timeout(wait) {
                Ok(event) => absorb(event),
                Err(_) => break,
            }
        }
        for dir in created_dirs {
            add_dir_watches(&watcher, &root, dir);
        }
        if git {
            events.publish(Event::GitStatusChanged);
        }
        if structural {
            events.publish(Event::FileTreeChanged);
        }
        for path in changed {
            events.publish(Event::FileChanged(path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_rules() {
        let root = Path::new("/w");
        assert!(matches!(
            classify(root, Path::new("/w/src/main.rs")),
            Classification::Workspace
        ));
        assert!(matches!(
            classify(root, Path::new("/w/.git/index")),
            Classification::Git
        ));
        assert!(matches!(
            classify(root, Path::new("/w/target/debug/foo")),
            Classification::Noise
        ));
        assert!(matches!(
            classify(root, Path::new("/elsewhere/x")),
            Classification::Noise
        ));
    }

    #[test]
    fn unreadable_subdir_does_not_kill_the_watcher() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let bus = EventBus::new();
        let rx = bus.subscribe();
        let _watcher = start(dir.path().to_path_buf(), bus).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&file, "two").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            match rx.try_recv() {
                Ok(Event::FileChanged(_)) | Ok(Event::FileTreeChanged) => seen = true,
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        // Restore permissions so tempdir cleanup can remove it.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(seen, "sibling of an unreadable dir went unwatched");
    }

    #[test]
    fn created_directories_get_watched() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let _watcher = start(dir.path().to_path_buf(), bus).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        // Wait out the debounce flush that arms the new watch.
        std::thread::sleep(Duration::from_millis(600));
        std::fs::write(sub.join("b.txt"), "hello").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            match rx.try_recv() {
                Ok(Event::FileChanged(path)) if path.ends_with("b.txt") => seen = true,
                Ok(Event::FileTreeChanged) => {} // the mkdir itself
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(seen, "file in a just-created directory went unwatched");
    }

    #[test]
    fn end_to_end_modify_event_reaches_bus() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let bus = EventBus::new();
        let rx = bus.subscribe();
        let _watcher = start(dir.path().to_path_buf(), bus).unwrap();
        // Give the watcher a moment to arm, then modify.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&file, "two").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(Event::FileChanged(path)) if path.ends_with("a.txt") => break,
                Ok(Event::FileTreeChanged) => break, // some backends report create
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no watcher event within 5s"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}
