//! One inotify instance for the whole fleet's config watching.
//!
//! Every environment needs to know when its own `.devcontainer/` changes —
//! that is what `Supervisor::recheck` answers and what the drift banner and
//! `devcontainer_reload` are built on. Each supervisor used to open its own
//! `notify` watcher for it, which is one `inotify_init` per environment.
//!
//! **Instances are the scarce resource, and watch descriptors are not.**
//! `fs.inotify.max_user_instances` is 128 and per *uid*; under rootless
//! podman with `--userns=keep-id` the IDE, the user's desktop session,
//! every editor they have open, and every agent in every environment all
//! spend from that one budget — on the machine this was measured on, 67 of
//! the 128 were gone before a single environment came up.
//! `fs.inotify.max_user_watches` was 273731 on the same machine, and one
//! instance may hold as many descriptors as it likes on paths that have
//! nothing to do with each other. So the fleet's cost should scale with the
//! *number of paths* it cares about, not with the number of environments,
//! and one watcher for all of them is the shape that does that.
//!
//! It also removes a silent failure. `start_watching` used to be a
//! per-environment call whose error was logged and dropped, so an
//! exhausted budget meant an environment that quietly stopped noticing
//! config drift — no banner, no toast, and drift is what gates a reload.
//!
//! [`crate::WorkspaceWatcher`]'s slot is the same idea from the other end:
//! one watcher re-aimed at whichever checkout is on screen, rather than one
//! per environment ever opened.
//!
//! # The rule this module exists to keep
//!
//! **Nothing on notify's event-loop thread may call `watch()`, and no lock
//! that thread needs may be held by a caller of `watch()`.**
//!
//! `notify`'s inotify backend runs one thread that both delivers events to
//! the handler and services `watch`/`unwatch`: `watch_inner` posts
//! `EventLoopMsg::AddWatch` and blocks on the reply ("we expect the event
//! loop to live and reply", notify 8.2 `src/inotify.rs`), and
//! `event_handler.handle_event` is called from that same loop. So a handler
//! that calls `watch()` waits for a reply from the thread it is running on,
//! for ever, and a `watch()` caller holding a mutex the handler wants
//! deadlocks the pair.
//!
//! Both were easy to write. The per-supervisor version had the first one
//! shipped: its handler called `recheck`, `recheck` re-arms the
//! `.devcontainer/` watch, and that `watch()` call ran on the handler's own
//! thread — so the FIRST filesystem event in any environment that had a
//! `.devcontainer/` directory froze that environment's watcher permanently.
//! Nobody noticed because the symptom is the absence of one: a dead watcher
//! thread and a config nobody is editing look exactly alike.
//!
//! So: the handler locks [`ConfigWatch::watched`] and nothing else, and
//! hands the recheck to a thread of our own over an unbounded channel —
//! unbounded because a handler that blocks on a send is the same deadlock
//! wearing a different hat. Rechecks then run somewhere that may safely
//! call `watch()`. The watcher itself sits behind its own mutex, which is
//! never held together with `watched`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};

use crate::supervisor::Supervisor;

/// The fleet's config watcher. Owned by the [`crate::EnvironmentRegistry`],
/// which is the only thing that knows what the fleet is.
#[derive(Default)]
pub struct ConfigWatch {
    /// The one instance, created with the first environment and dropped
    /// with the last so an IDE watching nothing holds none.
    ///
    /// Its own lock, held across `watch()` and `unwatch()` — which block on
    /// the event-loop thread — and therefore never held together with
    /// `watched`, which is what that thread needs.
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Which environment owns which root. **The only lock the event
    /// handler takes.**
    watched: Mutex<Vec<Watched>>,
    /// Where the handler posts rechecks so they run off its thread. `None`
    /// until the first environment arrives and again after the last leaves.
    rechecks: Mutex<Option<Sender<Arc<Supervisor>>>>,
}

struct Watched {
    root: PathBuf,
    /// Weak, so a supervisor the registry has forgotten cannot be kept
    /// alive by the watcher that was told about it.
    supervisor: Weak<Supervisor>,
}

