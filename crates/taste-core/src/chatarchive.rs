//! Conversations stashed on the user's own machine, and nowhere else.
//!
//! A chat is the one artifact this IDE keeps that is **deliberately not in
//! the restorable archive**. Everything else an environment produces
//! becomes refs — the working copy, the backlog, review verdicts — because
//! refs travel, which is what makes a workspace restorable on another
//! machine or another provisioner (docs/ENVIRONMENTS.md → "Isolation").
//! Conversations do not get that treatment, for two reasons the user gave
//! together (David, 2026-09-17):
//!
//! > I'm concerned about conversations getting pushed to GitHub, and I care
//! > less about them than other artifacts. I want the IDE to back them up
//! > to state the IDE keeps on my machine — but not store in the restorable
//! > archive itself.
//!
//! Both halves matter. A transcript is the likeliest place in the whole
//! system for a pasted secret to be sitting, and a ref is a thing that gets
//! mirrored and can be pushed; keeping conversations out of refs means they
//! cannot leave the machine by accident. And they are worth less than the
//! code, so paying for their durability with that risk would be a bad
//! trade.
//!
//! So the archive is host-side state, under the workspace's own state
//! directory ([`crate::state::workspace_state_dir`]) — which the backup
//! excludes wholesale, because that is also where credentials live.
//!
//! **What restore does:** look for a stash for the environment being
//! restored; replay it if it is there, and start the chat fresh if it is
//! not. There is no error case. A conversation is a nicety that survives
//! when the machine that heard it is still around, and a workspace restored
//! onto a new laptop starts talking again rather than failing to.
//!
//! **What is stored** is the ACP `SessionUpdate` stream verbatim, one JSON
//! object per line, as it arrived. That is the documented interface — the
//! wire type, `Serialize` and all — rather than the adapter's own history
//! directory, whose layout is private, undocumented, and different for
//! every agent. This module does not interpret the payload: it owns an
//! envelope with a timestamp in it, and the caller owns what the envelope
//! carries.
//!
//! **Retention is seven days** ([`RETENTION`]), swept by mtime. "Delete old
//! chat archives after 7 days" — so an archive restored after a fortnight
//! finds nothing and starts fresh, which is the intended outcome and not a
//! failure.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentId;

/// How long a stashed conversation is kept after its last update.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The most one environment's stash may hold. What is stashed is what the
/// agent streamed, and an agent decides how much that is; a week of it,
/// unbounded, is this host's disk. Past this the oldest half goes: the
/// replay is of a conversation's end, which is the part worth having.
pub const MAX_STASH_BYTES: u64 = 32 * 1024 * 1024;

/// One update as stashed: an envelope this module owns, around a payload it
/// does not read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchivedUpdate {
    /// Unix seconds. Not for display — this is for ordering a file two
    /// writers appended to, and for saying how old a conversation is.
    pub at: u64,
    /// The ACP `SessionUpdate` exactly as it arrived.
    pub update: serde_json::Value,
}

/// One workspace's stashed conversations, one file per environment.
pub struct ChatArchive {
    dir: PathBuf,
}

impl ChatArchive {
    /// The archive for a workspace, beside its other host-side state.
    pub fn for_workspace(root: &Path) -> Self {
        Self::at(crate::state::workspace_state_dir(root).join("chats"))
    }

