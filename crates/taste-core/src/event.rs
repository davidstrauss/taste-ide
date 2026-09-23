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
/// What kind of answer an [`Event::AskRequested`] wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    /// A password, passphrase, or PIN: typed hidden.
    Secret,
    /// Something readable — a username, mostly.
    Text,
    /// Yes or no: a host key to accept.
    Confirm,
    /// Nothing to type: "touch your security key", shown until the asker
    /// is done.
    Notice,
}

/// Where the guest image — the operating system this machine's VMs boot —
/// stands, as a fetch progresses or as the disk says. One value for the
/// header's indicator, its tooltip, the app log, and the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestImageFetch {
    /// The pinned release, e.g. `44.20260829.3.1`.
    pub release: String,
    pub phase: GuestImagePhase,
    /// Bytes done of `total` in the current phase; both zero when the
    /// phase has no measure (verifying, absent).
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestImagePhase {
    /// Nothing on disk and nothing under way.
    Absent,
    /// The compressed image is downloading.
    Fetching,
    /// Downloaded; being decompressed to the base image VMs overlay.
    Decompressing,
    /// Decompressed; its digest being checked against the pin.
    Verifying,
    /// The base image is on disk and checked. Nothing to show.
    Ready,
}

impl GuestImagePhase {
    /// Whether something is happening that a person would want to see.
    pub fn active(self) -> bool {
        matches!(self, Self::Fetching | Self::Decompressing | Self::Verifying)
    }
}

impl GuestImageFetch {
    /// Done over total, or `None` where the phase has no measure.
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).clamp(0.0, 1.0))
    }
}

/// A step of the primary's startup that happens once for the workspace
/// rather than inside the environment's own build — the startup page's
/// rows, for a step's conclusion to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupStage {
    Sweep,
    GuestImage,
    Vm,
    ServiceImage,
    Files,
    Place,
}

