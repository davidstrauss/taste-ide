//! The open workspace: root directory plus the shared handles every
//! subsystem hangs off.

use std::path::{Path, PathBuf};

use crate::ide_state::IdeState;
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
}

impl Workspace {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            events: EventBus::new(),
            exec: ExecContext::host(),
            ide: IdeState::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
