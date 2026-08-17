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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

/// Start watching. The returned watcher must be kept alive for the life of
/// the window.
pub fn start(root: PathBuf, events: EventBus) -> Result<notify::RecommendedWatcher> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    std::thread::Builder::new()
        .name("taste-workspace-watch".into())
        .spawn(move || debounce_loop(root, events, rx))?;
    Ok(watcher)
}

fn debounce_loop(root: PathBuf, events: EventBus, rx: std::sync::mpsc::Receiver<notify::Event>) {
    while let Ok(first) = rx.recv() {
        let mut changed: HashSet<PathBuf> = HashSet::new();
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
