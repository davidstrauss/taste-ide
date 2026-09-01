//! Per-workspace restore state, the industry-normal way.
//!
//! There is deliberately no "project" concept beyond this: recently opened
//! folders live in the desktop's recent-files list (GtkRecentManager), and
//! what was last open lives here — a JSON file per workspace under
//! `$XDG_STATE_HOME/taste-ide/workspaces/` (the XDG base-dir spec's home
//! for exactly this kind of data: open files, layout, session handles).
//! The chat sessions themselves are restored through ACP's own
//! `session/load`; only their ids are persisted here.
//!
//! The file carries a schema [`STATE_VERSION`]. While the IDE is alpha
//! there is deliberately NO migration path: a file written by an older
//! schema is discarded and the workspace starts fresh (the caller is
//! told, so the user hears about it once — see [`load_reporting`]).
//! Restore state is a convenience, never data of record; the cost of
//! carrying compatibility shims for every alpha iteration is not worth
//! paying, and a half-migrated file is worse than a clean one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentId;

/// Bump on any incompatible change to the shape below. v2 replaced the
/// single-chat fields (`agent_id`/`session_id`/`model_value`) with
/// [`WorkspaceState::open_chats`]; v3 added the environment dimension
/// ([`WorkspaceState::environments`] and [`ChatEntry::environment`]); v4
/// added [`ChatEntry::role`], which is what makes one chat the
/// orchestrator across restarts.
pub const STATE_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Schema version; a file without it, or with another value, is stale
    /// and gets discarded rather than migrated.
    pub version: u32,
    /// The workspace root (for human inspection of the state file).
    #[serde(default)]
    pub root: PathBuf,
    #[serde(default)]
    pub open_files: Vec<PathBuf>,
    #[serde(default)]
    pub active_file: Option<PathBuf>,
    /// The chat tabs that were open, left to right. Empty means "no chat
    /// worth restoring" — the window opens one fresh chat.
    #[serde(default)]
    pub open_chats: Vec<ChatEntry>,
    /// Index into `open_chats` of the tab that was selected. Out of range
    /// simply means the first tab.
    #[serde(default)]
    pub active_chat: usize,
    /// Environments this workspace knows about, beyond the primary (which
    /// exists by construction and is never listed).
    ///
    /// Deliberately thin: the clone directory under
    /// `$XDG_STATE_HOME/taste-ide/environments/` is the real inventory, and
    /// a state file that disagreed with the disk would be a second source of
    /// truth. What lives here is only what the disk cannot say — the human
    /// name the user gave it.
    #[serde(default)]
    pub environments: Vec<EnvironmentEntry>,
}

/// Persisted metadata for one non-primary environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentEntry {
    pub id: EnvironmentId,
    /// What the user calls it. Absent means "call it by its slug".
    #[serde(default)]
    pub display_name: Option<String>,
    /// RFC 3339 creation timestamp, for the fleet view's ordering.
    #[serde(default)]
    pub created_at: Option<String>,
}

impl WorkspaceState {
    /// What the user calls this environment, if they have named it.
    ///
    /// `None` means "call it by its slug" — deliberately not filled in with
    /// the slug here, so the caller can tell a name from a default and the
    /// rename affordance starts empty rather than pre-typed.
    pub fn environment_name(&self, id: &EnvironmentId) -> Option<&str> {
        self.environments
            .iter()
            .find(|entry| &entry.id == id)
            .and_then(|entry| entry.display_name.as_deref())
    }

    /// Name (or un-name) an environment, creating its entry if this is the
    /// first thing the state file has ever had to say about it.
    ///
    /// A blank name is `None`, not an empty string: an entry claiming the
    /// user named it "" would render as a nameless row that nonetheless
    /// refuses to fall back to the slug.
    pub fn set_environment_name(&mut self, id: &EnvironmentId, name: Option<&str>) {
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        match self.environments.iter_mut().find(|entry| &entry.id == id) {
            Some(entry) => entry.display_name = name.map(str::to_string),
            None => self.environments.push(EnvironmentEntry {
                id: id.clone(),
                display_name: name.map(str::to_string),
                created_at: None,
            }),
        }
    }

    /// Record when an environment was created, if it is not already known.
    /// The clone directory is the inventory of record; this is only what
    /// the disk cannot say in a stable order.
    pub fn note_environment_created(&mut self, id: &EnvironmentId, when: String) {
        if let Some(entry) = self.environments.iter_mut().find(|entry| &entry.id == id) {
            entry.created_at.get_or_insert(when);
            return;
        }
        self.environments.push(EnvironmentEntry {
            id: id.clone(),
            display_name: None,
            created_at: Some(when),
        });
    }

