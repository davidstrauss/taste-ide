//! The IDE's own runtime log, kept where agents can read it.
//!
//! GTK and GLib complain to a structured log the user rarely sees — unknown
//! CSS properties, missing icons, unparented widgets. Those warnings answer
//! exactly the questions an agent has after a UI change, so `taste-app`
//! mirrors them (and its own `tracing` output) into this process-global
//! ring buffer, and the MCP server serves the tail as `ide_app_log`.
//!
//! Process-global on purpose: one window is one process, and log writers
//! are installed once per process, before any workspace exists.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const CAPACITY: usize = 1000;

fn buffer() -> &'static Mutex<VecDeque<String>> {
    static BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// Wall-clock HH:MM:SS (UTC) — enough to correlate with a user's "just
/// now"; full dates would be noise in a same-session ring buffer.
pub fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Append one line. Callable from any thread (GLib log writers run on
/// whichever thread logged).
pub fn push(level: &str, source: &str, message: &str) {
    let line = format!("{} {:5} {}: {}", clock(), level, source, message.trim_end());
    let mut buffer = buffer().lock().unwrap();
    if buffer.len() >= CAPACITY {
        buffer.pop_front();
    }
    buffer.push_back(line);
}

/// The most recent `n` lines, oldest first.
pub fn tail(n: usize) -> Vec<String> {
    let buffer = buffer().lock().unwrap();
    buffer
        .iter()
        .skip(buffer.len().saturating_sub(n))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_newest_lines_and_caps_growth() {
        for i in 0..(CAPACITY + 10) {
            push("WARN", "test", &format!("line {i}"));
        }
        let tail = tail(2);
        assert_eq!(tail.len(), 2);
        assert!(tail[1].ends_with(&format!("line {}", CAPACITY + 9)));
        assert!(tail[0].ends_with(&format!("line {}", CAPACITY + 8)));
        // The buffer never exceeds its capacity.
        assert_eq!(super::tail(usize::MAX).len(), CAPACITY);
    }
}
