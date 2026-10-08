//! Whether an agent's command changed the environment's devcontainer
//! config.
//!
//! An agent that edits the config with `ide_edit_file` or `ide_write_file`
//! is told, in that call's answer, that a rebuild applies it. One that
//! edits it from a shell — `sed -i` through `ide_exec`, a heredoc in its
//! terminal — was told nothing, and finished its turn with the change
//! unapplied. So both shells take a [`Fingerprint`] of the config when a
//! command starts and another when it ends, and a difference is reported
//! with the command's own result (David, 2026-10-08: "plugging the blind
//! spot by letting the agent know when a write completes to one of those
//! files").
//!
//! **What this cannot tell is who wrote.** The user may save the same file
//! in their editor while the agent's build runs, and that change lands in
//! the same window. The report therefore names the files that changed and
//! leaves the attribution to the agent, which knows what its command does:
//! a `cargo test` that "changed .devcontainer/Containerfile" did not. The
//! nudge is never sent for a change made between commands, which is where
//! the user's own editing happens; an agent is not told to apply that.
//!
//! Sizes and modification times, not contents: the config is a handful of
//! small files, but this runs twice for every command an agent runs, and
//! on a remote checkout each read is a round trip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::files::{Files, Kind};

/// Past this many entries the walk stops: a `.devcontainer/` that large is
/// not a config, and a command must not wait on reading it.
const MAX_ENTRIES: usize = 256;

/// The config as it stood at one moment: every file under `.devcontainer/`
/// and a root `.devcontainer.json`, by path relative to the checkout, with
/// its size and modification time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint(BTreeMap<PathBuf, (u64, u64)>);

impl Fingerprint {
    /// The config under `root`, or `None` when the files cannot be reached
    /// — a files service not connected answers "absent" for everything,
    /// which would read as the whole config having been deleted.
    pub fn take(files: &Files, root: &Path) -> Option<Self> {
        if !files.is_connected() {
            return None;
        }
        let mut seen = BTreeMap::new();
        let root_file = PathBuf::from(".devcontainer.json");
        if let Ok(stat) = files.stat(&root.join(&root_file)) {
            seen.insert(root_file, (stat.size, stat.mtime_ms));
        }
        let mut dirs = vec![PathBuf::from(".devcontainer")];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = files.list(&root.join(&dir)) else {
                continue;
            };
            for entry in entries {
                if seen.len() >= MAX_ENTRIES {
                    return Some(Self(seen));
                }
                let relative = dir.join(&entry.name);
                match entry.kind {
                    Kind::Dir => dirs.push(relative),
                    _ => {
                        if let Ok(stat) = files.stat(&root.join(&relative)) {
                            seen.insert(relative, (stat.size, stat.mtime_ms));
                        }
                    }
                }
            }
        }
        Some(Self(seen))
    }

    /// The files that differ between `self` (before) and `after`: added,
    /// removed, or rewritten, sorted by path.
    pub fn changed(&self, after: &Fingerprint) -> Vec<String> {
        let mut paths: Vec<&PathBuf> = self.0.keys().chain(after.0.keys()).collect();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter(|path| self.0.get(*path) != after.0.get(*path))
            .map(|path| path.display().to_string())
            .collect()
    }
}

/// What a command's result says when the config changed while it ran: the
/// files, and what to do if the command was what changed them.
pub fn report(changed: &[String]) -> String {
    format!(
        "The devcontainer config changed while this command ran: {}. If this command \
         made that change and the config is complete, call devcontainer_reload to rebuild \
         with it. If it did not, the change is the user's: leave it to them.",
        changed.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_writes_the_config_is_seen_and_one_that_does_not_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".devcontainer/units")).unwrap();
        std::fs::write(root.join(".devcontainer/devcontainer.json"), "{}").unwrap();
        std::fs::write(root.join(".devcontainer/units/db.service"), "[Unit]").unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();
        let files = Files::Local;
        let before = Fingerprint::take(&files, root).unwrap();

        std::fs::write(root.join("README.md"), "hello").unwrap();
        let after = Fingerprint::take(&files, root).unwrap();
        assert!(before.changed(&after).is_empty());

        std::fs::write(root.join(".devcontainer/units/db.service"), "[Unit]\n").unwrap();
        std::fs::write(root.join(".devcontainer/Containerfile"), "FROM x").unwrap();
        std::fs::write(root.join(".devcontainer.json"), "{}").unwrap();
        let after = Fingerprint::take(&files, root).unwrap();
        assert_eq!(
            before.changed(&after),
            vec![
                ".devcontainer/Containerfile",
                ".devcontainer/units/db.service",
                ".devcontainer.json",
            ]
        );

        std::fs::remove_file(root.join(".devcontainer/devcontainer.json")).unwrap();
        let gone = Fingerprint::take(&files, root).unwrap();
        assert!(after
            .changed(&gone)
            .contains(&".devcontainer/devcontainer.json".to_string()));
    }

    #[test]
    fn a_checkout_with_no_config_has_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let print = Fingerprint::take(&Files::Local, dir.path()).unwrap();
        assert_eq!(print, Fingerprint::default());
    }
}
