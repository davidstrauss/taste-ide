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
/// single-chat fields (`agent_id`/`session_id`/`model_value`) with a list
/// of open chats; v3 added the environment dimension
/// ([`WorkspaceState::environments`] and [`ChatEntry::environment`]); v4
/// added [`ChatEntry::role`], which is what makes one chat the
/// orchestrator across restarts; v5 made a chat's environment REQUIRED and
/// unique — one chat per environment, which is what killed the chat tab
/// strip (see [`WorkspaceState::set_chat`]).
pub const STATE_VERSION: u32 = 5;

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
    /// The workspace's chats, at most one per environment.
    ///
    /// Private, and reachable only through [`WorkspaceState::set_chat`] and
    /// friends, because the uniqueness is the *model* and not a convention
    /// the UI is trusted to keep: a chat is an environment's conversation,
    /// so two of them in one environment is not a state this IDE has an
    /// answer for — which of the two does the pane show?
    ///
    /// There is deliberately no "active chat" beside it. Which chat is on
    /// screen follows the selected environment, and that selection is UI
    /// state the window never persists (a fresh IDE opens on the user's own
    /// checkout).
    #[serde(default)]
    chats: Vec<ChatEntry>,
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
    ///
    /// Its chat goes with it. A conversation is an environment's — there is
    /// nowhere else for it to live, and a chat entry naming a clone that
    /// has been deleted would restore as a pane with no world.
    pub fn forget_environment(&mut self, id: &EnvironmentId) {
        self.environments.retain(|entry| &entry.id != id);
        self.chats.retain(|chat| &chat.environment != id);
    }

    /// Every chat, one per environment, in a stable order.
    pub fn chats(&self) -> &[ChatEntry] {
        &self.chats
    }

    /// This environment's chat, if it has one. An environment without one
    /// is not an error — a human-created environment has no conversation
    /// until someone starts an agent in it.
    pub fn chat_for(&self, env: &EnvironmentId) -> Option<&ChatEntry> {
        self.chats.iter().find(|chat| &chat.environment == env)
    }

    /// Record an environment's chat, replacing whatever it had.
    ///
    /// This is the only way a chat enters the state, and it is why "one
    /// chat per environment" cannot be violated by a caller that forgets:
    /// the environment is the key, so a second write to the same
    /// environment is an update rather than an addition.
    pub fn set_chat(&mut self, chat: ChatEntry) {
        match self
            .chats
            .iter_mut()
            .find(|existing| existing.environment == chat.environment)
        {
            Some(existing) => *existing = chat,
            None => self.chats.push(chat),
        }
    }

    /// Replace every chat at once — the window writing what it has open.
    /// Normalized on the way in, so a caller handing over two chats for one
    /// environment cannot install a state the panes cannot render.
    pub fn set_chats(&mut self, chats: Vec<ChatEntry>) {
        self.chats = Vec::with_capacity(chats.len());
        for chat in chats {
            self.set_chat(chat);
        }
    }

    /// Drop an environment's chat while keeping the environment.
    pub fn forget_chat(&mut self, env: &EnvironmentId) {
        self.chats.retain(|chat| &chat.environment != env);
    }

    /// Enforce the invariant on state that came from outside this process.
    ///
    /// A file is bytes on disk: it can be hand-edited, half-written, or
    /// written by a build whose ideas differed. The first chat named for an
    /// environment wins and the rest are dropped, in exactly the spirit the
    /// orchestrator role is settled after a restore — decide once, here,
    /// rather than leaving it to whichever pane notices first.
    fn settle_chats(&mut self) -> bool {
        let before = self.chats.len();
        let mut seen: Vec<EnvironmentId> = Vec::with_capacity(before);
        self.chats.retain(|chat| {
            if seen.contains(&chat.environment) {
                return false;
            }
            seen.push(chat.environment.clone());
            true
        });
        before != self.chats.len()
    }
}