/// Who a migration notice is for (`taste_devcontainer::migration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationAudience {
    /// The moving environment's own agent, told a move is pending — said
    /// again every ten minutes, so a notice to a chat mid-turn may wait
    /// for the next.
    Agent,
    /// The moved environment's agent, told its move is done — said once,
    /// so it is queued behind a running turn rather than dropped.
    Moved,
    /// The coordinator, who approves moves.
    Coordinator,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Git working-tree status changed (files staged, modified, committed…).
    GitStatusChanged,
    /// The guest image this machine's VMs boot is being fetched,
    /// decompressed, or verified — or is ready. Published by the
    /// environment registry as the download runs (throttled), and answered
    /// on demand from the disk when the header asks to freshen.
    GuestImage(GuestImageFetch),
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
    /// A line of a VM's story: the provisioner's steps as the IDE takes
    /// them, and the guest's own serial console as it boots. Keyed by the
    /// domain, since a VM hosts several environments; the Logs section's
    /// Virtual Machine row shows it for each of them.
    VmLog { domain: String, line: String },
    /// How far the step a VM's story is at has got — "seeding the
    /// checkout: sending objects, 45%" — said at most once a second and
    /// never written to the log, since each one replaces the last. The
    /// startup page shows it as the step's detail.
    VmProgress { domain: String, line: String },
    /// What a startup step came to, once it is done: "Stopped 2 unused
    /// VMs. 12.0 GiB of memory available for IDE VMs." The startup page
    /// shows it under the step's check, so a finished list reads as what
    /// is now true rather than as a row of ticks.
    StartupConcluded {
        stage: StartupStage,
        summary: String,
    },
    /// Words for an agent about an environment's move to a VM on the
    /// current guest release: its own agent told the move is pending, or
    /// that it is done; the coordinator told one waits on its approval.
    /// The window delivers them as a prompt to that chat.
    /// Lines a task's run said (`crate::tasks`), in a batch.
    TaskOutput {
        env: crate::environment::EnvironmentId,
        name: String,
        lines: Vec<String>,
    },
    /// A task's run started or ended: its row's light changed.
    TaskState {
        env: crate::environment::EnvironmentId,
        name: String,
    },
    /// An environment's agent offered the user replies to its last
    /// question (`suggest_replies`): its chat shows them as buttons, and a
    /// click sends one as the user's message.
    SuggestedReplies {
        env: crate::environment::EnvironmentId,
        replies: Vec<String>,
    },
    MigrationNotice {
        env: crate::environment::EnvironmentId,
        audience: MigrationAudience,
        text: String,
    },
    /// A line the container itself wrote — its main process's stdout or
    /// stderr, as `podman logs --follow` hands it on. The devcontainer spec
    /// has no notion of a log; this stream is the one thing a container
    /// formally has, and the supervisor follows it for as long as the
    /// container runs.
    ContainerOutput { env: EnvironmentId, line: String },
    /// An environment joined the workspace's registry — created by the user,
    /// or picked back up from its clone at startup. The MCP server binds
    /// that environment's socket on this, which is what gives the
    /// environment an identity agents can connect to: the socket IS the
    /// identity, so an environment with no socket is unreachable.
    EnvironmentCreated { env: EnvironmentId },
    /// An environment left the registry: its clone, container and volumes
    /// are gone, and so is its socket.
    EnvironmentRemoved { env: EnvironmentId },
    /// An environment's working copy is somewhere else now — the primary's,
    /// placed in the workspace's VM by the registry — and this is how its
    /// files are reached from here on. The window relays it to the
    /// workspace and re-aims the panes.
    CheckoutMoved {
        env: EnvironmentId,
        checkout: crate::environment::Checkout,
        files: crate::files::Files,
    },
    /// An environment moved along the review arc (`taste_core::review`) —
    /// flagged for review, merged, rejected, or put back to work. The fleet
    /// view redraws on this rather than polling the board.
    EnvironmentReviewChanged { env: EnvironmentId },
    /// An item was filed on the backlog (`refs/taste/issues`), by an agent
    /// through `issue_create` or by the user in the IDE's own composer.
    ///
    /// `by` is who filed it: `None` is the user, the one filer that is not
    /// an environment. The coordinator is woken by this, and skips its own
    /// filings — being told about the issue it just wrote would be a turn
    /// spent to learn nothing, and a loop if it filed another in reply.
    ///
    /// Published where the issue is written, not derived from a re-read of
    /// the ref: the read cannot say who filed it (an agent's issue and the
    /// user's both carry `primary` as the reporter when the coordinator is
    /// the one asking), and who filed it is the whole of the question.
    IssueFiled {
        id: String,
        title: String,
        by: Option<EnvironmentId>,
    },
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
    /// Bring the console's Devcontainer log tab to the front.
    ShowDevcontainerLog,
    /// Open the primary's Virtual Machine log: the banner's View Log while
    /// the VM is the stage in progress.
    ShowVmLog,
    /// A command console tab's process ended (e.g. a sign-in TUI). `tail`
    /// is the last screenful the tab showed, for the one flow that reads
    /// its output: `claude setup-token` prints the token the settings
    /// shade then stores (chat.rs → `on_sign_in_finished`).
    CommandTabExited {
        title: String,
        status: i32,
        tail: String,
    },
    /// SIGINT/SIGTERM (Ctrl+C on the launching console, container stop):
    /// close the window gracefully so state persists.
    QuitRequested,
    /// A user clicked a URL (terminal Ctrl+click): open it in the
    /// browser, or fall back to the clipboard when there is none.
    OpenUrlRequested(String),
    /// A user clicked an issue reference in a chat (`issue_pill`): select
    /// that issue — or its environment, once it has one — in the backlog.
    RevealIssueRequested(String),
    /// A sentence for one environment's chat from something that is not
    /// the agent: the auth proxy waking a private server before its turn
    /// (`taste_authproxy::wake`). Drawn as a note in that chat's
    /// transcript.
    /// A note for one chat's transcript. With a `key`, a later notice
    /// with the same key REPLACES the row rather than adding one: a wake-up
    /// that is sent every retry is one line counting up, not a column of
    /// identical lines (David, 2026-09-21).
    ChatNotice {
        env: EnvironmentId,
        key: Option<String>,
        text: String,
    },
    /// A reload the environment's own agent asked for (devcontainer_reload,
    /// approved by the user) has finished: `ok` says whether the project's
    /// environment came up, `message` is the failure when it did not. The
    /// agent lived in the container it asked to rebuild, so it died with
    /// the reload and comes back knowing nothing; the chat hands it this
    /// as its next prompt (David, 2026-09-16: "notify it when the relaunch
    /// is complete — and whether it was successful and how").
    ReloadReport {
        env: EnvironmentId,
        ok: bool,
        message: String,
    },
    /// The auth proxy read the account's model listing and the top tier
    /// changed: a project provisioned after launch just had its first
    /// working turn, or a re-provision moved it to another account.
    /// `top_tier` is the newest model above Opus the credential can run,
    /// or `None` when there is none. A Claude Code pane whose picker was
    /// composed before this respawns onto the same conversation to take
    /// the row (`taste_acp::authproxy::spawn_env`).
    ModelsRefreshed { top_tier: Option<String> },
    /// Git or ssh, running a Pull or Push the user pressed, has a question
    /// — or a notice — for the person: a passphrase, a PIN, a host key to
    /// accept, or "touch your security key". Drawn in the safe-mode
    /// banner's strip (`taste_app::devcontainer_ui`), answered through
    /// `taste_app::askpass::answer`, and withdrawn by `AskDone` when the
    /// asker has gone away or been answered.
    AskRequested {
        id: u64,
        prompt: String,
        kind: AskKind,
    },
    /// The question or notice `id` is over.
    AskDone { id: u64 },
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
    /// window routes (e.g. "chat-destroy-session", or
    /// "prompt-repair:<environment>" for a failure an agent can be handed).
    /// `timeout_seconds` is how long it stays before dismissing itself, 0
    /// for until dismissed: a toast with a button is one the reader has to
    /// reach, and the five seconds a plain notice gets are not enough for
    /// that (David, 2026-09-16: "Add a Prompt Agent button to this, and
    /// leave it up for longer").
    ToastAction {
        message: String,
        label: String,
        action: String,
        timeout_seconds: u32,
    },
    /// Transient user-facing feedback (rendered as an AdwToast). The HIG
    /// convention for action outcomes: visible, non-blocking, ephemeral.
    Toast(String),
    /// A face button on a game controller went down or up (taste-app's
    /// controller.rs reads the pad; compose.rs answers).
    Controller {
        button: ControllerButton,
        pressed: bool,
    },
}

