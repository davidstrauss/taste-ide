//! **The files on their way between the folder and Personal's checkout**,
//! for the file tree's progress rings (`taste_core::Event::SyncFiles`).
//!
//! A path is marked the moment either side's watcher sees it change, so
//! the ring is up before the sync's pass begins, and it fills as the pass
//! moves it. What can be measured is measured, and what cannot is said to
//! be shared:
//!
//! - **Up**, the folder's file to the checkout, goes through the files
//!   service a chunk at a time, so each file's ring is its own bytes
//!   (`Files::write_reporting`).
//! - **Down**, the checkout's file to the folder, arrives inside one git
//!   fetch, and git reports only the pack as a whole: each file coming down
//!   is sized in the checkout before the fetch (`size_down`), and wears the
//!   fetch's progress against that size, marked `shared` — what is left of
//!   it is an estimate, and the time is the fetch's.
//!
//! A submodule syncs as a pair of its own, with a fetch of its own: its
//! pass sees the tracker [`Transfers::within`] it, naming paths as the
//! submodule does while the tree's rings are keyed from the root, and its
//! fetch's progress goes on its files alone, not on the parent's.
//!
//! A pass that writes a file sets the other side's watcher off over that
//! same file. Those echoes are not changes to send back, so a path that
//! just moved is not marked going the other way for a few seconds
//! (`ECHO`). The set clears when a pass ends: anything still in it was a
//! change the pass found nothing to move for — a build's output nobody
//! syncs, say.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use taste_core::{Event, EventBus, SyncFile};

/// How long a path that moved is deaf to its own echo on the other side.
const ECHO: Duration = Duration::from_secs(5);

/// The longest between two publishes of the same set's progress; a path
/// joining or leaving the set is published at once.
const PACE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
struct Moving {
    up: bool,
    fraction: Option<f64>,
    done: u64,
    total: Option<u64>,
    /// When it started moving, for its rate.
    since: Option<Instant>,
    shared: bool,
}

#[derive(Debug, Default)]
struct Inner {
    /// The submodules seen syncing, by path from the root: a file under
    /// one is that submodule's fetch's to report on.
    scopes: std::collections::BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, Moving>,
    /// Paths that moved, the direction they moved in, and when.
    landed: HashMap<PathBuf, (bool, Instant)>,
    published: Option<Instant>,
    /// The fetch under way: when it began, for its rate.
    fetch_since: Option<Instant>,
}

/// The tracker: one per primary environment, cheap to clone.
#[derive(Clone)]
pub struct Transfers {
    events: EventBus,
    inner: Arc<Mutex<Inner>>,
    /// Where the pass using this handle is, from the root: empty for the
    /// parent, a submodule's path for its pass (`within`).
    scope: PathBuf,
}