/// One environment's chat: which agent it talks to, which conversation it
/// holds, and the model chosen for it. Session and model settings travel
/// with the environment, so this is the whole of a chat's restorable
/// identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// devcontainer, its exec target.
    ///
    /// Required, and the key this chat is stored under: a chat *is* an
    /// environment's conversation. The primary environment's chat is the
    /// one the user talks to about their own checkout, and it is an
    /// environment like any other here.
    #[serde(default = "EnvironmentId::primary")]
    pub environment: EnvironmentId,
    /// What this chat is *for*. Absent — the overwhelmingly common case —
    /// is an ordinary chat.
    #[serde(default)]
    pub role: Option<ChatRole>,
}

impl Default for ChatEntry {
    /// A chat of the user's own environment, talking to the default agent.
    /// Hand-written rather than derived because a chat's environment has no
    /// empty value — the primary is what "no environment named" means.
    fn default() -> Self {
        Self {
            agent_id: None,
            session_id: None,
            model_value: None,
            permission_mode: None,
            auto_approve: false,
            environment: EnvironmentId::primary(),
            role: None,
        }
    }
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
            chats: Vec::new(),
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
        Ok(mut state) if state.version == STATE_VERSION => {
            // Deserialization goes around the accessors, so the invariant is
            // re-established here rather than assumed of a file.
            if state.settle_chats() {
                tracing::warn!(
                    "workspace state {} named more than one chat for an environment \
                     — keeping the first of each",
                    path.display()
                );
            }
            (state, false)
        }
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

/// Write the state out **atomically**.
///
/// `fs::write` truncates before it fills, so a reader arriving in that
/// window sees an empty or half-written file — and this file's whole job is
/// to be read at startup. A crash mid-write left the next launch parsing
/// nothing and calling it a schema reset, which is the alpha reset notice
/// firing over a file that was never stale.
///
/// The temporary carries the pid, so two processes writing at once cannot
/// collide on it. They can still both write — the supervising window is the
/// only one that saves (see `crate::instance`), but a folder whose lock
/// could not be taken at all has every window saving — and with the rename
/// the loser's file is simply replaced whole rather than interleaved with
/// the winner's. Last close wins, and what lands is always one window's
/// complete idea of the session.
pub fn save_to(base: &Path, root: &Path, state: &WorkspaceState) -> Result<()> {
    std::fs::create_dir_all(base).context("creating state dir")?;
    let path = file_for(base, root);
    let json = serde_json::to_string_pretty(state).context("serializing state")?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, json).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("installing {}", path.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    /// A chat of the user's own environment.
    fn chat(agent: &str, session: &str) -> ChatEntry {
        ChatEntry {
            agent_id: Some(agent.into()),
            session_id: Some(session.into()),
            ..Default::default()
        }
    }

    /// ...and one of an agent environment's.
    fn chat_in(slug: &str, agent: &str, session: &str) -> ChatEntry {
        ChatEntry {
            environment: env(slug),
            ..chat(agent, session)
        }
    }

    fn with_chats(root: &Path, chats: Vec<ChatEntry>) -> WorkspaceState {
        let mut state = WorkspaceState {
            root: root.to_path_buf(),
            ..Default::default()
        };
        state.set_chats(chats);
        state
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

        let mut state = WorkspaceState {
            version: STATE_VERSION,
            root: root.to_path_buf(),
            open_files: vec![root.join("src/main.rs"), root.join("README.md")],
            active_file: Some(root.join("src/main.rs")),
            ..Default::default()
        };
        state.set_chat(chat("claude-code", "sess-abc"));
        save_to(base.path(), root, &state).unwrap();
        assert_eq!(load_from(base.path(), root), state);
    }

    /// One chat per environment, each carrying its own session settings.
    /// The environment is the key, so the chats come back attached to the
    /// worlds they work in rather than to a position in a tab strip.
    #[test]
    fn one_chat_per_environment_round_trips_with_its_settings() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");
        let state = with_chats(
            root,
            vec![
                chat("claude-code", "sess-1"),
                ChatEntry {
                    model_value: Some("opus[1m]".into()),
                    permission_mode: Some("auto".into()),
                    auto_approve: true,
                    ..chat_in("calm-1", "claude-code", "sess-2")
                },
                // An environment whose agent has never been prompted: a
                // chat with no session yet.
                ChatEntry {
                    agent_id: Some("gemini".into()),
                    environment: env("spry-2"),
                    ..Default::default()
                },
            ],
        );
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(loaded.chats().len(), 3);
        let calm = loaded.chat_for(&env("calm-1")).unwrap();
        assert_eq!(calm.model_value.as_deref(), Some("opus[1m]"));
        assert_eq!(calm.permission_mode.as_deref(), Some("auto"));
        assert!(calm.auto_approve);
        let mine = loaded.chat_for(&EnvironmentId::primary()).unwrap();
        assert!(!mine.auto_approve, "a chat carries its own settings");
        assert_eq!(mine.session_id.as_deref(), Some("sess-1"));
    }

