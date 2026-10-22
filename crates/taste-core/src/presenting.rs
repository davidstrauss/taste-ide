//! Presentation mode: the user is showing their screen, until a time or
//! until they end it.
//!
//! While it lasts, the machine does not sleep or blank (the app holds the
//! desktop's inhibitor), and the IDE does nothing by its own choice that a
//! person presenting would feel — a VM swapped for a newer release, an
//! image rebuilt for its packages, a gigabyte fetched ahead of either, an
//! index built at full tilt (David, 2026-10-22: "It should prevent the
//! system from sleeping and also prevent things like unnecessary VM swaps
//! and container rebuilds"). What an agent asks for is not refused
//! outright: it is told the user is presenting, asks them, and asks again
//! saying they agreed ("Don't outright prevent an agent from rebuilding,
//! but maybe confirm somehow"). The user is asked in the agent's chat,
//! never in a dialog over what they are presenting.
//!
//! **Machine-wide.** Every project the user has open is a process of its
//! own with VMs of its own, and any of them swapping a VM is felt the same
//! way, so the mode lives in one file in the runtime directory
//! ([`file`]) that every process reads: started in one window, it is on in
//! all of them. Each process keeps its own copy of the answer
//! ([`presenting`]), read again by [`refresh`] — which does IO, so never on
//! the GTK thread — on the clock's tick, before a tool decides, and when
//! the app's monitor of the file says it changed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// The mode as this process last read it: 0 when off, [`BY_HAND`] when on
/// until ended, otherwise when it ends in seconds since the epoch.
static UNTIL: AtomicU64 = AtomicU64::new(0);

/// "On until the user ends it", in [`UNTIL`].
const BY_HAND: u64 = u64::MAX;

/// When presentation mode ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// At this time, in seconds since the epoch.
    At(u64),
    /// When the user ends it.
    Ended,
}

impl Until {
    /// "until 15:30" or "until it is ended", in this machine's time zone.
    pub fn words(self) -> String {
        match self {
            Until::At(at) => format!("until {}", clock(at)),
            Until::Ended => "until it is ended".to_string(),
        }
    }
}

/// The file that says presentation mode is on: `$XDG_RUNTIME_DIR/taste-ide/
/// presenting`. `None` without a runtime directory, which a desktop session
/// always has.
pub fn file() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|run| PathBuf::from(run).join("taste-ide").join("presenting"))
}

/// Whether presentation mode is on, as this process last read it: a pure
/// read, for any thread.
pub fn presenting() -> bool {
    until().is_some()
}

/// When presentation mode ends, if it is on, as this process last read it.
pub fn until() -> Option<Until> {
    match UNTIL.load(Ordering::Relaxed) {
        0 => None,
        BY_HAND => Some(Until::Ended),
        at if at > now() => Some(Until::At(at)),
        _ => None,
    }
}

/// Read the file again. True when the answer changed. IO: not on the GTK
/// thread.
pub fn refresh() -> bool {
    refresh_from(file())
}

fn refresh_from(path: Option<PathBuf>) -> bool {
    let read = path
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| parse(&text))
        .map_or(0, encode);
    UNTIL.swap(read, Ordering::Relaxed) != read
}

/// Turn presentation mode on, for every IDE on this machine, until
/// `until`. IO: not on the GTK thread.
pub fn start(until: Until) -> Result<()> {
    start_in(file(), until)
}

fn start_in(path: Option<PathBuf>, until: Until) -> Result<()> {
    let path = path.context("no runtime directory to keep presentation mode in")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = match until {
        Until::At(at) => at.to_string(),
        Until::Ended => "ended".to_string(),
    };
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    UNTIL.store(encode(until), Ordering::Relaxed);
    Ok(())
}

/// Turn presentation mode off, for every IDE on this machine. IO: not on
/// the GTK thread.
pub fn end() -> Result<()> {
    end_in(file())
}

fn end_in(path: Option<PathBuf>) -> Result<()> {
    UNTIL.store(0, Ordering::Relaxed);
    let Some(path) = path else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// What a tool an agent called says when it would do something presentation
/// mode holds, unless the agent says the user agreed: `what` is the act,
/// "rebuild this environment". `None` when the mode is off. Reads the file
/// first. IO: not on the GTK thread.
pub fn refusal(what: &str, flag: &str) -> Option<String> {
    refresh();
    Some(refusal_words(until()?, what, flag))
}

fn refusal_words(until: Until, what: &str, flag: &str) -> String {
    format!(
        "Not now: the user is presenting their screen ({}), and to {what} would \
         interrupt it. Nothing was done. Ask the user whether to {what} anyway; if they \
         say yes, call again with `{flag}: true`. Otherwise wait until the presentation \
         ends.",
        until.words()
    )
}

fn parse(text: &str) -> Option<Until> {
    let text = text.trim();
    if text == "ended" {
        return Some(Until::Ended);
    }
    let at: u64 = text.parse().ok()?;
    (at > now()).then_some(Until::At(at))
}

fn encode(until: Until) -> u64 {
    match until {
        Until::At(at) => at.min(BY_HAND - 1),
        Until::Ended => BY_HAND,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `at` as a clock reads it here, "15:30".
fn clock(at: u64) -> String {
    let time = at as libc::time_t;
    // SAFETY: `localtime_r` writes only into `tm`, which outlives the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return "later".to_string();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_says_until_when() {
        assert_eq!(parse("ended\n"), Some(Until::Ended));
        let later = now() + 600;
        assert_eq!(parse(&later.to_string()), Some(Until::At(later)));
        // A time already past is no presentation, whoever forgot the file.
        assert_eq!(parse(&(now() - 1).to_string()), None);
        assert_eq!(parse("soon"), None);
    }

    #[test]
    fn the_mode_is_shared_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(dir.path().join("taste-ide").join("presenting"));
        end_in(path.clone()).unwrap();
        assert!(!presenting());
        let at = now() + 3600;
        start_in(path.clone(), Until::At(at)).unwrap();
        // Another process reads it from the file.
        UNTIL.store(0, Ordering::Relaxed);
        assert!(refresh_from(path.clone()));
        assert_eq!(until(), Some(Until::At(at)));
        start_in(path.clone(), Until::Ended).unwrap();
        assert_eq!(until(), Some(Until::Ended));
        end_in(path.clone()).unwrap();
        assert!(!refresh_from(path));
        assert!(!presenting());
    }

    #[test]
    fn an_agent_is_told_to_ask_and_how_to_say_yes() {
        let said = refusal_words(Until::Ended, "rebuild this environment", "while_presenting");
        assert!(said.contains("rebuild this environment"), "{said}");
        assert!(said.contains("`while_presenting: true`"), "{said}");
        assert!(said.contains("until it is ended"), "{said}");
    }
}
