//! Per-workspace restore state, the industry-normal way.
//!
//! There is deliberately no "project" concept beyond this: recently opened
//! folders live in the desktop's recent-files list (GtkRecentManager), and
//! what was last open lives here — a JSON file per workspace under
//! `$XDG_STATE_HOME/taste-ide/workspaces/` (the XDG base-dir spec's home
//! for exactly this kind of data: open files, layout, session handles).
//! The chat session itself is restored through ACP's own `session/load`;
//! only its id is persisted here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// The workspace root (for human inspection of the state file).
    #[serde(default)]
    pub root: PathBuf,
    #[serde(default)]
    pub open_files: Vec<PathBuf>,
    #[serde(default)]
    pub active_file: Option<PathBuf>,
    /// The agent the chat pane was using (registry id).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// The ACP session id, restorable via `session/load`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The user's explicit model choice for this project (config option
    /// value id). Absent = follow the agent's default.
    #[serde(default)]
    pub model_value: Option<String>,
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
    let path = file_for(base, root);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
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

    #[test]
    fn roundtrips_and_defaults_on_missing() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");

        assert_eq!(load_from(base.path(), root), WorkspaceState::default());

        let state = WorkspaceState {
            root: root.to_path_buf(),
            open_files: vec![root.join("src/main.rs"), root.join("README.md")],
            active_file: Some(root.join("src/main.rs")),
            agent_id: Some("claude-code".into()),
            session_id: Some("sess-abc".into()),
            model_value: Some("opus".into()),
        };
        save_to(base.path(), root, &state).unwrap();
        assert_eq!(load_from(base.path(), root), state);
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
                session_id: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        save_to(
            base.path(),
            b,
            &WorkspaceState {
                session_id: Some("b".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(base.path(), a).session_id.as_deref(), Some("a"));
        assert_eq!(load_from(base.path(), b).session_id.as_deref(), Some("b"));
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
