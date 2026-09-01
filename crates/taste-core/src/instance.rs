//! One folder, one supervisor.
//!
//! Each `taste-ide <folder>` is its own process and window (`main.rs`,
//! NON_UNIQUE), which is the point: a person works on several projects at
//! once. Every podman-visible name, socket and state path is keyed by
//! [`crate::environment::workspace_key`], so N windows on N folders never
//! meet.
//!
//! N windows on the SAME folder is the degenerate case, and it is the one
//! keying cannot answer — the key is the folder, so both windows compute
//! the same container names, the same volumes, the same fleet socket, the
//! same build staging directory. Two supervisors then fight: one window's
//! reload force-removes the container the other is streaming logs from, one
//! window's staging wipe lands mid-`podman build` in the other, and both
//! bind the socket an agent dials.
//!
//! There is no arbitration that makes two supervisors correct, so there is
//! no arbitration. **The first window to open a folder supervises it; a
//! second window opens for editing and supervises nothing.** Editing is
//! genuinely useful on its own — files, git, search, the editor — and it is
//! the half that has no shared mutable state behind it. The second window
//! says so once, plainly, and does not nag.
//!
//! The mechanism is an advisory `flock` on a file under the state
//! directory, held for as long as the process lives. It is the right tool
//! for precisely the reason a pid file is the wrong one: the kernel drops
//! it when the process dies, however it dies, so a crashed window never
//! leaves a folder permanently unsupervisable. The pid inside the file is
//! written for a human reading `ls`, and for naming the other window in the
//! notice — never as the lock itself.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// `$XDG_STATE_HOME/taste-ide/supervision` — one lock file per workspace.
///
/// Deliberately not the `workspaces/` directory that holds restore state:
/// that directory's contents are the user's session and are worth backing
/// up, and these are runtime detritus that mean nothing after a reboot.
pub fn supervision_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/state")
        })
        .join("taste-ide")
        .join("supervision")
}

fn lock_path(base: &Path, workspace_root: &Path) -> PathBuf {
    base.join(format!(
        "{}.lock",
        crate::environment::workspace_key(workspace_root)
    ))
}

/// A held claim on a workspace's supervision.
///
/// The lock lives as long as this value does, and the process's exit is what
/// releases it in the normal case — so the window holds one for its whole
/// life rather than taking and dropping it around operations. Dropping it
/// early is not an error, it is resignation: whatever this window was
/// supervising, it no longer is.
#[derive(Debug)]
pub struct SupervisionLock {
    /// Held open purely for the `flock` on it. Closing releases.
    _file: File,
    path: PathBuf,
}

impl SupervisionLock {
    /// The lock file, for a person looking at what a window has claimed.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SupervisionLock {
    fn drop(&mut self) {
        // The file stays; the lock goes with the descriptor. Leaving the
        // (empty-ish) file behind is deliberate — unlinking it would race a
        // window that has already opened it and is about to lock it, and
        // hand two windows the supervision of one folder, which is the one
        // outcome this module exists to prevent.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path);
    }
}

/// Whether this window supervises the folder it has open.
#[derive(Debug)]
pub enum Supervision {
    /// This window is the supervisor. Environments, reconciliation, the
    /// registry and the fleet socket are all its business.
    Granted(SupervisionLock),
    /// Another live window already has this folder. This one edits and
    /// supervises nothing.
    HeldElsewhere {
        /// The other window's pid, when it managed to record one. A hint
        /// for the notice, never a fact anything branches on — the lock is
        /// the truth, and the pid can be stale by the time it is read.
        pid: Option<u32>,
    },
}

impl Supervision {
    pub fn is_granted(&self) -> bool {
        matches!(self, Self::Granted(_))
    }

    /// What to tell the user, once, when this window is not the supervisor.
    ///
    /// Calm and specific: it names what still works, because almost
    /// everything does. A person who opened the same project twice on
    /// purpose — a second window on another monitor — has done nothing
    /// wrong and should not be made to feel they have.
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Granted(_) => None,
            Self::HeldElsewhere { pid } => Some(format!(
                "Environments for this folder are managed by another \
                 taste-ide window{}. This window edits, searches and commits \
                 normally; containers, agents and the fleet belong to that one.",
                match pid {
                    Some(pid) => format!(" (process {pid})"),
                    None => String::new(),
                }
            )),
        }
    }
}

/// Claim supervision of a workspace, without blocking.
///
/// Never fails: a state directory that cannot be created or locked is a
/// reason to supervise (this is the only window we can see), not a reason
/// to refuse to open a folder. The lock is a coordination aid between
/// cooperating windows of one IDE, not a security boundary, so degrading to
/// "assume we are alone" is the right failure — the alternative is an IDE
/// that will not start on a read-only home.
pub fn claim(workspace_root: &Path) -> Supervision {
    claim_in(&supervision_dir(), workspace_root)
}

