//! The event bus connecting tokio-side services to the GTK main loop.

use std::path::PathBuf;

use crate::environment::EnvironmentId;

/// Events published by background services and consumed by the UI (and by
/// the MCP server, which mirrors some of this state to agents).
///
/// Every devcontainer event names the environment it came from. There is no
/// untagged variant and no default: a workspace supervises N environments,
/// and a subscriber that cannot say which one an event belongs to would
/// paint one environment's build log over another's. Subscribers aimed at a
/// single environment (today: all of them, at the primary) compare the tag
/// and drop the rest.
#[derive(Debug, Clone)]
pub enum Event {
    /// Git working-tree status changed (files staged, modified, committed…).
    GitStatusChanged,
    /// One environment's devcontainer lifecycle moved to a new state.
    DevcontainerState {
        env: EnvironmentId,
        state: DevcontainerStateEvent,
    },
    /// An environment's devcontainer configuration on disk no longer matches
    /// its running container. Raises the persistent "rebuild" banner and the
    /// MCP flag.
    DevcontainerPendingChanges { env: EnvironmentId, pending: bool },
    /// A line of devcontainer build/startup output (mirrored to the
    /// supervisor console tab and the MCP log ring buffer).
    DevcontainerLog { env: EnvironmentId, line: String },
    /// An environment joined the workspace's registry — created by the user,
    /// or picked back up from its clone at startup. The MCP server binds
    /// that environment's socket on this, which is what gives the
    /// environment an identity agents can connect to: the socket IS the
    /// identity, so an environment with no socket is unreachable.
    EnvironmentCreated { env: EnvironmentId },
    /// An environment left the registry: its clone, container and volumes
    /// are gone, and so is its socket.
    EnvironmentRemoved { env: EnvironmentId },
    /// An environment's shell roster changed — a shell appeared, ended, or
    /// was released ([`crate::shells`]). Deliberately coarse: subscribers
    /// re-list, because the alternative is a per-byte event, and terminal
    /// output on a broadcast bus is work every subscriber does only to
    /// throw away. Output reaches an open tab through
    /// [`crate::ShellRoster::watch`] instead.
    ShellRosterChanged { env: EnvironmentId },
    /// The Flatpak packaging pipeline moved to a new state.
    FlatpakState(FlatpakStateEvent),
    /// A line of Flatpak build/install output (mirrored to the Flatpak
    /// console tab and the MCP log ring buffer).
    FlatpakLog(String),
    /// An agent session produced an update (streamed chunk, tool call…).
    /// The payload is kept opaque here; `taste-acp` defines the rich type
    /// and the UI downcasts via the session registry.
    AgentSessionUpdate { session_id: String },
    /// A file's *content* changed on disk outside the editor (agent edit,
    /// container build, terminal). Editors reload clean buffers in place.
    FileChanged(PathBuf),
    /// Files were created, removed, or renamed: the tree's structure is
    /// stale and needs a rebuild.
    FileTreeChanged,
    /// An agent asked (over MCP) to show a file in the editor.
    OpenFileRequested { path: PathBuf, line: Option<u32> },
    /// Service totals for the console tab badge (from the Services pane).
    ServiceSummary { total: usize, failed: usize },
    /// Services can't be listed. `systemd_missing` distinguishes a running
    /// container without systemd (warn badge) from no container at all
    /// (neutral badge) — neither is an *issue*, so neither may show red.
    ServicesUnavailable { systemd_missing: bool },
    /// Bring the console's Devcontainer log tab to the front.
    ShowDevcontainerLog,
    /// A command console tab's process ended (e.g. a sign-in TUI).
    CommandTabExited { title: String, status: i32 },
    /// SIGINT/SIGTERM (Ctrl+C on the launching console, container stop):
    /// close the window gracefully so state persists.
    QuitRequested,
    /// A user clicked a URL (terminal Ctrl+click): open it in the
    /// browser, or fall back to the clipboard when there is none.
    OpenUrlRequested(String),
    /// Open a console tab running one specific command (e.g. an agent's
    /// terminal-auth login TUI) in the current execution context.
    /// The safe-mode banner's Create button: open the devcontainer config
    /// the same way the file tree's ghost row would.
    CreateDevcontainerConfig,
    /// Open a NOT-yet-existing file as an unsaved editor buffer, prefilled;
    /// saving materializes it on disk (and thus in the tree).
    CreateFileRequested { path: PathBuf, content: String },
    RunInTerminal {
        title: String,
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        /// True when the command is already wrapped in its execution
        /// context (e.g. the agent's confinement) — run it verbatim
        /// instead of resolving into the devcontainer.
        wrapped: bool,
    },
    /// A toast with one action button; `action` is an app-defined id the
    /// window routes (e.g. "chat-destroy-session").
    ToastAction {
        message: String,
        label: String,
        action: String,
    },
    /// Transient user-facing feedback (rendered as an AdwToast). The HIG
    /// convention for action outcomes: visible, non-blocking, ephemeral.
    Toast(String),
}

