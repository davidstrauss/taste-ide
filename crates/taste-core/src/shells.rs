//! The per-environment shell roster: every live process an environment is
//! running on the user's behalf, in one queryable place.
//!
//! ENVIRONMENTS.md → "Watching an environment": the console enumerates, per
//! environment, the user's own terminals (interactive), the agent's ACP
//! terminals (read-only, killable), `ide_exec` jobs (read-only mirrors) and
//! the build/lifecycle stream. This module is that list — **data and
//! events, no widgets, no pane**. The console renders it; a fleet view and
//! the varlink service will read the same struct rather than growing their
//! own inventories.
//!
//! Three deliberate shapes:
//!
//! - **Registration is by whoever spawns.** `taste-acp` registers the
//!   terminals it serves, `taste-mcp` registers its `ide_exec` jobs, the
//!   GTK console registers the user's own shells. None of them knows about
//!   the others, and the roster knows about none of them.
//! - **Bytes do not ride the [`crate::EventBus`].** The bus is an unbounded
//!   broadcast to every subscriber in the process; a `cargo build`'s output
//!   on it would be cloned into the file tree, the git pane and the MCP
//!   server to be dropped by each. Output goes to per-shell watchers that
//!   only an open tab holds, and the bus carries one coarse
//!   [`crate::Event::ShellRosterChanged`] so the UI knows to look again.
//! - **Ids are monotonic and never reused.** A tab keyed by one can outlive
//!   the shell it showed — which is the console's retention policy: the
//!   output stays until the user closes it.
//!
//! **Honest limit, stated where it lives:** a process an agent spawns
//! *without* asking for a terminal is not in here and cannot be. Visibility
//! is by convention — the adapter prefers client-served terminals when the
//! IDE offers them — not by ptrace. After relocation that convention covers
//! nearly everything an agent runs, and nothing here pretends otherwise.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::capped::CappedOutput;
use crate::environment::EnvironmentId;
use crate::{Event, EventBus};

/// A roster-wide handle. Monotonic, never reused.
pub type ShellId = u64;

/// Who asked for this process, which is also who may touch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShellKind {
    /// The user's own terminal, attached to this environment. Interactive:
    /// it is theirs, and the roster only lists it.
    User,
    /// A terminal the agent asked for over ACP's terminal extension.
    /// Read-only to the user — but killable, because stopping a runaway
    /// process is supervision, not editing.
    Agent,
    /// An `ide_exec` job: a read-only mirror of a command the agent ran
    /// through the MCP tool rather than through a terminal.
    ExecJob,
    /// The environment's build/lifecycle stream — one per environment, and
    /// the only entry that is not a process.
    Lifecycle,
}

impl ShellKind {
    /// How the console labels the kind. Short: the label is prefixed by
    /// the environment and followed by the command.
    pub fn noun(self) -> &'static str {
        match self {
            ShellKind::User => "shell",
            ShellKind::Agent => "agent terminal",
            ShellKind::ExecJob => "agent command",
            ShellKind::Lifecycle => "lifecycle",
        }
    }

    /// Whether the user may type into it. Exactly one kind is theirs.
    pub fn interactive(self) -> bool {
        matches!(self, ShellKind::User)
    }
}

/// Where a shell is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellState {
    Running,
    /// Ended. A signal death has no exit code, and saying so beats
    /// inventing a number that reads like a result.
    Exited {
        code: Option<i32>,
        signal: Option<String>,
    },
}

impl ShellState {
    pub fn is_running(&self) -> bool {
        matches!(self, ShellState::Running)
    }

    /// One line for the console header: what happened, in words.
    pub fn summary(&self) -> String {
        match self {
            ShellState::Running => "running".to_string(),
            ShellState::Exited {
                code: Some(0),
                signal: None,
            } => "exited 0".to_string(),
            ShellState::Exited {
                code: Some(code),
                signal: None,
            } => format!("exited {code}"),
            ShellState::Exited {
                signal: Some(signal),
                ..
            } => format!("killed ({signal})"),
            ShellState::Exited {
                code: None,
                signal: None,
            } => "ended".to_string(),
        }
    }
}

/// One row of the roster. Cloned out on every query — it is small, and a
/// caller holding a lock on the roster while it renders is how the GTK
/// thread ends up blocked on a tokio task.
#[derive(Debug, Clone)]
pub struct ShellEntry {
    pub id: ShellId,
    pub env: EnvironmentId,
    pub kind: ShellKind,
    /// The command as a human should read it: what the agent (or user)
    /// asked for, never the `podman exec --env … ` wrapper the IDE built
    /// around it. The wrapper is the IDE's business.
    pub command: String,
    pub state: ShellState,
    /// Whether anything can stop it from here.
    pub killable: bool,
}