    /// An archive at a directory of the caller's choosing. For tests, and
    /// for anything that already knows where state lives.
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// TASTE_PROBE_CHECK only: an archive under the temporary directory, so
    /// a screenshot run can write a conversation and read it back without
    /// leaving one in the user's state. The same reason `semantic` keeps its
    /// index out of a probe run.
    pub fn for_probe() -> Self {
        Self::at(std::env::temp_dir().join("taste-probe-chats"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where one environment's conversation is stashed.
    ///
    /// The environment's id is its own file name: `EnvironmentId` is already
    /// constrained to what a container name may contain, so it needs no
    /// escaping here.
    pub fn path_for(&self, env: &EnvironmentId) -> PathBuf {
        self.dir.join(format!("{env}.jsonl"))
    }

    /// Whether this environment has a conversation stashed — the question
    /// restore asks before deciding whether to replay or start fresh.
    pub fn has(&self, env: &EnvironmentId) -> bool {
        self.path_for(env).is_file()
    }

    /// Append one update.
    ///
    /// One line, one `write`, in append mode: two windows writing the same
    /// environment's chat is not a case this design has, and a single
    /// append-mode write of a short line does not interleave in practice
    /// even if it arose. The cost of being wrong is one unparseable line,
    /// which [`ChatArchive::load`] skips.
    pub fn append(&self, env: &EnvironmentId, update: serde_json::Value) -> Result<()> {
        self.append_all(env, vec![update])
    }

    /// Append a whole turn's worth of updates in one write.
    ///
    /// The form callers should reach for. A streaming answer is hundreds of
    /// chunks, and the GTK thread may not do filesystem IO at all, so the
    /// chat pane buffers a turn and writes it here once — which also makes
    /// the write atomic per turn rather than per chunk, so two flushes that
    /// overlap can only reorder turns and never interleave lines.
    pub fn append_all(&self, env: &EnvironmentId, updates: Vec<serde_json::Value>) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let at = now_secs();
        let mut text = String::new();
        for update in updates {
            let record = ArchivedUpdate { at, update };
            text.push_str(&serde_json::to_string(&record).context("serialising a chat update")?);
            text.push('\n');
        }
        let path = self.path_for(env);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("appending to {}", path.display()))?;
        drop(file);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_STASH_BYTES {
            self.keep_newest(&path, MAX_STASH_BYTES / 2)?;
        }
        Ok(())
    }

    /// Cut a stash down to its newest `keep` bytes, whole lines only,
    /// written beside and renamed over.
    fn keep_newest(&self, path: &Path, keep: u64) -> Result<()> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cut = text.len().saturating_sub(keep as usize);
        // By byte: `cut` may fall inside a character, and the newline after
        // it never does.
        let start = text.as_bytes()[cut..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |i| cut + i + 1);
        let staged = path.with_extension("jsonl.trim");
        std::fs::write(&staged, &text[start..])
            .with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Everything stashed for this environment, oldest first.
    ///
    /// Never an error, and deliberately tolerant: an IDE killed mid-append
    /// leaves a half-written last line, and one bad line must not cost the
    /// conversation above it. Unreadable lines are skipped and counted so
    /// the caller can say so if it wants to.
    pub fn load(&self, env: &EnvironmentId) -> Loaded {
        let Ok(text) = std::fs::read_to_string(self.path_for(env)) else {
            return Loaded::default();
        };
        let mut loaded = Loaded::default();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ArchivedUpdate>(line) {
                Ok(record) => loaded.updates.push(record),
                Err(_) => loaded.skipped += 1,
            }
        }
        loaded
    }

