//! The open workspace: root directory plus the shared handles every
//! subsystem hangs off.

use std::path::{Path, PathBuf};

use crate::ide_state::IdeState;
use crate::ui_probe::UiProbe;
use crate::{EventBus, ExecContext};

#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    pub events: EventBus,
    /// Where terminals and build/exec commands run (host until the
    /// devcontainer supervisor points it into a container).
    pub exec: ExecContext,
    /// What the user is looking at (open files, selection) — written by the
    /// editor, served to agents over MCP.
    pub ide: IdeState,
    /// Questions only the GTK main thread can answer (screenshots,
    /// computed geometry) — asked by the MCP server, answered by the window.
    pub ui: UiProbe,
}

impl Workspace {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            events: EventBus::new(),
            exec: ExecContext::host(),
            ide: IdeState::default(),
            ui: UiProbe::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