    /// Forget an environment that no longer exists. Called when one is
    /// destroyed: a name for a clone that is gone is a second inventory
    /// disagreeing with the disk.
    pub fn forget_environment(&mut self, id: &EnvironmentId) {
        self.environments.retain(|entry| &entry.id != id);
    }
}

/// One chat tab: which agent it talks to, which conversation it holds,
/// and the model chosen for it. Session and model settings travel with
/// the tab, so this is the whole of a chat's restorable identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatEntry {
    /// The agent registry id (`taste_acp::builtin_agents`).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// The ACP session id, restorable via `session/load`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The user's explicit model choice for this chat (config option value
    /// id). Absent = follow the agent's default.
    #[serde(default)]
    pub model_value: Option<String>,
    /// The permission mode this chat runs in (an ACP session mode id).
    /// Absent = the IDE's default, which the chat pane re-applies to every
    /// session it connects.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// The client-side auto-approve switch: answer the agent's permission
    /// requests without asking. Off unless the user turned it on.
    #[serde(default)]
    pub auto_approve: bool,
    /// The environment this chat's agent works in — its clone, its
    /// devcontainer, its exec target. Absent means the primary environment
    /// (the main checkout), which is what every chat gets until the
    /// environment-creation UI lands.
    #[serde(default)]
    pub environment: Option<EnvironmentId>,
    /// What this chat is *for*. Absent — the overwhelmingly common case —
    /// is an ordinary chat.
    #[serde(default)]
    pub role: Option<ChatRole>,
}

/// A chat's designated role. There is exactly one role and at most one
/// chat holding it, which is why this is an enum rather than a bag of
/// flags: a second role would be a second answer to "which socket serves
/// the orchestration tools", and there is only one socket to serve them
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatRole {
    /// The workspace's orchestrator: its environment's MCP socket serves
    /// the orchestration tools, and no other socket does.
    Orchestrator,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            root: PathBuf::new(),
            open_files: Vec::new(),
            active_file: None,
            open_chats: Vec::new(),
            active_chat: 0,
            environments: Vec::new(),
        }
    }
}

/// Now, as [`EnvironmentEntry::created_at`] wants it.
///
/// Hand-rolled rather than pulled in: this is the only date this codebase
/// formats, and the civil-from-days arithmetic is a dozen lines that need
/// no crate, no timezone database and no version to track. UTC, always —
/// the field is an ordering key and a fact about when, not a local clock
/// reading.
pub fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    rfc3339_from_unix(seconds)
}

fn rfc3339_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    // Howard Hinnant's civil_from_days, shifted to a March-based year so
    // the leap day lands at the end of it and the month arithmetic has no
    // special cases.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

fn state_base() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/state")
        })
        .join("taste-ide")
        .join("workspaces")
}

fn file_for(base: &Path, root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".into());
    base.join(format!("{name}-{short}.json"))
}

/// Load the workspace's state; any problem yields a clean default.
pub fn load(root: &Path) -> WorkspaceState {
    load_from(&state_base(), root)
}

pub fn load_from(base: &Path, root: &Path) -> WorkspaceState {
    load_reporting_from(base, root).0
}

/// Load, and say whether an existing file was DISCARDED as stale (wrong
/// schema version, or unreadable). The window surfaces that once, so a
/// reset never looks like silent data loss.
pub fn load_reporting(root: &Path) -> (WorkspaceState, bool) {
    load_reporting_from(&state_base(), root)
}

pub fn load_reporting_from(base: &Path, root: &Path) -> (WorkspaceState, bool) {
    let path = file_for(base, root);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (WorkspaceState::default(), false); // nothing there yet
    };
    match serde_json::from_str::<WorkspaceState>(&raw) {
        Ok(state) if state.version == STATE_VERSION => (state, false),
        Ok(state) => {
            tracing::warn!(
                "workspace state {} is schema v{} (this build writes v{STATE_VERSION}) \
                 — discarding it and starting fresh",
                path.display(),
                state.version
            );
            (WorkspaceState::default(), true)
        }
        Err(e) => {
            tracing::warn!(
                "workspace state {} is not readable ({e}) — discarding it and \
                 starting fresh",
                path.display()
            );
            (WorkspaceState::default(), true)
        }
    }
}

pub fn save(root: &Path, state: &WorkspaceState) -> Result<()> {
    save_to(&state_base(), root, state)
}