impl ShellEntry {
    /// The console's tab title: `<env> · <command>`, as ENVIRONMENTS.md
    /// specifies. Long commands are the norm, so the *caller* ellipsizes —
    /// truncating here would put a lie in the roster.
    pub fn label(&self) -> String {
        format!("{} · {}", self.env, self.command)
    }
}

/// What a watcher receives. Output and state on one channel, in order, so
/// a tab can never paint "exited" above bytes that arrived before it.
#[derive(Debug, Clone)]
pub enum ShellUpdate {
    Output(Vec<u8>),
    State(ShellState),
}

/// The ability to stop a shell. Implemented by whoever spawned it, because
/// only they know what stopping means — a `Notify` the wait task selects
/// on, a job handle, a pty hangup.
///
/// The blanket impl over `Fn()` is why this crate needs no tokio: the ACP
/// terminals and the `ide_exec` jobs both already have a `Notify` to fire,
/// and they hand it over as a closure rather than making taste-core learn
/// what a runtime is.
pub trait ShellControl: Send + Sync {
    fn kill(&self);
}

impl<F: Fn() + Send + Sync> ShellControl for F {
    fn kill(&self) {
        self();
    }
}

struct Shell {
    entry: ShellEntry,
    output: CappedOutput,
    watchers: Vec<async_channel::Sender<ShellUpdate>>,
    control: Option<Arc<dyn ShellControl>>,
}

#[derive(Default)]
struct Inner {
    next: ShellId,
    shells: BTreeMap<ShellId, Shell>,
    events: Option<EventBus>,
}

/// Every live shell in this workspace, across every environment.
///
/// Cheap to clone (it is a handle), safe to hold anywhere: no GTK, no
/// blocking IO, and the lock is never held across an await or a render.
#[derive(Clone, Default)]
pub struct ShellRoster {
    inner: Arc<Mutex<Inner>>,
}

impl ShellRoster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the bus the roster announces changes on. Without one the
    /// roster still works and still records — it just goes unwatched, which
    /// is what every headless test wants.
    pub fn attach_events(&self, events: EventBus) {
        self.inner.lock().unwrap().events = Some(events);
    }

    /// Add a shell and get the writer for it.
    ///
    /// `command` is what a human should read. `control` is `None` for
    /// anything that cannot be stopped from here (the lifecycle stream, a
    /// user shell the console drives itself).
    pub fn register(
        &self,
        env: EnvironmentId,
        kind: ShellKind,
        command: impl Into<String>,
        control: Option<Arc<dyn ShellControl>>,
    ) -> ShellSink {
        let (id, events) = {
            let mut inner = self.inner.lock().unwrap();
            inner.next += 1;
            let id = inner.next;
            let entry = ShellEntry {
                id,
                env: env.clone(),
                kind,
                command: command.into(),
                state: ShellState::Running,
                killable: control.is_some(),
            };
            inner.shells.insert(
                id,
                Shell {
                    entry,
                    output: CappedOutput::default(),
                    watchers: Vec::new(),
                    control,
                },
            );
            (id, inner.events.clone())
        };
        announce(events, env);
        ShellSink {
            roster: self.clone(),
            id,
        }
    }

    /// Every shell, or every shell of one environment, oldest first.
    pub fn list(&self, env: Option<&EnvironmentId>) -> Vec<ShellEntry> {
        self.inner
            .lock()
            .unwrap()
            .shells
            .values()
            .filter(|shell| env.is_none_or(|env| &shell.entry.env == env))
            .map(|shell| shell.entry.clone())
            .collect()
    }

    pub fn get(&self, id: ShellId) -> Option<ShellEntry> {
        self.inner
            .lock()
            .unwrap()
            .shells
            .get(&id)
            .map(|shell| shell.entry.clone())
    }

    /// Everything this shell has produced so far, plus a stream of what
    /// comes next.
    ///
    /// Snapshot and subscription happen under one lock on purpose: taking
    /// them separately drops whatever arrived in between, which is exactly
    /// the output a user opening a tab mid-build is looking for.
    pub fn watch(&self, id: ShellId) -> Option<(String, async_channel::Receiver<ShellUpdate>)> {
        let mut inner = self.inner.lock().unwrap();
        let shell = inner.shells.get_mut(&id)?;
        let backlog = shell.output.render();
        let (tx, rx) = async_channel::unbounded();
        // An already-finished shell gets its ending replayed too, so a tab
        // opened after the fact does not sit there claiming "running".
        if !shell.entry.state.is_running() {
            let _ = tx.try_send(ShellUpdate::State(shell.entry.state.clone()));
        }
        shell.watchers.push(tx);
        Some((backlog, rx))
    }

    /// Ask a shell to stop. A shell with no control, or none by that id, is
    /// a no-op — the button that called this may well have raced the exit.
    pub fn kill(&self, id: ShellId) {
        let control = self
            .inner
            .lock()
            .unwrap()
            .shells
            .get(&id)
            .and_then(|shell| shell.control.clone());
        // Outside the lock: an implementation is free to do whatever
        // stopping means for it, and none of that should block a query.
        if let Some(control) = control {
            control.kill();
        }
    }

    /// Drop a shell from the roster.
    ///
    /// The console deliberately keeps a tab whose shell was removed: the
    /// output is the record of what happened, and it stays until the user
    /// closes it. Removal only means nothing new will arrive.
    pub fn remove(&self, id: ShellId) {
        let (env, events) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(shell) = inner.shells.remove(&id) else {
                return;
            };
            // Watchers learn by the channel closing, which is what ends
            // their pump task.
            (shell.entry.env, inner.events.clone())
        };
        announce(events, env);
    }

    fn push(&self, id: ShellId, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        let Some(shell) = inner.shells.get_mut(&id) else {
            return;
        };
        shell.output.push(bytes);
        let update = ShellUpdate::Output(bytes.to_vec());
        shell
            .watchers
            .retain(|tx| tx.try_send(update.clone()).is_ok());
    }

    fn finish(&self, id: ShellId, state: ShellState) {
        let (env, events) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(shell) = inner.shells.get_mut(&id) else {
                return;
            };
            shell.entry.state = state.clone();
            shell.entry.killable = false;
            let update = ShellUpdate::State(state);
            shell
                .watchers
                .retain(|tx| tx.try_send(update.clone()).is_ok());
            (shell.entry.env.clone(), inner.events.clone())
        };
        announce(events, env);
    }
}