impl ConfigWatch {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Watch one environment's config locations, and tell its supervisor
    /// where to come back to when it wants the recursive watch re-armed.
    ///
    /// The root goes on non-recursively — that catches
    /// `.devcontainer.json` beside it, and the creation or removal of
    /// `.devcontainer/` itself — and `.devcontainer/` goes on recursively
    /// whenever it exists ([`Self::arm_devcontainer_dir`]).
    ///
    /// Called by the registry, never from the event-loop thread, which is
    /// what makes the `watch()` calls in here safe.
    pub fn add(self: &Arc<Self>, supervisor: &Arc<Supervisor>) -> Result<()> {
        let root = supervisor.root().to_path_buf();
        // Recorded BEFORE the watch is registered, and with no other lock
        // held: an event can arrive the instant the descriptor exists, and
        // one that arrives before the map knows who owns the path is an
        // event dropped on the floor.
        {
            let mut watched = self.watched.lock().unwrap();
            prune(&mut watched);
            match watched.iter_mut().find(|w| w.root == root) {
                // A re-adopted environment, or a second call for the same
                // one, is an update rather than a second entry — a
                // duplicate would recheck it twice for every event.
                Some(seen) => seen.supervisor = Arc::downgrade(supervisor),
                None => watched.push(Watched {
                    root: root.clone(),
                    supervisor: Arc::downgrade(supervisor),
                }),
            }
        }
        supervisor.set_config_watch(Arc::downgrade(self));
        self.start()?;
        self.watch_path(&root, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching {}", root.display()))?;
        // The same call `recheck` makes, so an environment that already has
        // a `.devcontainer/` is fully armed before anything happens in it.
        self.arm_devcontainer_dir(&root);
        Ok(())
    }

    /// Put the recursive watch back on `<root>/.devcontainer` whenever it
    /// exists. Idempotent: re-watching a watched path updates it.
    ///
    /// Called from `Supervisor::recheck`, on every recheck, and the reason
    /// is a hole that was real: arming this on a *successful* parse meant a
    /// malformed `devcontainer.json` returned early and left the watch
    /// unarmed, so the agent's edits fixing that very file raised no event.
    /// The file the repair loop edits is the one that cannot afford to stop
    /// being watched.
    ///
    /// Safe to call from the recheck thread and from the registry. **Not**
    /// from notify's event-loop thread — see the module's rule — which is
    /// why the handler posts rechecks instead of running them.
    pub fn arm_devcontainer_dir(&self, root: &Path) {
        let dc_dir = root.join(".devcontainer");
        if !dc_dir.is_dir() {
            return;
        }
        let _ = self.watch_path(&dc_dir, RecursiveMode::Recursive);
    }

    /// Stop watching an environment that is gone.
    ///
    /// Its descriptors go with it, and so do the instance and the recheck
    /// thread once the last environment leaves — a dropped `Supervisor` no
    /// longer takes its watcher down with it, so this is the only thing
    /// that can.
    pub fn forget(&self, root: &Path) {
        {
            let mut watcher = self.watcher.lock().unwrap();
            if let Some(watcher) = watcher.as_mut() {
                let _ = watcher.unwatch(root);
                let _ = watcher.unwatch(&root.join(".devcontainer"));
            }
        }
        let empty = {
            let mut watched = self.watched.lock().unwrap();
            watched.retain(|w| w.root != root);
            prune(&mut watched);
            watched.is_empty()
        };
        if empty {
            // The instance goes, and the recheck thread ends when its
            // sender does.
            *self.watcher.lock().unwrap() = None;
            *self.rechecks.lock().unwrap() = None;
        }
    }

    /// How many environments this one instance is watching. For the tests,
    /// and for anything that wants to say so in a log.
    pub fn watching(&self) -> usize {
        let mut watched = self.watched.lock().unwrap();
        prune(&mut watched);
        watched.len()
    }

    /// Create the instance and the recheck thread, once.
    fn start(self: &Arc<Self>) -> Result<()> {
        let mut watcher = self.watcher.lock().unwrap();
        if watcher.is_some() {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel::<Arc<Supervisor>>();
        spawn_recheck_thread(rx)?;
        *self.rechecks.lock().unwrap() = Some(tx.clone());
        let weak = Arc::downgrade(self);
        *watcher = Some(
            // The handler: resolve, post, return. It takes `watched` and
            // nothing else, and `tx` is unbounded, so it cannot block on
            // anything a `watch()` caller could be holding.
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else {
                    return;
                };
                let Some(this) = weak.upgrade() else {
                    return;
                };
                for supervisor in this.owners_of(&event) {
                    let _ = tx.send(supervisor);
                }
            })
            .map_err(taste_core::watcher::name_the_inotify_limit)
            .context("the fleet's config watcher")?,
        );
        Ok(())
    }

