//! The open workspace: root directory plus the shared handles every
//! subsystem hangs off.

use std::path::{Path, PathBuf};

use crate::ide_state::IdeState;
use crate::shells::ShellRoster;
use crate::ui_probe::UiProbe;
use crate::{EventBus, ExecContext};

#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    pub events: EventBus,
    /// The PRIMARY environment's execution target — where the user's
    /// terminals and `ide_exec` run (host until that environment's
    /// supervisor points it into a container).
    ///
    /// This is a handle, not the singleton it used to be. There is one
    /// [`ExecContext`] per environment and the `EnvironmentRegistry` owns
    /// them all; the workspace keeps the primary's because the call sites
    /// that predate environments (console terminals, the file tree's git
    /// steps, rust-analyzer) are primary-facing by definition. New code
    /// that has an environment in hand must take that environment's
    /// context from the registry instead of reaching for this one — "the"
    /// exec context no longer exists.
    pub exec: ExecContext,
    /// What the user is looking at (open files, selection) — written by the
    /// editor, served to agents over MCP.
    pub ide: IdeState,
    /// Questions only the GTK main thread can answer (screenshots,
    /// computed geometry) — asked by the MCP server, answered by the window.
    pub ui: UiProbe,
    /// Every live shell, across every environment: the user's terminals,
    /// the agent's ACP terminals, `ide_exec` jobs, the lifecycle streams.
    ///
    /// Workspace-wide and not per environment, because the things that
    /// WRITE to it are workspace-wide — one MCP server, one set of agent
    /// sessions — and every read is already filtered by environment. A
    /// roster per supervisor would have to be collected back together by
    /// the fleet view anyway.
    pub shells: ShellRoster,
}

impl Workspace {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let events = EventBus::new();
        let shells = ShellRoster::new();
        shells.attach_events(events.clone());
        Self {
            root: root.into(),
            events,
            exec: ExecContext::host(),
            ide: IdeState::default(),
            ui: UiProbe::new(),
            shells,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