/// [`claim`] against an explicit base directory, so tests need not touch
/// `XDG_STATE_HOME` — which is process-global, and therefore shared with
/// every other test running at the same moment.
pub fn claim_in(base: &Path, workspace_root: &Path) -> Supervision {
    let path = lock_path(base, workspace_root);
    if std::fs::create_dir_all(base).is_err() {
        return Supervision::Granted(unlocked(path));
    }
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return Supervision::Granted(unlocked(path));
    };

    match file.try_lock() {
        Ok(()) => {
            // Ours. Record who we are for the next window's notice; a
            // failure here costs a nicer message and nothing else.
            let _ = file.set_len(0);
            let _ = file.rewind();
            let _ = write!(file, "{}", std::process::id());
            let _ = file.flush();
            Supervision::Granted(SupervisionLock { _file: file, path })
        }
        Err(std::fs::TryLockError::WouldBlock) => Supervision::HeldElsewhere {
            pid: read_pid(&path),
        },
        // A filesystem with no working locks (some network mounts) reports
        // an error rather than contention. Assume we are alone rather than
        // refuse to supervise a folder nobody else may have open.
        Err(std::fs::TryLockError::Error(_)) => {
            Supervision::Granted(SupervisionLock { _file: file, path })
        }
    }
}

/// The degraded grant: supervision with no lock behind it, because the
/// state directory would not cooperate.
fn unlocked(path: PathBuf) -> SupervisionLock {
    // An anonymous handle so `Drop` has something to close. `/dev/null` is
    // openable wherever the IDE runs at all.
    let file = File::open("/dev/null").unwrap_or_else(|_| {
        // Truly nothing works; fall back to the lock path itself, which at
        // worst fails to open and takes the whole claim down — and if we
        // are here, the process has larger problems.
        File::open(&path).expect("no openable file descriptor for the supervision lock")
    });
    SupervisionLock { _file: file, path }
}

fn read_pid(path: &Path) -> Option<u32> {
    let mut raw = String::new();
    File::open(path).ok()?.read_to_string(&mut raw).ok()?;
    raw.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole contract, in the case it exists for: one folder, two
    /// windows, one supervisor.
    #[test]
    fn the_second_window_on_a_folder_does_not_supervise_it() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();

        let first = claim_in(base.path(), root.path());
        assert!(first.is_granted());
        assert_eq!(first.notice(), None, "the supervisor is told nothing");

        let second = claim_in(base.path(), root.path());
        assert!(!second.is_granted());
        let notice = second.notice().expect("the second window is told once");
        assert!(notice.contains("another taste-ide window"), "{notice}");
        // It names what still works. A person who opened the same project
        // twice on purpose has done nothing wrong.
        assert!(notice.contains("edits"), "{notice}");
        assert!(
            notice.contains(&format!("process {}", std::process::id())),
            "the holder is named: {notice}"
        );
    }

    /// Releasing hands the folder on. A window that closes must not leave
    /// its project unsupervisable.
    #[test]
    fn releasing_hands_supervision_to_the_next_window() {
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();

        let first = claim_in(base.path(), root.path());
        assert!(first.is_granted());
        assert!(!claim_in(base.path(), root.path()).is_granted());

        drop(first);
        let next = claim_in(base.path(), root.path());
        assert!(next.is_granted(), "a closed window releases its folder");
        assert!(next.notice().is_none());
    }

    /// The ordinary case, and the one that must never be slowed down by any
    /// of this: different folders never contend.
    #[test]
    fn different_folders_never_contend() {
        let base = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();

        let claims = [
            claim_in(base.path(), a.path()),
            claim_in(base.path(), b.path()),
            claim_in(base.path(), c.path()),
        ];
        assert!(
            claims.iter().all(Supervision::is_granted),
            "three windows on three folders all supervise"
        );
        // Three folders, three lock files — the key is doing the separating.
        let files: Vec<_> = std::fs::read_dir(base.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(files.len(), 3, "{files:?}");
    }

    /// The same folder reached through a symlink is the same folder, which
    /// is the whole reason the key canonicalizes. A window that dodged the
    /// lock by opening `/path/to/link` would be a second supervisor with
    /// every collision this module exists to prevent.
    #[test]
    fn a_symlink_to_an_open_folder_does_not_dodge_the_lock() {
        let base = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("project");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("project-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let first = claim_in(base.path(), &real);
        assert!(first.is_granted());
        assert!(
            !claim_in(base.path(), &link).is_granted(),
            "the same checkout by another name is the same checkout"
        );
    }

    /// A folder nobody has opened before is supervisable on the first try,
    /// including when the state directory does not exist yet.
    #[test]
    fn a_fresh_state_directory_is_created_on_demand() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("never").join("existed");
        let root = tempfile::tempdir().unwrap();
        assert!(claim_in(&nested, root.path()).is_granted());
        assert!(nested.is_dir());
    }

    /// A home that refuses writes is a reason to assume we are alone, not a
    /// reason to refuse to open a folder. The lock coordinates cooperating
    /// windows; it is not a boundary, and nothing security-relevant rests
    /// on it.
    #[test]
    fn an_unusable_state_directory_still_opens_the_folder() {
        let root = tempfile::tempdir().unwrap();
        let claim = claim_in(Path::new("/proc/nonexistent/taste"), root.path());
        assert!(claim.is_granted());
        assert!(claim.notice().is_none());
    }
}