    /// One `watch()`, holding the watcher's lock and no other.
    fn watch_path(&self, path: &Path, mode: RecursiveMode) -> Result<()> {
        let mut watcher = self.watcher.lock().unwrap();
        match watcher.as_mut() {
            Some(watcher) => Ok(watcher.watch(path, mode)?),
            // Nothing is watching yet, or the last environment has left.
            None => Ok(()),
        }
    }

    /// Which environments an event's paths belong to, deduplicated.
    fn owners_of(&self, event: &notify::Event) -> Vec<Arc<Supervisor>> {
        let mut watched = self.watched.lock().unwrap();
        prune(&mut watched);
        let mut out: Vec<Arc<Supervisor>> = Vec::new();
        for path in &event.paths {
            let Some(owner) = owner_of(&watched, path) else {
                continue;
            };
            // One event can name several paths in the same environment.
            if !out.iter().any(|seen| Arc::ptr_eq(seen, &owner)) {
                out.push(owner);
            }
        }
        out
    }
}

/// Rechecks, off notify's thread, one at a time, for the life of the
/// [`ConfigWatch`] that feeds it.
///
/// A thread rather than the runtime: `recheck` is blocking filesystem work
/// that ends in a `watch()` call, and it must not be able to occupy a
/// tokio worker or run anywhere near the event loop.
fn spawn_recheck_thread(rx: Receiver<Arc<Supervisor>>) -> Result<()> {
    std::thread::Builder::new()
        .name("taste-config-recheck".into())
        .spawn(move || {
            while let Ok(supervisor) = rx.recv() {
                if let Err(e) = supervisor.recheck() {
                    tracing::warn!("config recheck failed: {e:#}");
                }
            }
        })
        .context("starting the config recheck thread")?;
    Ok(())
}

/// Whose environment does this path belong to?
///
/// Longest matching root wins. Environment clones are siblings under one
/// state directory and the primary is the user's own checkout, so nesting
/// does not arise today — but "the deepest root that contains it" is the
/// answer that stays right if it ever does, and it costs a comparison per
/// environment.
fn owner_of(watched: &[Watched], path: &Path) -> Option<Arc<Supervisor>> {
    watched
        .iter()
        .filter(|w| path.starts_with(&w.root))
        .max_by_key(|w| w.root.as_os_str().len())
        .and_then(|w| w.supervisor.upgrade())
}