    /// The invariant, at the layer that owns it: an environment has at most
    /// one chat, so writing a second one for the same environment REPLACES
    /// the first rather than growing a strip of two.
    #[test]
    fn an_environment_has_at_most_one_chat() {
        let mut state = WorkspaceState::default();
        state.set_chat(chat_in("calm-1", "claude-code", "first"));
        state.set_chat(chat_in("calm-1", "claude-code", "second"));
        assert_eq!(state.chats().len(), 1, "the second is not a second chat");
        assert_eq!(
            state
                .chat_for(&env("calm-1"))
                .unwrap()
                .session_id
                .as_deref(),
            Some("second")
        );
        // ...and the same through the bulk write the window uses.
        state.set_chats(vec![
            chat("claude-code", "mine"),
            chat_in("calm-1", "claude-code", "a"),
            chat_in("calm-1", "gemini", "b"),
        ]);
        assert_eq!(state.chats().len(), 2);
        assert_eq!(
            state.chat_for(&env("calm-1")).unwrap().agent_id.as_deref(),
            Some("gemini"),
            "the last write wins, as an update would"
        );
    }

    /// A file is bytes: hand-edited, half-written, or from a build with
    /// other ideas. Two chats for one environment are settled on load —
    /// first wins — rather than handed to a pane that cannot render them.
    #[test]
    fn a_file_naming_two_chats_for_one_environment_is_settled_on_load() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/dupes");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            format!(
                r#"{{"version":{STATE_VERSION},"chats":[
                    {{"agent_id":"claude-code","session_id":"first","environment":"calm-1"}},
                    {{"agent_id":"gemini","session_id":"second","environment":"calm-1"}}]}}"#
            ),
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(!reset, "settling a duplicate is not a schema reset");
        assert_eq!(state.chats().len(), 1);
        assert_eq!(
            state
                .chat_for(&env("calm-1"))
                .unwrap()
                .session_id
                .as_deref(),
            Some("first")
        );
    }

    /// A chat with no environment named is the primary's — the user's own
    /// checkout is an environment like any other here.
    #[test]
    fn an_unnamed_environment_reads_as_the_primary() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/implicit");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            format!(
                r#"{{"version":{STATE_VERSION},
                     "chats":[{{"agent_id":"claude-code","session_id":"s"}}]}}"#
            ),
        )
        .unwrap();
        let state = load_from(base.path(), root);
        assert_eq!(state.chats()[0].environment, EnvironmentId::primary());
        assert!(state.chat_for(&EnvironmentId::primary()).is_some());
    }

    /// Destroying an environment destroys its conversation with it: there
    /// is nowhere else for a chat to live, and an entry naming a deleted
    /// clone would restore as a pane with no world.
    #[test]
    fn forgetting_an_environment_forgets_its_chat() {
        let mut state = WorkspaceState::default();
        state.note_environment_created(&env("calm-1"), "2026-09-01T10:00:00Z".into());
        state.set_chat(chat("claude-code", "mine"));
        state.set_chat(chat_in("calm-1", "claude-code", "theirs"));
        state.forget_environment(&env("calm-1"));
        assert_eq!(state.chats().len(), 1);
        assert!(state.chat_for(&env("calm-1")).is_none());
        assert!(state.chat_for(&EnvironmentId::primary()).is_some());
        assert!(state.environments.is_empty());
    }

    /// An environment can lose its agent and keep existing — a human
    /// environment the user is done talking to.
    #[test]
    fn a_chat_can_be_forgotten_without_its_environment() {
        let mut state = WorkspaceState::default();
        state.note_environment_created(&env("calm-1"), "2026-09-01T10:00:00Z".into());
        state.set_chat(chat_in("calm-1", "claude-code", "theirs"));
        state.forget_chat(&env("calm-1"));
        assert!(state.chat_for(&env("calm-1")).is_none());
        assert_eq!(state.environments.len(), 1, "the environment stays");
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
        let state = with_chats(
            root,
            vec![
                ChatEntry {
                    role: Some(ChatRole::Orchestrator),
                    ..chat_in("hub", "claude-code", "sess-hub")
                },
                chat("claude-code", "sess-2"),
            ],
        );
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(
            loaded.chat_for(&env("hub")).unwrap().role,
            Some(ChatRole::Orchestrator)
        );
        assert_eq!(
            loaded.chat_for(&EnvironmentId::primary()).unwrap().role,
            None
        );
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
        assert!(loaded.chats().is_empty());
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
        assert!(state.chats().is_empty());
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
        assert!(state.chats().is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    /// ...and so is a v4 file, which is the one that could hold two chats
    /// in one environment. Merging them would mean choosing which
    /// conversation an environment keeps, and nothing in a state file says
    /// which one the user meant. Alpha rules: reset, and say so.
    #[test]
    fn a_multi_chat_v4_file_is_discarded_rather_than_merged() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/v4");
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(
            file_for(base.path(), root),
            r#"{"version":4,"root":"/work/v4","open_chats":[
                {"agent_id":"claude-code","session_id":"one"},
                {"agent_id":"claude-code","session_id":"two"}],
                "active_chat":1}"#,
        )
        .unwrap();
        let (state, reset) = load_reporting_from(base.path(), root);
        assert!(reset, "the user hears about it once");
        assert!(state.chats().is_empty());
    }

    /// A chat's environment binding and the environment inventory survive a
    /// roundtrip.
    #[test]
    fn environment_binding_roundtrips() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/multi");
        let review = env("review");
        let mut state = with_chats(
            root,
            vec![
                chat("claude-code", "on-primary"),
                chat_in("review", "claude-code", "in-review"),
            ],
        );
        state.environments = vec![EnvironmentEntry {
            id: review.clone(),
            display_name: Some("Review".into()),
            created_at: Some("2026-08-31T12:00:00Z".into()),
        }];
        save_to(base.path(), root, &state).unwrap();
        let loaded = load_from(base.path(), root);
        assert_eq!(loaded, state);
        assert_eq!(loaded.chats()[0].environment, EnvironmentId::primary());
        assert_eq!(loaded.chats()[1].environment, review);
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
            &with_chats(a, vec![chat("claude-code", "a")]),
        )
        .unwrap();
        save_to(
            base.path(),
            b,
            &with_chats(b, vec![chat("claude-code", "b")]),
        )
        .unwrap();
        let session = |state: WorkspaceState| state.chats()[0].session_id.clone();
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

    /// The state file is written to be read at startup, so a reader must
    /// never catch it half-formed. A plain write truncates before it fills:
    /// a crash in that window left the next launch parsing nothing, calling
    /// it a stale schema, and telling the user their session was reset when
    /// nothing had changed but the timing.
    #[test]
    fn saving_is_atomic_and_leaves_no_temporaries() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/atomic");
        let mut state = WorkspaceState {
            root: root.to_path_buf(),
            ..Default::default()
        };
        state.set_chat(chat("claude-code", "sess"));
        save_to(base.path(), root, &state).unwrap();
        // Saving over an existing file is the common case and the one that
        // truncates.
        save_to(base.path(), root, &state).unwrap();
        assert_eq!(load_from(base.path(), root), state);

        let leftovers: Vec<_> = std::fs::read_dir(base.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
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
