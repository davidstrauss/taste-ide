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
/// ([`WorkspaceState::environments`] and [`ChatEntry::environment`]).
pub const STATE_VERSION: u32 = 3;

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

    #[test]
    fn corrupt_state_degrades_to_default() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/w");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(file_for(base.path(), root), "{not json").unwrap();
        assert_eq!(load_from(base.path(), root), WorkspaceState::default());
    }
}