impl Transfers {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            inner: Arc::default(),
            scope: PathBuf::new(),
        }
    }

    /// The same tracker, for the pass of the submodule at `sub` (from this
    /// handle's scope): its paths are the submodule's, and its fetch
    /// reports on the files inside it.
    pub fn within(&self, sub: &Path) -> Self {
        let scope = self.scope.join(sub);
        self.inner.lock().unwrap().scopes.insert(scope.clone());
        Self {
            events: self.events.clone(),
            inner: self.inner.clone(),
            scope,
        }
    }

    /// Whether `path` (from the root) is this handle's fetch's to report
    /// on: inside its scope, and in no submodule deeper than it.
    fn owns(&self, inner: &Inner, path: &Path) -> bool {
        if !path.starts_with(&self.scope) {
            return false;
        }
        !inner.scopes.iter().any(|scope| {
            scope != &self.scope && scope.starts_with(&self.scope) && path.starts_with(scope)
        })
    }

    /// A watcher saw `path` change: `up` when it was the folder's.
    pub fn changed(&self, path: &Path, up: bool) {
        let mut inner = self.inner.lock().unwrap();
        if let Some((moved_up, at)) = inner.landed.get(path) {
            if *moved_up != up && at.elapsed() < ECHO {
                return;
            }
        }
        // Already on its way, either way: a file the pass is writing is
        // not also waiting.
        if inner.files.contains_key(path) {
            return;
        }
        inner.files.insert(
            path.to_path_buf(),
            Moving {
                up,
                fraction: None,
                done: 0,
                total: None,
                since: None,
                shared: false,
            },
        );
        self.publish(&mut inner, true);
    }

    /// `path` going up: `done` of its `total` bytes have gone.
    pub fn sending(&self, path: &Path, done: u64, total: u64) {
        let path = &self.scope.join(path);
        let mut inner = self.inner.lock().unwrap();
        let joined = !inner.files.contains_key(path);
        let moving = inner
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| Moving {
                up: true,
                fraction: None,
                done: 0,
                total: None,
                since: None,
                shared: false,
            });
        moving.up = true;
        moving.since.get_or_insert_with(Instant::now);
        moving.done = done;
        moving.total = Some(total);
        moving.fraction = Some(if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        });
        self.publish(&mut inner, joined);
    }

    /// `path` has arrived, in the direction `up`.
    pub fn landed(&self, path: &Path, up: bool) {
        let path = &self.scope.join(path);
        let mut inner = self.inner.lock().unwrap();
        inner.files.remove(path);
        inner
            .landed
            .insert(path.to_path_buf(), (up, Instant::now()));
        self.publish(&mut inner, true);
    }

    /// What is coming down, sized where it is before the fetch brings it:
    /// `size` reads one path's bytes in the checkout.
    pub fn size_down(&self, size: impl Fn(&Path) -> Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        let mine: Vec<PathBuf> = inner
            .files
            .iter()
            .filter(|(path, m)| !m.up && m.total.is_none() && self.owns(&inner, path))
            .map(|(path, _)| path.clone())
            .collect();
        for path in mine {
            let bytes = path.strip_prefix(&self.scope).ok().and_then(&size);
            if let Some(moving) = inner.files.get_mut(&path) {
                moving.total = bytes;
            }
        }
    }

    /// The fetch bringing the checkout's files down is `percent` through
    /// its objects, with `received` bytes in.
    pub fn fetching(&self, percent: Option<u8>, received: Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        let since = *inner.fetch_since.get_or_insert_with(Instant::now);
        let mine: std::collections::HashSet<PathBuf> = inner
            .files
            .keys()
            .filter(|path| self.owns(&inner, path))
            .cloned()
            .collect();
        let mut any = false;
        for (_, moving) in inner
            .files
            .iter_mut()
            .filter(|(path, m)| !m.up && mine.contains(*path))
        {
            moving.shared = true;
            moving.since = Some(since);
            if let Some(percent) = percent {
                let fraction = f64::from(percent) / 100.0;
                moving.fraction = Some(fraction);
                // This file's share of the way, against its own size.
                if let Some(total) = moving.total {
                    moving.done = (total as f64 * fraction) as u64;
                }
            } else if let (Some(received), None) = (received, moving.total) {
                moving.done = received;
            }
            any = true;
        }
        if any {
            self.publish(&mut inner, false);
        }
    }

    /// A pass ended: what came down has landed, and anything still waiting
    /// had nothing to move.
    pub fn settle(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let files = std::mem::take(&mut inner.files);
        // What was coming down came down with the pass, whether or not the
        // fetch was big enough to report on; what was going up moved only
        // if it was sent.
        for (path, moving) in files {
            if !moving.up || moving.fraction.is_some() {
                inner.landed.insert(path, (moving.up, now));
            }
        }
        inner.fetch_since = None;
        inner.landed.retain(|_, (_, at)| at.elapsed() < ECHO);
        self.publish(&mut inner, true);
    }

    fn publish(&self, inner: &mut Inner, now: bool) {
        if !now && inner.published.is_some_and(|at| at.elapsed() < PACE) {
            return;
        }
        inner.published = Some(Instant::now());
        let files = inner
            .files
            .iter()
            .map(|(path, moving)| (path.clone(), file_of(moving)))
            .collect();
        self.events.publish(Event::SyncFiles(Arc::new(files)));
    }
}