pub fn save_to(base: &Path, root: &Path, state: &WorkspaceState) -> Result<()> {
    std::fs::create_dir_all(base).context("creating state dir")?;
    let path = file_for(base, root);
    let json = serde_json::to_string_pretty(state).context("serializing state")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(agent: &str, session: &str) -> ChatEntry {
        ChatEntry {
            agent_id: Some(agent.into()),
            session_id: Some(session.into()),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrips_and_defaults_on_missing() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");

        // Nothing on disk is not a reset — there was nothing to lose.
        assert_eq!(
            load_reporting_from(base.path(), root),
            (WorkspaceState::default(), false)
        );

        let state = WorkspaceState {
            version: STATE_VERSION,
            root: root.to_path_buf(),
            open_files: vec![root.join("src/main.rs"), root.join("README.md")],
            active_file: Some(root.join("src/main.rs")),
            open_chats: vec![chat("claude-code", "sess-abc")],
            active_chat: 0,
            environments: Vec::new(),
        };
        save_to(base.path(), root, &state).unwrap();
        assert_eq!(load_from(base.path(), root), state);
    }

    #[test]
    fn many_chats_roundtrip_in_order() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");
        let state = WorkspaceState {
            version: STATE_VERSION,
            root: root.to_path_buf(),
            open_chats: vec![
                chat("claude-code", "sess-1"),
                ChatEntry {
                    agent_id: Some("claude-code".into()),
                    session_id: Some("sess-2".into()),
                    model_value: Some("opus[1m]".into()),
                    permission_mode: Some("auto".into()),
                    auto_approve: true,
                    environment: None,
                    role: None,
                },
                // A tab opened but never prompted: no session yet.
                ChatEntry {
                    agent_id: Some("gemini".into()),
                    ..Default::default()
                },
            ],
            active_chat: 1,
            ..Default::default()
        };
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(loaded.open_chats.len(), 3);
        assert_eq!(
            loaded.open_chats[1].model_value.as_deref(),
            Some("opus[1m]")
        );
        assert_eq!(
            loaded.open_chats[1].permission_mode.as_deref(),
            Some("auto")
        );
        assert!(loaded.open_chats[1].auto_approve);
        assert!(!loaded.open_chats[0].auto_approve);
        assert_eq!(loaded.active_chat, 1);
    }

    /// The orchestrator role survives a restart, and an ordinary chat
    /// stays ordinary. Which chat holds it is the workspace's answer to
    /// "whose MCP socket serves the orchestration tools", so a role that
    /// evaporated on relaunch would silently take execution authority away
    /// from a conversation still describing itself as the orchestrator.
    #[test]
    fn the_orchestrator_role_round_trips() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");
        let state = WorkspaceState {
            version: STATE_VERSION,
            root: root.to_path_buf(),
            open_chats: vec![
                ChatEntry {
                    agent_id: Some("claude-code".into()),
                    environment: Some(EnvironmentId::parse("hub").unwrap()),
                    role: Some(ChatRole::Orchestrator),
                    ..Default::default()
                },
                chat("claude-code", "sess-2"),
            ],
            ..Default::default()
        };
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(loaded.open_chats[0].role, Some(ChatRole::Orchestrator));
        assert_eq!(loaded.open_chats[1].role, None);
        // Spelled kebab-case on disk: it is read by humans debugging a
        // workspace, and by nothing else.
        let written = std::fs::read_to_string(super::file_for(base.path(), root)).unwrap();
        assert!(written.contains("\"role\": \"orchestrator\""), "{written}");
    }

    #[test]
    fn empty_state_saves_and_loads() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/empty");
        let state = WorkspaceState {
            root: root.to_path_buf(),
            ..Default::default()
        };
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert!(loaded.open_chats.is_empty());
        assert_eq!(loaded.version, STATE_VERSION);
    }

    /// Alpha policy: an older schema is discarded, not migrated — and the
    /// discard is REPORTED, so the user hears about it once.
    #[test]
    fn old_format_is_discarded_not_migrated() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/legacy");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            r#"{"root":"/work/legacy","open_files":["/work/legacy/a.rs"],
                "agent_id":"claude-code","session_id":"sess-old"}"#,
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(reset, "a v1 file must be reported as reset");
        assert_eq!(state, WorkspaceState::default());
        assert!(state.open_chats.is_empty());
        assert!(state.open_files.is_empty());
    }

    /// A v2 file (chats, no environment dimension) is discarded exactly the
    /// same way. No migration shim: the environment core changed what a
    /// chat *is*, and half-restoring one would bind it to nothing.
    #[test]
    fn pre_environment_state_is_discarded() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/v2");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            r#"{"version":2,"root":"/work/v2",
                "open_chats":[{"agent_id":"claude-code","session_id":"s"}]}"#,
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(reset);
        assert!(state.open_chats.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    /// A chat's environment binding and the environment inventory survive a
    /// roundtrip; an unbound chat means "primary".
    #[test]
    fn environment_binding_roundtrips() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/multi");
        let review = EnvironmentId::parse("review").unwrap();
        let state = WorkspaceState {
            root: root.to_path_buf(),
            open_chats: vec![
                chat("claude-code", "on-primary"),
                ChatEntry {
                    agent_id: Some("claude-code".into()),
                    session_id: Some("in-review".into()),
                    environment: Some(review.clone()),
                    ..Default::default()
                },
            ],
            environments: vec![EnvironmentEntry {
                id: review.clone(),
                display_name: Some("Review".into()),
                created_at: Some("2026-08-31T12:00:00Z".into()),
            }],
            ..Default::default()
        };
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(loaded.open_chats[0].environment, None);
        assert_eq!(loaded.open_chats[1].environment.as_ref(), Some(&review));
        assert_eq!(loaded.environments[0].id, review);
    }

    /// The slug lands in container and volume names, so a state file
    /// carrying an unusable one is rejected rather than trusted.
    #[test]
    fn an_invalid_environment_slug_makes_the_file_unreadable() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/bad-slug");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            format!(r#"{{"version":{STATE_VERSION},"environments":[{{"id":"Not A Slug"}}]}}"#),
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(reset);
        assert!(state.environments.is_empty());
    }

    #[test]
    fn future_version_is_discarded() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/future");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            format!(r#"{{"version":{}}}"#, STATE_VERSION + 7),
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(reset);
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn different_roots_get_different_files() {
        let base = tempfile::tempdir().unwrap();
        let a = Path::new("/work/alpha");
        let b = Path::new("/other/alpha"); // same name, different path
        save_to(
            base.path(),
            a,
            &WorkspaceState {
                open_chats: vec![chat("claude-code", "a")],
                ..Default::default()
            },
        )
        .unwrap();
        save_to(
            base.path(),
            b,
            &WorkspaceState {
                open_chats: vec![chat("claude-code", "b")],
                ..Default::default()
            },
        )
        .unwrap();
        let session = |state: WorkspaceState| state.open_chats[0].session_id.clone();
        assert_eq!(session(load_from(base.path(), a)).as_deref(), Some("a"));
        assert_eq!(session(load_from(base.path(), b)).as_deref(), Some("b"));
    }

    /// The human name is the one thing the clone directory cannot say, so
    /// it has to survive a restart — and un-naming has to survive it too,
    /// or a cleared name comes back on the next launch.
    #[test]
    fn environment_names_round_trip_and_clear() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");
        let calm = EnvironmentId::parse("calm-1").unwrap();
        let spry = EnvironmentId::parse("spry-2").unwrap();

        let mut state = WorkspaceState {
            root: root.to_path_buf(),
            ..Default::default()
        };
        // Unknown environments have no name and are not invented.
        assert_eq!(state.environment_name(&calm), None);
        state.note_environment_created(&calm, "2026-08-31T10:00:00Z".into());
        state.set_environment_name(&calm, Some("  the refactor  "));
        state.set_environment_name(&spry, Some("docs"));
        save_to(base.path(), root, &state).unwrap();

        let reloaded = load_from(base.path(), root);
        assert_eq!(reloaded.environment_name(&calm), Some("the refactor"));
        assert_eq!(reloaded.environment_name(&spry), Some("docs"));
        // Naming an environment the state had never heard of created its
        // entry rather than dropping the name on the floor.
        assert_eq!(reloaded.environments.len(), 2);
        assert_eq!(
            reloaded
                .environments
                .iter()
                .find(|e| e.id == calm)
                .and_then(|e| e.created_at.clone())
                .as_deref(),
            Some("2026-08-31T10:00:00Z")
        );

        let mut cleared = reloaded;
        cleared.set_environment_name(&calm, Some("   "));
        cleared.forget_environment(&spry);
        save_to(base.path(), root, &cleared).unwrap();
        let after = load_from(base.path(), root);
        assert_eq!(after.environment_name(&calm), None, "blank is not a name");
        assert_eq!(after.environment_name(&spry), None);
        assert_eq!(after.environments.len(), 1, "a destroyed env is forgotten");
    }

    /// The stamp is an ordering key that a person also reads, so it has to
    /// be a real date — including across the leap years and century rules
    /// that make hand-rolled date maths worth testing.
    #[test]
    fn creation_stamps_are_real_utc_dates() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1), "1970-01-01T00:00:01Z");
        // A leap day in a year divisible by 400.
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        // The day after 1900's non-leap February, four hundred years on.
        assert_eq!(rfc3339_from_unix(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_756_684_799), "2025-08-31T23:59:59Z");
        // Sorts lexicographically, which is the whole point of the format.
        assert!(rfc3339_from_unix(1) < rfc3339_from_unix(951_782_400));
        assert!(now_rfc3339().as_str() > "2020-01-01T00:00:00Z");
    }

    #[test]
    fn corrupt_state_degrades_to_default() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/w");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(file_for(base.path(), root), "{not json").unwrap();
        assert_eq!(load_from(base.path(), root), WorkspaceState::default());
    }
}