impl Event {
    /// The environment this event is evidence of *activity* in, if any —
    /// what [`crate::activity`] counts.
    ///
    /// Not the same question as "does it carry an env tag", and the
    /// difference is the point. `EnvironmentRemoved` names an environment
    /// and is not activity in it: the environment is gone, and the last
    /// thing its ring should record is its own deletion. Everything else
    /// tagged is something happening in there — a lifecycle transition, a
    /// line of build output, a shell appearing or ending, the environment
    /// arriving.
    ///
    /// The match is exhaustive on purpose — no wildcard arm. A new variant
    /// does not silently default to "not activity"; it breaks the build
    /// here and someone decides.
    ///
    /// **What this cannot see**, stated plainly rather than papered over:
    /// terminal bytes and agent turn chunks never ride the bus (see
    /// [`crate::shells`] for why), so those two are counted at their own
    /// choke points — the roster's output path and the chat pane — and not
    /// here. This function is the bus's share of the picture.
    pub fn activity_env(&self) -> Option<&EnvironmentId> {
        match self {
            Event::DevcontainerState { env, .. }
            | Event::DevcontainerPendingChanges { env, .. }
            | Event::DevcontainerLog { env, .. }
            | Event::EnvironmentCreated { env }
            | Event::ShellRosterChanged { env } => Some(env),
            // Named, but not activity: nothing is happening in an
            // environment that has just stopped existing.
            Event::EnvironmentRemoved { .. } => None,
            // Untagged. A workspace-wide fact belongs to no row, and
            // attributing it to the primary would draw the user's own
            // sparkline every time a file changed anywhere.
            Event::GitStatusChanged
            | Event::FlatpakState(_)
            | Event::FlatpakLog(_)
            | Event::AgentSessionUpdate { .. }
            | Event::FileChanged(_)
            | Event::FileTreeChanged
            | Event::OpenFileRequested { .. }
            | Event::ServiceSummary { .. }
            | Event::ServicesUnavailable { .. }
            | Event::ShowDevcontainerLog
            | Event::CommandTabExited { .. }
            | Event::QuitRequested
            | Event::OpenUrlRequested(_)
            | Event::CreateDevcontainerConfig
            | Event::CreateFileRequested { .. }
            | Event::RunInTerminal { .. }
            | Event::ToastAction { .. }
            | Event::Toast(_) => None,
        }
    }
}

/// Flatpak packaging states, mirrored from `taste-flatpak` so the UI and
/// MCP server need not depend on it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatpakStateEvent {
    Building,
    Launching,
    Succeeded,
    Failed { message: String },
}

/// Devcontainer supervisor states, mirrored from `taste-devcontainer` so the
/// UI and MCP server need not depend on it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevcontainerStateEvent {
    NoConfig,
    ConfigDetected,
    Building,
    Starting,
    Running { container_id: String },
    Failed { message: String },
    Stopped,
}

/// Broadcast bus: any number of publishers, any number of subscribers.
/// Subscribers each get every event (clone-per-subscriber).
#[derive(Clone)]
pub struct EventBus {
    senders: std::sync::Arc<std::sync::Mutex<Vec<async_channel::Sender<Event>>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            senders: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Subscribe to all future events. The GTK side drains the returned
    /// receiver with `glib::spawn_future_local`.
    pub fn subscribe(&self) -> async_channel::Receiver<Event> {
        let (tx, rx) = async_channel::unbounded();
        self.senders.lock().unwrap().push(tx);
        rx
    }

    /// Publish an event to every live subscriber. Dead subscribers are
    /// pruned as a side effect.
    pub fn publish(&self, event: Event) {
        let mut senders = self.senders.lock().unwrap();
        senders.retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_subscribers_receive_events() {
        let bus = EventBus::new();
        let a = bus.subscribe();
        let b = bus.subscribe();
        bus.publish(Event::GitStatusChanged);
        assert!(matches!(a.try_recv().unwrap(), Event::GitStatusChanged));
        assert!(matches!(b.try_recv().unwrap(), Event::GitStatusChanged));
    }

    /// Activity is a narrower question than "is it tagged", and the two
    /// answers that are easy to get wrong are asserted directly: a removed
    /// environment is not busy, and a workspace-wide fact is nobody's.
    #[test]
    fn only_events_that_are_activity_somewhere_name_an_environment() {
        let env = EnvironmentId::parse("calm-1").unwrap();
        let counted = [
            Event::DevcontainerState {
                env: env.clone(),
                state: DevcontainerStateEvent::Building,
            },
            Event::DevcontainerPendingChanges {
                env: env.clone(),
                pending: true,
            },
            Event::DevcontainerLog {
                env: env.clone(),
                line: "Step 3/9".into(),
            },
            Event::EnvironmentCreated { env: env.clone() },
            Event::ShellRosterChanged { env: env.clone() },
        ];
        for event in counted {
            assert_eq!(
                event.activity_env(),
                Some(&env),
                "{event:?} happens in an environment"
            );
        }
        assert_eq!(
            Event::EnvironmentRemoved { env: env.clone() }.activity_env(),
            None,
            "a destroyed environment is not busy being destroyed"
        );
        for event in [
            Event::GitStatusChanged,
            Event::FileTreeChanged,
            Event::AgentSessionUpdate {
                session_id: "s1".into(),
            },
            Event::Toast("saved".into()),
        ] {
            assert_eq!(
                event.activity_env(),
                None,
                "{event:?} belongs to no row, so it draws in none"
            );
        }
    }

    #[test]
    fn dropped_subscribers_are_pruned() {
        let bus = EventBus::new();
        drop(bus.subscribe());
        bus.publish(Event::GitStatusChanged);
        assert_eq!(bus.senders.lock().unwrap().len(), 0);
    }
}