/// What the tree is told about one file: how far, and how long to go from
/// the rate so far.
fn file_of(moving: &Moving) -> SyncFile {
    let left = moving.total.map(|total| total.saturating_sub(moving.done));
    let seconds_left = match (moving.fraction, moving.since) {
        (Some(fraction), Some(since)) if fraction > 0.0 && fraction < 1.0 => {
            let elapsed = since.elapsed().as_secs_f64();
            // Not before a second of it: a rate from the first chunk is a
            // guess.
            (elapsed >= 1.0).then(|| (elapsed * (1.0 - fraction) / fraction).ceil() as u64)
        }
        _ => None,
    };
    SyncFile {
        up: moving.up,
        fraction: moving.fraction,
        left,
        moved: moving.done,
        seconds_left,
        shared: moving.shared,
    }
}

/// git's way of saying how much has crossed (`40.20 MiB`, `512 bytes`), in
/// bytes.
pub fn git_bytes(said: &str) -> Option<u64> {
    let (number, unit) = said.trim().split_once(' ')?;
    let number: f64 = number.parse().ok()?;
    let factor = match unit.trim() {
        "bytes" | "byte" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_set(rx: &async_channel::Receiver<Event>) -> BTreeMap<PathBuf, SyncFile> {
        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            if let Event::SyncFiles(files) = event {
                last = Some((*files).clone());
            }
        }
        last.expect("a publish")
    }

    /// A change is marked waiting, fills as it is sent, and is gone when it
    /// lands; the other side's echo of that write is not marked.
    #[test]
    fn a_change_waits_moves_lands_and_its_echo_is_ignored() {
        let events = EventBus::new();
        let rx = events.subscribe();
        let transfers = Transfers::new(events);
        let path = Path::new("dist/emet");
        transfers.changed(path, true);
        let waiting = last_set(&rx);
        assert_eq!(waiting[path].fraction, None);
        assert!(waiting[path].up);

        transfers.sending(path, 0, 400);
        transfers.landed(path, true);
        assert!(last_set(&rx).is_empty());

        // The checkout's watcher hears the write the sync just made.
        transfers.changed(path, false);
        assert!(rx.try_recv().is_err(), "an echo is not a change");
    }

    /// A submodule's pass names its paths as the submodule does, and its
    /// fetch reports on its own files, never the parent's.
    #[test]
    fn a_submodules_pass_is_scoped_to_its_files() {
        let events = EventBus::new();
        let rx = events.subscribe();
        let transfers = Transfers::new(events);
        let sub = transfers.within(Path::new("ext/lib"));
        transfers.changed(Path::new("README.md"), false);
        transfers.changed(Path::new("ext/lib/src/x.rs"), false);
        sub.size_down(|path| {
            assert_eq!(path, Path::new("src/x.rs"), "named as the submodule does");
            Some(10 << 20)
        });
        transfers.size_down(|path| {
            assert_eq!(path, Path::new("README.md"), "the parent's own only");
            Some(1 << 20)
        });
        std::thread::sleep(PACE);
        sub.fetching(Some(50), None);
        let set = last_set(&rx);
        assert_eq!(set[Path::new("ext/lib/src/x.rs")].fraction, Some(0.5));
        assert_eq!(set[Path::new("README.md")].fraction, None);
        sub.sending(Path::new("src/y.rs"), 1, 2);
        assert!(last_set(&rx).contains_key(Path::new("ext/lib/src/y.rs")));
    }

    /// What comes down wears the fetch's progress, said to be shared, and
    /// the set is empty once the pass settles.
    #[test]
    fn what_comes_down_shares_the_fetch_and_settles() {
        let events = EventBus::new();
        let rx = events.subscribe();
        let transfers = Transfers::new(events);
        transfers.changed(Path::new("a.txt"), false);
        transfers.changed(Path::new("b.txt"), false);
        transfers.size_down(|path| (path == Path::new("a.txt")).then_some(8 << 20));
        transfers.fetching(Some(40), Some(4 << 20));
        std::thread::sleep(PACE);
        transfers.fetching(Some(50), Some(5 << 20));
        let set = last_set(&rx);
        for file in set.values() {
            assert!(file.shared && !file.up);
            assert_eq!(file.fraction, Some(0.5));
        }
        // What is left of a file is its own size, as far as the fetch is.
        assert_eq!(set[Path::new("a.txt")].left, Some(4 << 20));
        transfers.settle();
        assert!(last_set(&rx).is_empty());
        assert_eq!(git_bytes("40.20 MiB"), Some(42_152_755));
        assert_eq!(git_bytes("512 bytes"), Some(512));
    }
}