/// The buttons of an Xbox-layout controller the IDE answers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerButton {
    A,
    B,
    X,
    Y,
    LeftShoulder,
    RightShoulder,
    Start,
    /// The logo button, held: the key reveal (taste-app's reveal.rs).
    Guide,
    Up,
    Down,
    /// The triggers, as buttons: a pull past halfway is a press, and the
    /// release back below it is the release. They step the backlog (David,
    /// 2026-09-16: "allow left/right trigger to step through backlog
    /// items").
    LeftTrigger,
    RightTrigger,
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
            | Event::ContainerOutput { env, .. }
            | Event::EnvironmentCreated { env }
            | Event::ShellRosterChanged { env } => Some(env),
            // An agent filing an issue is that agent working, so it draws
            // its row's sparkline; the user's own filing carries no
            // environment and belongs to no row.
            Event::IssueFiled { by, .. } => by.as_ref(),
            // Named, but not activity: nothing is happening in an
            // environment that has just stopped existing — nor in one that
            // has just been flagged for review, which is precisely the
            // announcement that it has *stopped* happening.
            Event::EnvironmentRemoved { .. }
            | Event::EnvironmentReviewChanged { .. }
            | Event::CheckoutMoved { .. } => None,
            // Untagged. A workspace-wide fact belongs to no row, and
            // attributing it to the primary would draw the user's own
            // sparkline every time a file changed anywhere.
            Event::GitStatusChanged
            | Event::GuestImage(_)
            | Event::FlatpakState(_)
            | Event::FlatpakLog(_)
            | Event::AgentSessionUpdate { .. }
            | Event::FileChanged(_)
            | Event::FileTreeChanged
            | Event::OpenFileRequested { .. }
            | Event::ShowDevcontainerLog
            | Event::ShowVmLog
            | Event::CommandTabExited { .. }
            | Event::QuitRequested
            | Event::OpenUrlRequested(_)
            | Event::RevealIssueRequested(_)
            | Event::ChatNotice { .. }
            | Event::VmLog { .. }
            | Event::VmProgress { .. }
            | Event::StartupConcluded { .. }
            | Event::MigrationNotice { .. }
            | Event::TaskOutput { .. }
            | Event::TaskState { .. }
            | Event::SuggestedReplies { .. }
            | Event::ReloadReport { .. }
            | Event::ModelsRefreshed { .. }
            | Event::AskRequested { .. }
            | Event::AskDone { .. }
            | Event::CreateDevcontainerConfig
            | Event::CreateFileRequested { .. }
            | Event::RunInTerminal { .. }
            | Event::ToastAction { .. }
            | Event::Toast(_)
            | Event::Controller { .. } => None,
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

/// How long a toast with a button stays up: long enough to read the
/// failure and reach the button, short enough not to be furniture.
pub const ACTION_TOAST_SECS: u32 = 30;

/// Devcontainer supervisor states, mirrored from `taste-devcontainer` so the
/// UI and MCP server need not depend on it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevcontainerStateEvent {
    /// The IDE is getting the environment somewhere it can run — the VM
    /// coming up, its files service connecting, the checkout being placed
    /// — and `what` is the step under way.
    Preparing {
        what: String,
    },
    NoConfig,
    ConfigDetected,
    Building,
    Starting,
    Running {
        container_id: String,
    },
    Failed {
        message: String,
    },
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