/// Drop the entries whose supervisor is gone.
///
/// Their descriptors stay until [`ConfigWatch::forget`] — an environment
/// the registry destroyed goes through it — so this is about not
/// dispatching into nothing, not about reclaiming kernel resources.
fn prune(watched: &mut Vec<Watched>) {
    watched.retain(|w| w.supervisor.strong_count() > 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::EnvironmentIdentity;
    use taste_core::environment::EnvironmentId;
    use taste_core::{EventBus, ExecContext};

    fn supervisor(root: &Path, id: &str) -> Arc<Supervisor> {
        std::fs::create_dir_all(root).unwrap();
        Supervisor::new_outside_container_for_tests(
            EnvironmentIdentity {
                id: EnvironmentId::parse(id).unwrap(),
                workspace_root: root.to_path_buf(),
                root: root.to_path_buf(),
            },
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
            crate::substrate::Substrate::local_for_tests(),
        )
    }

    fn instances(watch: &ConfigWatch) -> usize {
        usize::from(watch.watcher.lock().unwrap().is_some())
    }

    /// A fleet of any size is one inotify instance.
    ///
    /// The count this asserts is the whole point of the module: it used to
    /// be one per environment, on a per-uid budget of 128 shared with the
    /// user's entire desktop session.
    #[test]
    fn every_environment_shares_one_instance() {
        let dir = tempfile::tempdir().unwrap();
        let watch = ConfigWatch::new();
        let mut held = Vec::new();
        for id in ["i-0001", "i-0002", "i-0003"] {
            let root = dir.path().join(id);
            // With a `.devcontainer/` to arm, since that is the path that
            // goes on recursively — and the one whose re-arm deadlocked the
            // per-supervisor version.
            std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
            let supervisor = supervisor(&root, id);
            watch.add(&supervisor).unwrap();
            held.push(supervisor);
        }
        assert_eq!(watch.watching(), 3, "three environments...");
        assert_eq!(
            instances(&watch),
            1,
            "...and one instance between them (the whole point)"
        );
    }

    /// Writing to a watched `.devcontainer/` must not wedge anything.
    ///
    /// This is the module's rule as a test. The handler resolves the owner
    /// and posts the recheck to another thread; a handler that called
    /// `watch()` itself — which re-arming does — would block for ever on a
    /// reply from the thread it was running on, and this test would hang
    /// rather than fail. It is written to be survivable either way: the
    /// assertion is that a second `add` still completes afterwards, which
    /// needs the event loop to still be answering.
    #[test]
    fn an_event_on_a_watched_config_leaves_the_watcher_answering() {
        let dir = tempfile::tempdir().unwrap();
        let watch = ConfigWatch::new();
        let root = dir.path().join("i-0001");
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        let one = supervisor(&root, "i-0001");
        watch.add(&one).unwrap();

        // Churn in the watched directory, which is what the handler wakes
        // on and what a recheck re-arms.
        for n in 0..5 {
            std::fs::write(
                root.join(".devcontainer/devcontainer.json"),
                format!("{{\"n\": {n}}}"),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // The event loop is still servicing AddWatch, which is precisely
        // what a deadlocked one would not be.
        let other = dir.path().join("i-0002");
        let two = supervisor(&other, "i-0002");
        watch.add(&two).unwrap();
        assert_eq!(watch.watching(), 2);
        assert_eq!(instances(&watch), 1);
    }

    /// Adding the same environment twice is an update, not a second entry.
    #[test]
    fn the_same_environment_added_twice_is_watched_once() {
        let dir = tempfile::tempdir().unwrap();
        let watch = ConfigWatch::new();
        let one = supervisor(&dir.path().join("i-0001"), "i-0001");
        watch.add(&one).unwrap();
        watch.add(&one).unwrap();
        assert_eq!(watch.watching(), 1);
    }

    /// The instance goes when the last environment does, so an IDE
    /// watching nothing holds none.
    #[test]
    fn the_last_environment_to_leave_takes_the_instance_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let watch = ConfigWatch::new();
        let one = supervisor(&dir.path().join("i-0001"), "i-0001");
        let two = supervisor(&dir.path().join("i-0002"), "i-0002");
        watch.add(&one).unwrap();
        watch.add(&two).unwrap();
        assert_eq!(instances(&watch), 1);

        watch.forget(one.root());
        assert_eq!(watch.watching(), 1);
        assert_eq!(instances(&watch), 1, "one environment left, one instance");

        watch.forget(two.root());
        assert_eq!(watch.watching(), 0);
        assert_eq!(instances(&watch), 0, "and none left holds none");
    }

    /// An event under an environment's root is that environment's, and the
    /// deepest root containing the path is the one that owns it.
    #[test]
    fn a_path_belongs_to_the_deepest_root_that_contains_it() {
        let dir = tempfile::tempdir().unwrap();
        let outer = supervisor(&dir.path().join("outer"), "i-0001");
        let nested = supervisor(&dir.path().join("nested"), "i-0002");
        let watched = vec![
            Watched {
                root: PathBuf::from("/w"),
                supervisor: Arc::downgrade(&outer),
            },
            Watched {
                root: PathBuf::from("/w/nested/repo"),
                supervisor: Arc::downgrade(&nested),
            },
        ];

        let hit = owner_of(
            &watched,
            Path::new("/w/nested/repo/.devcontainer/devcontainer.json"),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&hit, &nested), "the nested one owns it");
        let hit = owner_of(&watched, Path::new("/w/.devcontainer/devcontainer.json"));
        assert!(Arc::ptr_eq(&hit.unwrap(), &outer));
        assert!(
            owner_of(&watched, Path::new("/somewhere/else")).is_none(),
            "and a path under neither belongs to nobody"
        );
    }

    /// A supervisor the registry has dropped is not kept alive by the
    /// watcher that was told about it, and stops being dispatched to.
    #[test]
    fn a_dropped_supervisor_is_pruned_rather_than_held_alive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("i-0001");
        let watch = ConfigWatch::new();
        let supervisor = supervisor(&root, "i-0001");
        watch.add(&supervisor).unwrap();
        assert_eq!(watch.watching(), 1);

        drop(supervisor);
        assert_eq!(watch.watching(), 0, "pruned, not resurrected");
        // ...and forgetting it afterwards is still safe.
        watch.forget(&root);
        assert_eq!(instances(&watch), 0);
    }
}
