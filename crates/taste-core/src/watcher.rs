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

/// A running watch. **Dropping it stops the watch and ends its thread.**
///
/// That is the whole of the type, and it is load-bearing: the handle holds
/// the only strong reference to the underlying notify watcher, and the
/// worker thread holds a [`Weak`] one. So a drop releases the watcher,
/// which drops the closure holding the channel sender, which ends the
/// thread's `recv` loop. A per-environment watcher that outlived its
/// watching would leak a thread and an inotify budget per environment the
/// user ever glanced at.
pub struct WorkspaceWatcher {
    /// Never read — held to be dropped. See above.
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
}

/// At most one *extra* watcher, aimed wherever the user is watching.
///
/// The workspace's own watcher runs for the life of the window; this is the
/// second one, and it exists only while the panes are aimed at another
/// environment's clone (ENVIRONMENTS.md → "Watching an environment": the
/// clone gets a watcher *while, and only while, it is watched*). N
/// environments each with a live recursive watch would be N tree walks and
/// N inotify budgets spent on directories nobody is looking at.
///
/// Aiming it somewhere new drops the previous watcher first, so the slot
/// can never accumulate two.
pub struct WatchSlot {
    events: EventBus,
    current: Option<(PathBuf, WorkspaceWatcher)>,
}

impl WatchSlot {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            current: None,
        }
    }

    /// What is being watched, if anything.
    pub fn root(&self) -> Option<&Path> {
        self.current.as_ref().map(|(root, _)| root.as_path())
    }

    /// Aim the slot at `root`, or drop it with `None`.
    ///
    /// Re-aiming at the root already watched is a no-op: returning to a
    /// watched environment must not cost a fresh tree walk.
    pub fn aim(&mut self, root: Option<PathBuf>) {
        match root {
            Some(root) => {
                if self.root() == Some(root.as_path()) {
                    return;
                }
                // Dropped BEFORE the new one starts: two live watchers over
                // the same tree would double every event.
                self.current = None;
                match start(root.clone(), self.events.clone()) {
                    Ok(watcher) => self.current = Some((root, watcher)),
                    Err(e) => tracing::warn!("watching {} failed: {e:#}", root.display()),
                }
            }
            None => self.current = None,
        }
    }
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
    // The thread gets a WEAK reference on purpose: the handle's drop must
    // be what stops the watch, and a strong reference in here would keep
    // both the watcher and this thread alive for the life of the process.
    let watcher = Arc::downgrade(&watcher);
    std::thread::Builder::new()
        .name("taste-workspace-watch".into())
        .spawn(move || {
            // .git gets shallow watches: index/HEAD/packed-refs and branch
            // tips cover "an agent committed"; object churn is noise.
            for git_dir in [".git", ".git/refs", ".git/refs/heads"] {
                let Some(alive) = watcher.upgrade() else {
                    return; // dropped before we even armed
                };
                let _ = alive
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
///
/// The walk gives up the moment the watcher is dropped: opening an
/// environment and closing it again must not leave a tree walk running.
fn add_dir_watches(
    watcher: &std::sync::Weak<Mutex<notify::RecommendedWatcher>>,
    root: &Path,
    start_dir: PathBuf,
) {
    let mut stack = vec![start_dir];
    while let Some(dir) = stack.pop() {
        if dir != *root && !matches!(classify(root, &dir), Classification::Workspace) {
            continue;
        }
        let Some(alive) = watcher.upgrade() else {
            return;
        };
        if alive
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
    watcher: std::sync::Weak<Mutex<notify::RecommendedWatcher>>,
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
                            // A safe save is write-temp-then-rename, so the
                            // real file's content change arrives as a
                            // STRUCTURAL event. Reporting only the tree
                            // refresh left open buffers stale against every
                            // tool that saves that way — the agent included.
                            // `is_file` costs one stat and excludes both
                            // directories (no content) and the vanished half
                            // of a rename (nothing to reload).
                            if path.is_file() {
                                changed.insert(path);
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
        // Dropped while this burst was being absorbed: the events belong to
        // something nobody is looking at any more.
        if watcher.upgrade().is_none() {
            return;
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
    fn atomic_save_reports_the_file_as_changed() {
        // Write-temp-then-rename is how careful tools save — editors, and
        // the agent's own file writes. The rename is a STRUCTURAL event, so
        // this used to publish FileTreeChanged alone and leave every open
        // buffer stale. FileChanged for the renamed-onto path is the fix.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let temp = dir.path().join("a.txt.tmp");
        std::fs::write(&file, "one").unwrap();

        let bus = EventBus::new();
        let rx = bus.subscribe();
        let _watcher = start(dir.path().to_path_buf(), bus).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&temp, "two").unwrap();
        std::fs::rename(&temp, &file).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(Event::FileChanged(path)) if path.ends_with("a.txt") => break,
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "atomic save never reported a.txt as changed"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Wait for a file-content event about `name`, or give up.
    fn saw_change(rx: &async_channel::Receiver<Event>, name: &str, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(Event::FileChanged(path)) if path.ends_with(name) => return true,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        false
    }

    /// The watching contract, both halves: a watched environment gets a
    /// watcher, and one that is no longer watched gets none. The second
    /// half is the one that matters — a slot that kept its watcher would
    /// leak a thread and a tree walk per environment ever opened.
    #[test]
    fn the_slot_watches_only_while_aimed_at_something() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let mut slot = WatchSlot::new(bus);
        assert!(slot.root().is_none(), "nothing is watched to begin with");

        slot.aim(Some(dir.path().to_path_buf()));
        assert_eq!(slot.root(), Some(dir.path()));
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&file, "two").unwrap();
        assert!(
            saw_change(&rx, "a.txt", Duration::from_secs(5)),
            "a watched clone must report its edits"
        );

        // Re-aiming at the same root keeps the running watcher (no walk),
        // and it keeps working.
        slot.aim(Some(dir.path().to_path_buf()));
        std::fs::write(&file, "three").unwrap();
        assert!(saw_change(&rx, "a.txt", Duration::from_secs(5)));

        // Returning to the primary drops it: nothing arrives afterwards.
        slot.aim(None);
        assert!(slot.root().is_none());
        while rx.try_recv().is_ok() {} // drain the debounce tail
        std::fs::write(&file, "four").unwrap();
        assert!(
            !saw_change(&rx, "a.txt", Duration::from_millis(1500)),
            "an unwatched clone must go quiet"
        );
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
