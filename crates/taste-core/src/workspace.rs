//! The open workspace: root directory plus the shared handles every
//! subsystem hangs off.

use std::path::{Path, PathBuf};

use crate::activity::Activity;
use crate::ide_state::IdeState;
use crate::orchestration::OrchestrationProbe;
use crate::review::ReviewBoard;
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
    /// The orchestrator's questions about other chats — asked by the MCP
    /// server on the orchestrator's socket alone, answered by the chat
    /// strip. A separate handle from [`Workspace::ui`] on purpose: these
    /// are execution authority (creating a chat spawns an agent that will
    /// run code), and mixing them into the probe every agent's tools
    /// already reach would make that authority one `match` arm away from
    /// every socket.
    pub orchestration: OrchestrationProbe,
    /// Every live shell, across every environment: the user's terminals,
    /// the agent's ACP terminals, `ide_exec` jobs, the lifecycle streams.
    ///
    /// Workspace-wide and not per environment, because the things that
    /// WRITE to it are workspace-wide — one MCP server, one set of agent
    /// sessions — and every read is already filtered by environment. A
    /// roster per supervisor would have to be collected back together by
    /// the fleet view anyway.
    pub shells: ShellRoster,
    /// How much has been happening in each environment lately
    /// ([`crate::activity`]) — the environment panel's sparklines.
    ///
    /// Workspace-wide for the same reason the roster is: the things that
    /// write to it are workspace-wide (one bus, one roster, one chat
    /// strip), and every read is already by environment.
    pub activity: Activity,
    /// Where each environment stands in the review arc: which are waiting
    /// on the user, which the user has settled.
    ///
    /// Workspace-wide and a handle, for the same reason the roster is one:
    /// the MCP server writes it from tokio when an agent says it is ready,
    /// the console reads it while drawing a row, and the supervisor asks it
    /// whether a container should be down. A copy per environment would be
    /// N answers to a question with one.
    pub review: ReviewBoard,
}

impl Workspace {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let events = EventBus::new();
        let activity = Activity::new();
        let root: PathBuf = root.into();
        let review = ReviewBoard::new(&root);
        review.attach_events(events.clone());
        let shells = ShellRoster::new();
        shells.attach_events(events.clone());
        // Terminal and `ide_exec` output is the liveliest signal there is
        // and the one the bus deliberately refuses to carry, so the panel
        // gets it where the bytes already pass: the roster.
        shells.attach_activity(activity.clone());
        Self {
            root,
            events,
            exec: ExecContext::host(),
            ide: IdeState::default(),
            ui: UiProbe::new(),
            orchestration: OrchestrationProbe::new(),
            shells,
            activity,
            review,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
