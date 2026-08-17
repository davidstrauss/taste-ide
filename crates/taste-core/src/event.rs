//! The event bus connecting tokio-side services to the GTK main loop.

use std::path::PathBuf;

/// Events published by background services and consumed by the UI (and by
/// the MCP server, which mirrors some of this state to agents).
#[derive(Debug, Clone)]
pub enum Event {
    /// Git working-tree status changed (files staged, modified, committed…).
    GitStatusChanged,
    /// Devcontainer lifecycle moved to a new state.
    DevcontainerState(DevcontainerStateEvent),
    /// The devcontainer configuration on disk no longer matches the running
    /// container. Raises the persistent "rebuild" banner and the MCP flag.
    DevcontainerPendingChanges { pending: bool },
    /// A line of devcontainer build/startup output (mirrored to the
    /// supervisor console tab and the MCP log ring buffer).
    DevcontainerLog(String),
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

    #[test]
    fn dropped_subscribers_are_pruned() {
        let bus = EventBus::new();
        drop(bus.subscribe());
        bus.publish(Event::GitStatusChanged);
        assert_eq!(bus.senders.lock().unwrap().len(), 0);
    }
}
