//! Logs kept on disk, for the questions asked after the IDE has exited.
//!
//! The supervisor's log and the app log are rings in memory, which answer
//! "what is happening" and lose "what happened" with the process: a launch
//! that sat refused beside a container that was up could not be traced
//! once the window had closed, because every line that would have said
//! which path it took was gone with it (2026-10-02). A [`LogFile`] is the
//! same lines, also appended to a file.
//!
//! Each file has a writer thread of its own, so a caller never waits on
//! the disk: lines are logged from the GTK thread too, which must not do
//! file IO (CLAUDE.md → Performance). A file past [`MAX_BYTES`] is rotated
//! to `<name>.1`, overwriting the one before, so a log costs at most twice
//! that. A directory removed under an open log — an environment destroyed
//! — is never recreated: the writer keeps its open file until the last
//! handle is dropped, and a rotation that cannot reopen stops writing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// The size a log is rotated at.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// One log on disk. Cheap to log to from any thread; dropping the last
/// handle ends its writer once what was sent is written.
pub struct LogFile {
    lines: Mutex<Sender<String>>,
}

impl LogFile {
    /// Open `path` for appending, making its directory, with a writer
    /// thread behind it. `None` when the file cannot be opened — a log
    /// that cannot be kept is not a reason for anything else to fail.
    pub fn open(path: impl Into<PathBuf>) -> Option<Arc<Self>> {
        Self::open_rotating_at(path.into(), MAX_BYTES)
    }

    fn open_rotating_at(path: PathBuf, max_bytes: u64) -> Option<Arc<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        let file = append(&path).ok()?;
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("taste-logfile".into())
            .spawn(move || write_lines(path, file, rx, max_bytes))
            .ok()?;
        Some(Arc::new(Self {
            lines: Mutex::new(tx),
        }))
    }

    /// Append one line, stamped with the moment it was logged rather than
    /// the moment it reached the disk.
    pub fn line(&self, line: &str) {
        let stamped = format!("{} {}\n", stamp(), line.trim_end());
        let _ = self.lines.lock().unwrap().send(stamped);
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn write_lines(path: PathBuf, mut file: File, rx: Receiver<String>, max_bytes: u64) {
    let mut size = file.metadata().map(|m| m.len()).unwrap_or(0);
    while let Ok(first) = rx.recv() {
        // Everything already waiting goes in one flush: a build's burst of
        // output is one write, not one per line.
        let mut batch = first;
        while let Ok(next) = rx.try_recv() {
            batch.push_str(&next);
        }
        if size > 0 && size + batch.len() as u64 > max_bytes {
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            let _ = std::fs::rename(&path, &rotated);
            // Opened without making the directory: one removed under this
            // log stays removed, and the writer stops.
            match append(&path) {
                Ok(fresh) => {
                    file = fresh;
                    size = 0;
                }
                Err(_) => return,
            }
        }
        if file.write_all(batch.as_bytes()).is_err() {
            return;
        }
        let _ = file.flush();
        size += batch.len() as u64;
    }
}

/// UTC to the millisecond, RFC 3339 (`2026-10-02T11:04:05.123Z`).
fn stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = crate::state::rfc3339_from_unix(now.as_secs());
    format!(
        "{}.{:03}Z",
        seconds.trim_end_matches('Z'),
        now.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines arrive after the writer drains them; the handle's drop is the
    /// only ordering a caller gets, so the tests wait on the file.
    fn wait_for(path: &Path, needle: &str) -> String {
        for _ in 0..200 {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{needle:?} never reached {}", path.display());
    }

    #[test]
    fn lines_are_stamped_and_appended_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env").join("supervisor.log");
        LogFile::open(&path).unwrap().line("first");
        wait_for(&path, "first");
        LogFile::open(&path).unwrap().line("second\n");
        let text = wait_for(&path, "second");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].ends_with(" first"), "{text}");
        assert!(lines[1].ends_with(" second"), "{text}");
        // 2026-10-02T11:04:05.123Z
        let stamp = lines[0].split(' ').next().unwrap();
        assert_eq!(stamp.len(), 24, "{stamp}");
        assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");
    }

    #[test]
    fn a_full_log_rotates_to_one_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let log = LogFile::open_rotating_at(path.clone(), 100).unwrap();
        log.line(&"a".repeat(80));
        wait_for(&path, "aaaa");
        log.line(&"b".repeat(80));
        wait_for(&path, "bbbb");
        let previous = std::fs::read_to_string(dir.path().join("app.log.1")).unwrap();
        assert!(previous.contains("aaaa") && !previous.contains("bbbb"));
        let current = std::fs::read_to_string(&path).unwrap();
        assert!(!current.contains("aaaa"), "{current}");
    }

    #[test]
    fn a_removed_directory_is_not_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("env");
        let path = env.join("supervisor.log");
        let log = LogFile::open_rotating_at(path.clone(), 100).unwrap();
        log.line(&"a".repeat(80));
        wait_for(&path, "aaaa");
        std::fs::remove_dir_all(&env).unwrap();
        // Past the limit: a rotation, which cannot reopen and stops.
        log.line(&"b".repeat(80));
        drop(log);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !env.exists(),
            "the destroyed environment's directory came back"
        );
    }
}