    /// Forget one environment's conversation — its environment is gone, or
    /// the user asked.
    pub fn forget(&self, env: &EnvironmentId) -> Result<()> {
        let path = self.path_for(env);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already absent is the outcome asked for.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Delete conversations untouched for longer than [`RETENTION`], and
    /// report how many went.
    ///
    /// By mtime, which is the last append: a file's last write and its last
    /// record's timestamp are the same moment, and mtime does not require
    /// reading a megabyte of transcript to learn it.
    pub fn sweep(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            // A file from the future keeps: a clock that moved backwards is
            // not a reason to delete somebody's conversation.
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age > RETENTION && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// What [`ChatArchive::load`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    pub updates: Vec<ArchivedUpdate>,
    /// Lines that could not be parsed — a crash mid-append leaves one.
    pub skipped: usize,
}

impl Loaded {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stash_past_its_cap_keeps_its_newest_half() {
        let (_dir, archive) = archive();
        let env = EnvironmentId::parse("review").unwrap();
        let chunk = "é".repeat(64 * 1024);
        let mut n = 0;
        while archive.path_for(&env).metadata().map_or(0, |m| m.len())
            <= MAX_STASH_BYTES - 256 * 1024
        {
            archive
                .append(&env, json!({ "n": n, "text": chunk }))
                .unwrap();
            n += 1;
        }
        archive
            .append(&env, json!({ "n": n, "text": chunk }))
            .unwrap();
        archive
            .append(&env, json!({ "n": n + 1, "text": chunk }))
            .unwrap();
        let size = archive.path_for(&env).metadata().unwrap().len();
        assert!(size <= MAX_STASH_BYTES, "{size}");
        let loaded = archive.load(&env);
        assert_eq!(loaded.skipped, 0, "only whole lines are kept");
        assert_eq!(loaded.updates.last().unwrap().update["n"], n + 1);
        assert!(
            loaded.updates[0].update["n"].as_u64().unwrap() > 0,
            "the oldest went"
        );
    }

    fn archive() -> (tempfile::TempDir, ChatArchive) {
        let dir = tempfile::tempdir().unwrap();
        let archive = ChatArchive::at(dir.path().join("chats"));
        (dir, archive)
    }

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn age(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn updates_round_trip_in_order() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        assert!(!archive.has(&one));
        assert!(archive.load(&one).is_empty());

        archive
            .append(&one, json!({"sessionUpdate": "user_message_chunk"}))
            .unwrap();
        archive
            .append(&one, json!({"sessionUpdate": "agent_message_chunk"}))
            .unwrap();

        assert!(archive.has(&one));
        let loaded = archive.load(&one);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(loaded.updates.len(), 2);
        assert_eq!(
            loaded.updates[0].update["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            loaded.updates[1].update["sessionUpdate"],
            "agent_message_chunk"
        );
        assert!(loaded.updates[0].at > 0);
    }

    /// The batched form is one write, and keeps its order.
    #[test]
    fn a_turn_is_appended_in_one_write_and_in_order() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        archive
            .append_all(
                &one,
                vec![json!({"n": 1}), json!({"n": 2}), json!({"n": 3})],
            )
            .unwrap();
        archive.append_all(&one, vec![]).unwrap();
        let loaded = archive.load(&one);
        assert_eq!(loaded.updates.len(), 3, "an empty flush writes nothing");
        let order: Vec<i64> = loaded
            .updates
            .iter()
            .map(|u| u.update["n"].as_i64().unwrap())
            .collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    /// Environments do not share a stash.
    #[test]
    fn each_environment_has_its_own() {
        let (_dir, archive) = archive();
        archive.append(&env("i-0001"), json!({"n": 1})).unwrap();
        archive.append(&env("i-0002"), json!({"n": 2})).unwrap();
        assert_eq!(archive.load(&env("i-0001")).updates[0].update["n"], 1);
        assert_eq!(archive.load(&env("i-0002")).updates[0].update["n"], 2);
    }

    /// An IDE killed mid-append leaves a half-written line. It must cost
    /// that line and nothing above it.
    #[test]
    fn a_torn_last_line_does_not_cost_the_conversation() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        archive.append(&one, json!({"n": 1})).unwrap();
        archive.append(&one, json!({"n": 2})).unwrap();
        // ...and then the process died half a line in.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(archive.path_for(&one))
            .unwrap();
        file.write_all(b"{\"at\":1,\"upda").unwrap();

        let loaded = archive.load(&one);
        assert_eq!(loaded.updates.len(), 2, "the good lines survived");
        assert_eq!(loaded.skipped, 1, "and the torn one is counted, not hidden");
    }

    /// Seven days, by mtime, and a fresh conversation is not collateral.
    #[test]
    fn conversations_older_than_the_retention_are_swept() {
        let (_dir, archive) = archive();
        let old = env("i-0001");
        let recent = env("i-0002");
        archive.append(&old, json!({"n": 1})).unwrap();
        archive.append(&recent, json!({"n": 2})).unwrap();

        age(&archive.path_for(&old), RETENTION + Duration::from_secs(60));

        assert_eq!(archive.sweep(), 1);
        assert!(!archive.has(&old), "the old conversation went");
        assert!(archive.has(&recent), "the recent one stayed");
        // And a restore now finds nothing for it, which is "start fresh"
        // rather than an error.
        assert!(archive.load(&old).is_empty());
    }

    /// A conversation right at the edge is kept: retention is a floor on
    /// how long it lives, not a race.
    #[test]
    fn a_conversation_inside_the_retention_is_kept() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        archive.append(&one, json!({"n": 1})).unwrap();
        age(&archive.path_for(&one), RETENTION - Duration::from_secs(60));
        assert_eq!(archive.sweep(), 0);
        assert!(archive.has(&one));
    }

    /// A clock that jumped backwards must not delete anything.
    #[test]
    fn a_file_dated_in_the_future_is_left_alone() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        archive.append(&one, json!({"n": 1})).unwrap();
        let when = SystemTime::now() + Duration::from_secs(3600);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(archive.path_for(&one))
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
        assert_eq!(archive.sweep(), 0);
        assert!(archive.has(&one));
    }

    #[test]
    fn forgetting_is_idempotent_and_sweeping_an_absent_archive_is_not_an_error() {
        let (_dir, archive) = archive();
        let one = env("i-0001");
        assert_eq!(archive.sweep(), 0, "no directory yet");
        archive.forget(&one).unwrap();
        archive.append(&one, json!({"n": 1})).unwrap();
        archive.forget(&one).unwrap();
        archive.forget(&one).unwrap();
        assert!(!archive.has(&one));
    }

    /// The stash lives under the workspace's own state directory, which is
    /// what keeps it out of the restorable archive: the backup excludes
    /// that directory wholesale, because credentials are in it too.
    #[test]
    fn the_stash_is_under_the_workspaces_own_state_directory() {
        let root = Path::new("/home/dev/project");
        let archive = ChatArchive::for_workspace(root);
        let state = crate::state::workspace_state_dir(root);
        assert!(
            archive.dir().starts_with(&state),
            "{} is not under {}",
            archive.dir().display(),
            state.display()
        );
        assert!(archive.dir().ends_with("chats"));
    }
}