fn announce(events: Option<EventBus>, env: EnvironmentId) {
    if let Some(events) = events {
        events.publish(Event::ShellRosterChanged { env });
    }
}

/// The writing end of one roster entry, held by whoever spawned the
/// process. Cloneable so a stdout and a stderr drain can share it.
#[derive(Clone)]
pub struct ShellSink {
    roster: ShellRoster,
    id: ShellId,
}

impl ShellSink {
    pub fn id(&self) -> ShellId {
        self.id
    }

    /// Append output. Interleaved in arrival order across every stream that
    /// shares this sink — which is what a terminal shows anyway, and what
    /// makes an `ide_exec` mirror read like the console tab it mirrors.
    pub fn push(&self, bytes: &[u8]) {
        self.roster.push(self.id, bytes);
    }

    /// Record the ending. Idempotent-ish: a second call overwrites, which
    /// is the right answer when a kill and a natural exit race.
    pub fn finish(&self, state: ShellState) {
        self.roster.finish(self.id, state);
    }

    /// Take this entry out of the roster (ACP `terminal/release`, a job
    /// collected).
    pub fn remove(self) {
        self.roster.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EnvironmentId {
        EnvironmentId::parse("review").unwrap()
    }

    #[test]
    fn a_registered_shell_is_listed_for_its_environment_only() {
        let roster = ShellRoster::new();
        let other = EnvironmentId::primary();
        roster.register(env(), ShellKind::Agent, "cargo test", None);
        roster.register(other.clone(), ShellKind::User, "bash", None);

        assert_eq!(roster.list(None).len(), 2);
        let mine = roster.list(Some(&env()));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].command, "cargo test");
        assert_eq!(mine[0].kind, ShellKind::Agent);
        assert_eq!(mine[0].label(), "review · cargo test");
        assert_eq!(roster.list(Some(&other))[0].kind, ShellKind::User);
    }

    /// A tab opened mid-run must see what already happened AND what comes
    /// next, with nothing lost in the join.
    #[test]
    fn watching_replays_the_backlog_then_streams() {
        let roster = ShellRoster::new();
        let sink = roster.register(env(), ShellKind::Agent, "cargo build", None);
        sink.push(b"compiling\n");

        let (backlog, updates) = roster.watch(sink.id()).unwrap();
        assert_eq!(backlog, "compiling\n");

        sink.push(b"done\n");
        sink.finish(ShellState::Exited {
            code: Some(0),
            signal: None,
        });

        let ShellUpdate::Output(bytes) = updates.try_recv().unwrap() else {
            panic!("expected output first");
        };
        assert_eq!(bytes, b"done\n");
        let ShellUpdate::State(state) = updates.try_recv().unwrap() else {
            panic!("expected the ending second");
        };
        assert_eq!(
            state,
            ShellState::Exited {
                code: Some(0),
                signal: None
            }
        );
    }

    /// Opening a tab on a shell that already ended must not leave it
    /// claiming to be running.
    #[test]
    fn watching_a_finished_shell_replays_its_ending() {
        let roster = ShellRoster::new();
        let sink = roster.register(env(), ShellKind::ExecJob, "false", None);
        sink.push(b"nope\n");
        sink.finish(ShellState::Exited {
            code: Some(1),
            signal: None,
        });

        let (backlog, updates) = roster.watch(sink.id()).unwrap();
        assert_eq!(backlog, "nope\n");
        let ShellUpdate::State(state) = updates.try_recv().unwrap() else {
            panic!("expected the ending");
        };
        assert_eq!(state.summary(), "exited 1");
    }

    fn flag() -> (Arc<std::sync::atomic::AtomicBool>, Arc<dyn ShellControl>) {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control: Arc<dyn ShellControl> = Arc::new({
            let flag = flag.clone();
            move || flag.store(true, std::sync::atomic::Ordering::SeqCst)
        });
        (flag, control)
    }

    #[test]
    fn finishing_clears_killability_and_records_the_ending() {
        let roster = ShellRoster::new();
        let (_, control) = flag();
        let sink = roster.register(env(), ShellKind::Agent, "sleep 600", Some(control));
        assert!(roster.get(sink.id()).unwrap().killable);

        sink.finish(ShellState::Exited {
            code: None,
            signal: Some("SIGTERM".into()),
        });
        let entry = roster.get(sink.id()).unwrap();
        assert!(!entry.killable, "a dead shell cannot be killed again");
        assert_eq!(entry.state.summary(), "killed (SIGTERM)");
    }

    /// Kill reaches the spawner's own notion of stopping, and a kill on a
    /// shell that already went away is a no-op rather than a panic — the
    /// button always races the exit.
    #[test]
    fn kill_reaches_the_control_and_tolerates_a_race() {
        let roster = ShellRoster::new();
        let (fired, control) = flag();
        let sink = roster.register(env(), ShellKind::Agent, "sleep 600", Some(control));

        roster.kill(sink.id());
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));

        let id = sink.id();
        sink.remove();
        roster.kill(id); // gone: still a no-op
        assert!(roster.get(id).is_none());
    }

    /// Removal is not "close the tab": the record survives in whatever is
    /// already showing it. What removal does is end the stream.
    #[test]
    fn removal_closes_the_watchers_stream() {
        let roster = ShellRoster::new();
        let sink = roster.register(env(), ShellKind::ExecJob, "true", None);
        let (_, updates) = roster.watch(sink.id()).unwrap();
        sink.remove();
        assert!(updates.is_closed(), "the stream must end");
        assert!(updates.try_recv().is_err());
    }

    /// The roster is what the UI polls after an announcement, so the
    /// announcement has to name the environment that changed.
    #[test]
    fn changes_are_announced_on_the_bus_naming_the_environment() {
        let roster = ShellRoster::new();
        let bus = EventBus::new();
        let seen = bus.subscribe();
        roster.attach_events(bus);

        let sink = roster.register(env(), ShellKind::Agent, "ls", None);
        sink.finish(ShellState::Exited {
            code: Some(0),
            signal: None,
        });
        sink.remove();

        let mut envs = Vec::new();
        while let Ok(event) = seen.try_recv() {
            if let Event::ShellRosterChanged { env } = event {
                envs.push(env.to_string());
            }
        }
        assert_eq!(envs, ["review", "review", "review"]);
    }

    /// Bytes must not travel on the bus: a build's output broadcast to
    /// every subscriber in the process is work each of them does only to
    /// throw it away.
    #[test]
    fn output_does_not_ride_the_event_bus() {
        let roster = ShellRoster::new();
        let bus = EventBus::new();
        let seen = bus.subscribe();
        roster.attach_events(bus);
        let sink = roster.register(env(), ShellKind::Agent, "ls", None);
        while seen.try_recv().is_ok() {}

        sink.push(&vec![b'x'; 100_000]);
        assert!(
            seen.try_recv().is_err(),
            "output must reach watchers, never the broadcast bus"
        );
    }

    /// Ids never come back, so a console tab keyed by one can outlive the
    /// shell without ever being confused for a newer one.
    #[test]
    fn ids_are_monotonic_and_not_reused() {
        let roster = ShellRoster::new();
        let first = roster.register(env(), ShellKind::Agent, "a", None);
        let first_id = first.id();
        first.remove();
        let second = roster.register(env(), ShellKind::Agent, "b", None);
        assert!(second.id() > first_id);
    }
}
