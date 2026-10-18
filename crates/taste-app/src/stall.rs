//! The main thread's stalls, found and named in the log.
//!
//! The main thread must never block or grind (CLAUDE.md → Performance), and
//! when it does, the user sees GNOME offer to kill the app with nothing to
//! say why (2026-10-17: "It was frozen long enough to trigger GNOME's
//! question to kill the app … Way too long"). No debugger reaches this
//! process from where it is built, so the app watches itself: the main loop
//! beats every [`BEAT`], a thread notices when the beats stop for
//! [`STALLED`], and the log says what the main thread was doing — the
//! event or handler marked with [`doing`] — and, when it comes back, how
//! long it was gone. A marked piece of work that runs long without stalling
//! anything for that long is logged too, past [`SLOW`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gtk::glib;

/// How often the main loop says it is alive.
const BEAT: Duration = Duration::from_millis(100);
/// A main loop silent this long is stalled.
const STALLED: Duration = Duration::from_millis(1000);
/// A marked piece of work this long is logged when it ends.
const SLOW: Duration = Duration::from_millis(200);

/// Milliseconds since [`start`], at the main loop's last beat.
static LAST_BEAT: AtomicU64 = AtomicU64::new(0);
/// What the main thread is doing now, and since when; `None` between
/// marked pieces of work.
static DOING: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// Start the beat and its watcher. Call once, on the main thread.
pub fn start() {
    LAST_BEAT.store(now_ms(), Ordering::Relaxed);
    glib::timeout_add_local(BEAT, || {
        LAST_BEAT.store(now_ms(), Ordering::Relaxed);
        glib::ControlFlow::Continue
    });
    let spawned = std::thread::Builder::new()
        .name("taste-stall".into())
        .spawn(|| {
            let mut stalled_since: Option<u64> = None;
            loop {
                std::thread::sleep(BEAT * 2);
                let last = LAST_BEAT.load(Ordering::Relaxed);
                let silent = now_ms().saturating_sub(last);
                match stalled_since {
                    None if silent >= STALLED.as_millis() as u64 => {
                        stalled_since = Some(last);
                        tracing::warn!(
                            "main thread stalled for {silent} ms, doing: {}",
                            doing_now()
                        );
                    }
                    Some(since) if last > since => {
                        tracing::warn!("main thread back after {} ms", last.saturating_sub(since));
                        stalled_since = None;
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("the stall watcher did not start: {e}");
    }
}

fn doing_now() -> String {
    match DOING.lock().unwrap().as_ref() {
        Some((what, since)) => format!("{what} (for {} ms)", since.elapsed().as_millis()),
        None => "nothing marked — GTK's own layout, drawing, or an unmarked signal".into(),
    }
}

/// Marks the main thread as doing `what` until it is dropped; logs it if it
/// ran past [`SLOW`]. Nested marks keep the outermost.
pub struct Doing {
    outer: bool,
    started: Instant,
}

/// Mark what the main thread is about to do.
pub fn doing(what: impl FnOnce() -> String) -> Doing {
    let mut slot = DOING.lock().unwrap();
    let outer = slot.is_none();
    if outer {
        *slot = Some((what(), Instant::now()));
    }
    Doing {
        outer,
        started: Instant::now(),
    }
}

impl Drop for Doing {
    fn drop(&mut self) {
        if !self.outer {
            return;
        }
        let what = DOING.lock().unwrap().take();
        let took = self.started.elapsed();
        if took >= SLOW {
            if let Some((what, _)) = what {
                tracing::warn!("main thread: {what} took {} ms", took.as_millis());
            }
        }
    }
}

/// `value`'s Debug text, stopped at `max` characters: a label for a mark,
/// cheap however large the value is.
pub fn short_debug(value: &impl std::fmt::Debug, max: usize) -> String {
    struct Capped {
        text: String,
        max: usize,
    }
    impl std::fmt::Write for Capped {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            for c in s.chars() {
                if self.text.len() >= self.max {
                    self.text.push('…');
                    return Err(std::fmt::Error);
                }
                self.text.push(c);
            }
            Ok(())
        }
    }
    let mut capped = Capped {
        text: String::new(),
        max,
    };
    let _ = std::fmt::write(&mut capped, format_args!("{value:?}"));
    capped.text
}

/// Lets a loop on the main thread give GTK its turn. An event loop that
/// finds its channel full never awaits anything that waits, so it ran a
/// chat's whole replay — thousands of updates — without one frame drawn or
/// one key read: 6.6 s frozen at startup, which this module's log named
/// (2026-10-17). Once a loop has worked [`PACE`] straight, [`Pacer::breathe`]
/// waits a turn at idle priority, behind GTK's redraw and input, which
/// outrank ordinary work and would never run behind a default-priority
/// yield.
pub struct Pacer {
    since: Instant,
}

/// How long a main-thread loop works before it lets a frame through.
const PACE: Duration = Duration::from_millis(8);

impl Pacer {
    pub fn new() -> Self {
        Self {
            since: Instant::now(),
        }
    }

    /// Give GTK a turn if this loop has had the thread for [`PACE`].
    pub async fn breathe(&mut self) {
        if self.since.elapsed() >= PACE {
            glib::timeout_future_with_priority(glib::Priority::DEFAULT_IDLE, Duration::ZERO).await;
            self.since = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_cut_short_however_large_the_value() {
        let big: Vec<u32> = (0..100_000).collect();
        let label = short_debug(&big, 20);
        assert!(label.chars().count() <= 21, "{label}");
        assert!(label.ends_with('…'));
        assert_eq!(short_debug(&"hi", 20), "\"hi\"");
    }
}
