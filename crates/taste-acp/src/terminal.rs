//! The client-served ACP terminal extension, in container mode only.
//!
//! # What the protocol actually models (v1, which is what we speak)
//!
//! `agent-client-protocol` 2.x carries two different terminal ideas and
//! only one of them is ours. The **v2 draft** (`schema::v2::terminal`, gated
//! behind the crate's `unstable_protocol_v2` feature and not enabled here)
//! has *agent-owned* terminals the agent reports for display via
//! `session/update`. **v1**, the version this IDE negotiates in
//! `InitializeRequest::new(ProtocolVersion::V1)`, has *client-served*
//! terminals: the agent sends `terminal/create`, `terminal/output`,
//! `terminal/wait_for_exit`, `terminal/kill` and `terminal/release` to the
//! CLIENT, and the client runs the process. That is what this module
//! implements, and it is why the IDE — not the agent — is the thing holding
//! a `podman exec`.
//!
//! The capability is one flag, `ClientCapabilities::terminal: bool`, sent
//! once in `initialize`.
//!
//! # Why it is served now, when ARCHITECTURE.md once refused
//!
//! The refusal ("no third route to a process") was argued for the
//! outside-confined topology, where the agent runs beside no files and a
//! client-served terminal would have been a *new* execution authority
//! reaching into the devcontainer. ENVIRONMENTS.md → "Trust model deltas"
//! changes the position for the one case that changed underneath it: after
//! relocation the agent process is already inside its environment's
//! container, where it has a shell (`ide_exec`) and the workspace is
//! writable. Serving terminals there trades no authority whatsoever — it
//! buys the user live visibility of commands they would otherwise only see
//! summarized in a transcript.
//!
//! So the gate is exactly relocation's gate, and this module is
//! unreachable without one: [`TerminalHost`] is constructed from the same
//! `AgentHosting` predicate, and a session that did not relocate does not
//! advertise the capability at all. **Safe mode stays unserved** — there is
//! no exec target there, the refusal holds precisely where it was argued,
//! and [`Terminals::create`] refuses on its own rather than trusting the
//! caller to have checked.
//!
//! # Advertisement is per connection, which is per session, which is honest
//!
//! ACP v1 has no per-session capability and no renegotiation: capabilities
//! are `initialize` parameters. That is not a limitation here, because a
//! chat's topology change *is* a respawn — ENVIRONMENTS.md's "the chat
//! never restarts; the process does", bridged by `session/load`. A session
//! that relocates comes back with `terminal: true`; one that drops to safe
//! mode comes back with it false. The advertisement therefore follows the
//! mode by construction rather than by a code path remembering to.
//!
//! What that leaves is the window between a container dying and the
//! respawn landing. [`Terminals::create`] refuses in it, naming safe mode —
//! per-request refusal as the backstop, never a host fallback.
//!
//! # Permission surface: deliberately none
//!
//! There is **no permission prompt per terminal**, and that is a decision,
//! not an omission. Creating a terminal in container mode is the exec
//! authority the agent already holds there: the same container, the same
//! workspace, the same shell it reaches through `ide_exec` without a prompt.
//! A dialog on every `cargo test` would be a prompt whose only possible
//! answer is yes, repeated until the user stops reading it — the
//! click-through training CLAUDE.md's consent gates exist to avoid spending
//! on things that matter (`devcontainer_reload`, force-publish). The ACP
//! permission flow for tool calls is untouched: an agent that asks
//! permission to run something still asks. What the user gets here instead
//! is *supervision* — the command is visible while it runs, and one click
//! stops it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, TerminalExitStatus, TerminalId, TerminalOutputResponse,
};
use anyhow::{Context, Result};
use taste_core::shells::{ShellControl, ShellKind, ShellRoster, ShellSink, ShellState};
use taste_core::{CappedOutput, EnvironmentId, ExecContext};

/// Everything serving terminals needs, and the fact that it exists is the
/// gate: `Some` means this session relocated into a container that can host
/// agent processes, `None` means it did not and the extension goes
/// unadvertised.
///
/// Built by the chat pane from the same values `taste_acp::Relocation` is
/// built from, so the two can never disagree about which world an agent is
/// in — see `ChatPane::terminal_host`.
#[derive(Clone)]
pub struct TerminalHost {
    /// The environment whose container the commands land in. Also the first
    /// half of every console tab label.
    pub environment: EnvironmentId,
    /// That environment's execution target. The same handle `ide_exec`
    /// resolves against, so a reload re-points both at once and neither
    /// holds a container id of its own.
    pub exec: ExecContext,
    /// The environment's checkout, at the host path that is also its
    /// container path. Used when the agent names no `cwd`.
    pub cwd: PathBuf,
    /// Where the tabs come from.
    pub roster: ShellRoster,
}

struct TerminalState {
    output: CappedOutput,
    exit: Option<TerminalExitStatus>,
}

struct Terminal {
    /// What the agent asked for, as a human reads it — the tab title and
    /// the roster row. Not the `podman exec --env …` the IDE built.
    command: String,
    state: Arc<Mutex<TerminalState>>,
    done: Arc<tokio::sync::Notify>,
    kill: Arc<tokio::sync::Notify>,
    shell: ShellSink,
}

/// One session's terminals. Created with the session, emptied with it.
#[derive(Clone)]
pub struct Terminals {
    host: TerminalHost,
    inner: Arc<Mutex<HashMap<TerminalId, Terminal>>>,
    next: Arc<AtomicU64>,
}

impl Terminals {
    pub fn new(host: TerminalHost) -> Self {
        Self {
            host,
            inner: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Serve `terminal/create`: spawn the command in this environment's
    /// container and start draining it into the roster.
    ///
    /// The composition is `ExecContext::resolve_for_agent_in` — the same
    /// `podman exec` route relocation takes (`relocate::relocated_agent_command`
    /// is the precedent) and the same one `ide_exec` takes, which is what
    /// makes "one environment of record" true rather than aspirational. The
    /// agent git policy rides along because that resolver applies it.
    pub fn create(&self, request: &CreateTerminalRequest) -> Result<TerminalId> {
        // Belt and braces, exactly as `taste_mcp::exec` does it: the caller
        // gates advertisement, and this refuses anyway. A container that
        // stopped between `initialize` and now must not become a reason to
        // run an agent's command on the user's host.
        if !self.host.exec.is_container() {
            anyhow::bail!(
                "environment {} has no devcontainer running, so there is nowhere to run \
                 this — and agent commands never fall back to the user's host. This is \
                 safe mode: author .devcontainer/, call devcontainer_reload, and \
                 terminals come back with it.",
                self.host.environment
            );
        }

        let args: Vec<&str> = request.args.iter().map(String::as_str).collect();
        let env: Vec<(String, String)> = request
            .env
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        let cwd = request
            .cwd
            .clone()
            .unwrap_or_else(|| self.host.cwd.clone())
            .display()
            .to_string();
        let spec = self
            .host
            .exec
            .resolve_for_agent_in(Some(&cwd), &env, &request.command, &args);

        let display = std::iter::once(request.command.clone())
            .chain(request.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");

        let mut child = tokio::process::Command::new(&spec.program)
            .args(&spec.args)
            // No stdin: an ACP terminal is a captured command, not a pty.
            // A process that reads stdin gets EOF rather than hanging on a
            // console nobody can type into.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The `podman exec` client dies with this process, and the
            // command in the container dies with the exec. Together with
            // `release_all` below that is what keeps a reload from leaving
            // execs behind.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", spec.program))?;

        let kill = Arc::new(tokio::sync::Notify::new());
        let control: Arc<dyn ShellControl> = Arc::new({
            let kill = kill.clone();
            // `notify_one`, not `notify_waiters`: the latter wakes only
            // waiters already parked, so a Kill landing before the wait
            // task reaches its `select!` would be dropped and the user's
            // button would silently do nothing. `notify_one` stores a
            // permit, and there is exactly one waiter to spend it.
            move || kill.notify_one()
        });
        let shell = self.host.roster.register(
            self.host.environment.clone(),
            ShellKind::Agent,
            display.clone(),
            Some(control),
        );

        // The agent's `outputByteLimit` is a total budget; without one the
        // exec tool's cap applies, because an agent that named no limit
        // still must not be able to make the IDE hold a gigabyte.
        let state = Arc::new(Mutex::new(TerminalState {
            output: match request.output_byte_limit {
                Some(limit) => CappedOutput::with_budget(limit.min(usize::MAX as u64) as usize),
                None => CappedOutput::default(),
            },
            exit: None,
        }));

        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;
        drain(stdout, state.clone(), shell.clone());
        drain(stderr, state.clone(), shell.clone());

        let done = Arc::new(tokio::sync::Notify::new());
        {
            let (state, done, kill, shell) =
                (state.clone(), done.clone(), kill.clone(), shell.clone());
            tokio::spawn(async move {
                let status = tokio::select! {
                    status = child.wait() => status,
                    _ = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };
                let (exit, shell_state) = exit_status(status);
                state.lock().unwrap().exit = Some(exit);
                shell.finish(shell_state);
                done.notify_waiters();
            });
        }

        // Ids are per session and opaque to the agent; a counter is
        // enough, and a readable one makes a transcript readable.
        let id = TerminalId::new(format!(
            "term_{}",
            self.next.fetch_add(1, Ordering::Relaxed) + 1
        ));
        self.inner.lock().unwrap().insert(
            id.clone(),
            Terminal {
                command: display,
                state,
                done,
                kill,
                shell,
            },
        );
        Ok(id)
    }

    /// Serve `terminal/output`: everything kept so far, whether it has
    /// exited or not.
    pub fn output(&self, id: &TerminalId) -> Result<TerminalOutputResponse> {
        let state = self.state_of(id)?;
        let state = state.lock().unwrap();
        let mut response =
            TerminalOutputResponse::new(state.output.render(), state.output.truncated());
        response.exit_status = state.exit.clone();
        Ok(response)
    }

    /// Serve `terminal/wait_for_exit`: park until the command ends.
    ///
    /// Interest is registered before the check, or a command that finishes
    /// in between leaves the agent waiting for a notification already sent
    /// — the same ordering `taste_mcp::exec::Jobs::wait` gets right.
    pub async fn wait_for_exit(&self, id: &TerminalId) -> Result<TerminalExitStatus> {
        let (state, done) = {
            let terminals = self.inner.lock().unwrap();
            let terminal = terminals.get(id).with_context(|| unknown(id))?;
            (terminal.state.clone(), terminal.done.clone())
        };
        loop {
            let notified = done.notified();
            if let Some(exit) = state.lock().unwrap().exit.clone() {
                return Ok(exit);
            }
            notified.await;
        }
    }

    /// Serve `terminal/kill`: stop the process, keep the terminal. The
    /// agent is expected to read the output afterwards, which is why this
    /// does not release.
    pub fn kill(&self, id: &TerminalId) -> Result<()> {
        let terminals = self.inner.lock().unwrap();
        let terminal = terminals.get(id).with_context(|| unknown(id))?;
        terminal.kill.notify_one();
        Ok(())
    }

    /// Serve `terminal/release`: kill if still running, and forget it.
    ///
    /// The console tab does NOT go with it — the output is the record of
    /// what happened and stays until the user closes it (ENVIRONMENTS.md's
    /// read-only supervision). What release ends is the live stream.
    pub fn release(&self, id: &TerminalId) -> Result<()> {
        let terminal = self
            .inner
            .lock()
            .unwrap()
            .remove(id)
            .with_context(|| unknown(id))?;
        terminal.kill.notify_one();
        terminal.shell.remove();
        Ok(())
    }

    /// Every terminal dies with its session.
    ///
    /// A relocated agent's terminals are `podman exec` clients the IDE
    /// owns; nothing else will reap them, and an agent that went away
    /// cannot release its own. Called when the connection ends and again
    /// when the client is dropped, because a respawn is both.
    pub fn release_all(&self) {
        let terminals: Vec<Terminal> = self.inner.lock().unwrap().drain().map(|(_, t)| t).collect();
        for terminal in terminals {
            terminal.kill.notify_one();
            terminal.shell.remove();
        }
    }

    /// The commands this session currently has open, for tests and for the
    /// roster's own sanity.
    pub fn commands(&self) -> Vec<String> {
        let mut open: Vec<String> = self
            .inner
            .lock()
            .unwrap()
            .values()
            .map(|t| t.command.clone())
            .collect();
        open.sort();
        open
    }

    fn state_of(&self, id: &TerminalId) -> Result<Arc<Mutex<TerminalState>>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(id)
            .with_context(|| unknown(id))?
            .state
            .clone())
    }
}

/// A released terminal id is spent, and an agent holding a stale one should
/// be able to tell that from having invented one.
fn unknown(id: &TerminalId) -> String {
    format!(
        "no such terminal: {id} — a terminal id is spent once terminal/release has \
         been called on it, and terminals do not survive their session."
    )
}

/// Both streams go to one sink, interleaved in arrival order: that is what
/// a terminal shows, and separating them here would give the console two
/// panes to reconcile for no gain.
fn drain<R>(reader: R, state: Arc<Mutex<TerminalState>>, shell: ShellSink)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = reader;
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    state.lock().unwrap().output.push(&buffer[..n]);
                    shell.push(&buffer[..n]);
                }
            }
        }
    });
}

/// Turn a process result into both vocabularies at once: ACP's exit status
/// for the agent, and the roster's for the console. A signal death has no
/// exit code in either, and inventing one would read as a result.
fn exit_status(
    status: std::io::Result<std::process::ExitStatus>,
) -> (TerminalExitStatus, ShellState) {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => (
                TerminalExitStatus::new().exit_code(u32::try_from(code).ok()),
                ShellState::Exited {
                    code: Some(code),
                    signal: None,
                },
            ),
            (None, Some(signal)) => {
                let name = signal_name(signal);
                (
                    TerminalExitStatus::new().signal(name.clone()),
                    ShellState::Exited {
                        code: None,
                        signal: Some(name),
                    },
                )
            }
            // Neither: the platform told us nothing. ACP models exactly
            // this — an exit status object with neither field set.
            (None, None) => (
                TerminalExitStatus::new(),
                ShellState::Exited {
                    code: None,
                    signal: None,
                },
            ),
        },
        Err(e) => {
            let message = format!("could not reap the process: {e}");
            tracing::warn!("{message}");
            (
                TerminalExitStatus::new(),
                ShellState::Exited {
                    code: None,
                    signal: None,
                },
            )
        }
    }
}

/// ACP asks for "the conventional platform signal name". These are the ones
/// a killed build actually dies of; anything else is reported by number,
/// which is still a name a reader can look up.
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        other => return format!("SIG{other}"),
    }
    .to_string()
}

/// The other direction: terminals the AGENT owns, mirrored into the roster.
///
/// # Why this exists at all
///
/// The pinned Claude Code adapter (`@agentclientprotocol/claude-agent-acp`
/// 0.69.0) **never sends `terminal/create`** — the string does not appear
/// in the package. It runs Bash inside its own process and, when the client
/// advertises `clientCapabilities._meta["terminal_output"]`, *reports* what
/// it ran: the tool call's content becomes
/// `ToolCallContent::Terminal { terminal_id }` (the id is the tool-use id),
/// and `_meta` carries `terminal_info`, then `terminal_output { data }` and
/// `terminal_exit { exit_code, signal }` when the command finishes. That is
/// the v2 draft's agent-owned model — `TerminalUpdate` /
/// `TerminalOutputChunk` — carried over `_meta` as a v1 extension, and it
/// is the only terminal shape this adapter speaks. The one client
/// capability it reads called "terminal" is `auth.terminal`, which is the
/// sign-in TUI and unrelated.
///
/// So [`Terminals`] above — correct ACP v1, and proven live — would sit
/// inert for the IDE's default agent. Both paths land in the same
/// [`ShellRoster`], so the console renders them identically and the user
/// never has to know which protocol direction produced a tab.
///
/// # What it honestly cannot do
///
/// **No Kill.** The process lives inside the adapter; there is no child of
/// ours to signal and no ACP request to ask for one. Rows registered here
/// are not killable, and the console renders the button insensitive rather
/// than offering a control that would do nothing. The user's lever is
/// cancelling the turn.
///
/// **Output arrives once, at the end.** The adapter emits the whole capture
/// with the tool result, not as it is produced, so these tabs fill in when
/// the command completes. A row appears at tool-call time, so a long
/// command is at least *visible* while it runs.
pub struct AgentOwnedTerminals {
    host: TerminalHost,
    rows: Mutex<HashMap<String, ShellSink>>,
}

/// The `_meta` key the adapter gates its terminal reporting on. Advertised
/// only where [`Terminals`] is served, so one gate still decides.
pub const TERMINAL_OUTPUT_META: &str = "terminal_output";

impl AgentOwnedTerminals {
    pub fn new(host: TerminalHost) -> Self {
        Self {
            host,
            rows: Mutex::new(HashMap::new()),
        }
    }

    /// Fold one session update into the roster. Called for every update
    /// before it reaches the UI; anything that is not a terminal-bearing
    /// tool call is ignored.
    pub fn observe(&self, update: &agent_client_protocol::schema::v1::SessionUpdate) {
        use agent_client_protocol::schema::v1::SessionUpdate;
        let (terminals, title, meta) = match update {
            SessionUpdate::ToolCall(call) => (
                terminal_ids(&call.content),
                Some(call.title.clone()),
                call.meta.as_ref(),
            ),
            SessionUpdate::ToolCallUpdate(update) => (
                update
                    .fields
                    .content
                    .as_deref()
                    .map(terminal_ids)
                    .unwrap_or_default(),
                update.fields.title.clone(),
                update.meta.as_ref(),
            ),
            _ => return,
        };
        // A row per terminal the call announces. The title is the command:
        // the adapter sets it to `input.command` for Bash.
        for id in terminals {
            self.ensure_row(&id, title.as_deref());
        }
        let Some(meta) = meta else { return };
        // `terminal_info` can name a terminal the content did not, which is
        // the adapter's own "the terminal exists" signal.
        if let Some(id) = meta_terminal_id(meta.get("terminal_info")) {
            self.ensure_row(&id, title.as_deref());
        }
        if let Some(output) = meta.get("terminal_output") {
            if let (Some(id), Some(data)) = (
                meta_terminal_id(Some(output)),
                output.get("data").and_then(|d| d.as_str()),
            ) {
                self.ensure_row(&id, title.as_deref());
                if let Some(row) = self.rows.lock().unwrap().get(&id) {
                    row.push(data.as_bytes());
                }
            }
        }
        if let Some(exit) = meta.get("terminal_exit") {
            if let Some(id) = meta_terminal_id(Some(exit)) {
                self.ensure_row(&id, title.as_deref());
                let state = ShellState::Exited {
                    code: exit
                        .get("exit_code")
                        .and_then(|c| c.as_i64())
                        .map(|c| c as i32),
                    signal: exit
                        .get("signal")
                        .and_then(|s| s.as_str())
                        .map(str::to_string),
                };
                if let Some(row) = self.rows.lock().unwrap().get(&id) {
                    row.finish(state);
                }
            }
        }
    }

    /// Every row this session opened goes when the session does, exactly as
    /// a client-served terminal does.
    pub fn release_all(&self) {
        for (_, row) in self.rows.lock().unwrap().drain() {
            row.remove();
        }
    }

    fn ensure_row(&self, id: &str, title: Option<&str>) {
        let mut rows = self.rows.lock().unwrap();
        if rows.contains_key(id) {
            return;
        }
        rows.insert(
            id.to_string(),
            self.host.roster.register(
                self.host.environment.clone(),
                ShellKind::Agent,
                // No title yet means the update that named this terminal
                // carried none; the id is a poor label but an honest one.
                title.unwrap_or(id),
                // Not killable: the process is inside the adapter. See the
                // type docs — offering a button that cannot work is worse
                // than not offering one.
                None,
            ),
        );
    }
}

fn terminal_ids(content: &[agent_client_protocol::schema::v1::ToolCallContent]) -> Vec<String> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Terminal(terminal) => Some(terminal.terminal_id.to_string()),
            _ => None,
        })
        .collect()
}

fn meta_terminal_id(value: Option<&serde_json::Value>) -> Option<String> {
    value?
        .get("terminal_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(roster: &ShellRoster) -> TerminalHost {
        TerminalHost {
            environment: EnvironmentId::parse("review").unwrap(),
            // `for_tests(true)` is the self-hosting shape: container mode
            // with no podman to wrap, so the resolved command is the real
            // one and these tests run anywhere.
            exec: ExecContext::for_tests(true),
            cwd: std::env::temp_dir(),
            roster: roster.clone(),
        }
    }

    fn request(command: &str, args: &[&str]) -> CreateTerminalRequest {
        CreateTerminalRequest::new("sess", command)
            .args(args.iter().map(|s| s.to_string()).collect())
    }

    /// The whole lifecycle the adapter drives: create, read, wait, release.
    #[tokio::test]
    async fn create_output_exit_and_release() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        let id = terminals
            .create(&request("sh", &["-c", "echo hello; echo oops >&2"]))
            .unwrap();

        // The command shows up as a killable agent shell in ITS environment.
        let listed = roster.list(Some(&EnvironmentId::parse("review").unwrap()));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, ShellKind::Agent);
        assert!(listed[0].killable);
        assert_eq!(
            listed[0].label(),
            "review · sh -c echo hello; echo oops >&2"
        );

        let exit = terminals.wait_for_exit(&id).await.unwrap();
        assert_eq!(exit.exit_code, Some(0));

        let output = terminals.output(&id).unwrap();
        assert!(output.output.contains("hello"), "{}", output.output);
        // stderr is interleaved, not dropped: a terminal shows both.
        assert!(output.output.contains("oops"), "{}", output.output);
        assert!(!output.truncated);
        assert_eq!(output.exit_status.unwrap().exit_code, Some(0));

        // The roster agrees, and the row is no longer killable.
        let entry = roster.get(listed[0].id).unwrap();
        assert_eq!(entry.state.summary(), "exited 0");
        assert!(!entry.killable);

        terminals.release(&id).unwrap();
        assert!(roster.list(None).is_empty(), "release drops the row");
        assert!(terminals.output(&id).is_err());
        assert!(
            terminals
                .output(&id)
                .unwrap_err()
                .to_string()
                .contains("spent"),
            "a stale id must be distinguishable from an invented one"
        );
    }

    /// Kill stops the process and keeps the terminal, because the agent
    /// still has to read what it produced.
    #[tokio::test]
    async fn kill_stops_the_process_but_keeps_the_terminal() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        let id = terminals
            .create(&request("sh", &["-c", "echo started; sleep 600"]))
            .unwrap();

        // Kill through the ROSTER — the user's Kill button, not the agent's
        // request — which is the supervision path this batch exists for.
        let shell = roster.list(None)[0].id;
        // Let the child get as far as exec'ing before pulling the rug.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        roster.kill(shell);

        let exit = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            terminals.wait_for_exit(&id),
        )
        .await
        .expect("a killed terminal must exit")
        .unwrap();
        assert!(
            exit.signal.is_some() || exit.exit_code.is_some(),
            "the ending must be reported one way or the other"
        );

        let output = terminals.output(&id).unwrap();
        assert!(output.output.contains("started"), "{}", output.output);
        assert!(
            output.exit_status.is_some(),
            "still readable after the kill"
        );
        assert!(!roster.list(None).is_empty(), "and still listed");
    }

    /// Safe mode: no exec target, so no terminal — and the refusal says so
    /// rather than quietly running the command somewhere else.
    #[tokio::test]
    async fn safe_mode_refuses_to_create_a_terminal() {
        let roster = ShellRoster::new();
        let mut host = host(&roster);
        host.exec = ExecContext::for_tests(false);
        let terminals = Terminals::new(host);
        let error = terminals
            .create(&request("echo", &["hi"]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("never fall back to the user's host"),
            "{error}"
        );
        assert!(error.contains("safe mode"), "{error}");
        assert!(roster.list(None).is_empty(), "nothing was spawned");
    }

    /// Terminals die with their session. Nothing else reaps a `podman
    /// exec` the IDE started, so a respawn that left them would leak one
    /// per command for the life of the container.
    #[tokio::test]
    async fn every_terminal_dies_with_its_session() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        let first = terminals.create(&request("sleep", &["600"])).unwrap();
        terminals.create(&request("sleep", &["600"])).unwrap();
        assert_eq!(roster.list(None).len(), 2);

        terminals.release_all();
        assert!(roster.list(None).is_empty());
        assert!(terminals.commands().is_empty());
        assert!(terminals.output(&first).is_err());
    }

    /// The agent's byte limit is respected, and what comes back says what
    /// it lost instead of pretending to be the whole log.
    #[tokio::test]
    async fn a_byte_limit_truncates_honestly() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        let mut request = request(
            "sh",
            &[
                "-c",
                "echo FIRST; for i in $(seq 1 3000); do echo padding-line-$i; done; echo LAST",
            ],
        );
        request.output_byte_limit = Some(2048);
        let id = terminals.create(&request).unwrap();
        terminals.wait_for_exit(&id).await.unwrap();

        let output = terminals.output(&id).unwrap();
        assert!(
            output.truncated,
            "the limit was exceeded and must be reported"
        );
        assert!(output.output.starts_with("FIRST"), "{}", output.output);
        assert!(
            output.output.trim_end().ends_with("LAST"),
            "{}",
            output.output
        );
        assert!(output.output.contains("bytes elided"), "{}", output.output);
    }

    /// A terminal whose container goes away is reaped, not left "running"
    /// forever. Standing in for the container here is the exec client
    /// itself: killing that is exactly what a container death does to it.
    #[tokio::test]
    async fn a_terminal_whose_process_dies_is_reaped() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        // `sh -c 'kill -TERM $$'` ends the way a container teardown ends an
        // exec: a signal, no exit code.
        let id = terminals
            .create(&request("sh", &["-c", "kill -TERM $$"]))
            .unwrap();
        let exit = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            terminals.wait_for_exit(&id),
        )
        .await
        .expect("a dead process must be reaped")
        .unwrap();
        assert_eq!(exit.signal.as_deref(), Some("SIGTERM"));
        assert_eq!(exit.exit_code, None, "a signal death has no exit code");

        let entry = roster.get(roster.list(None)[0].id).unwrap();
        assert_eq!(entry.state.summary(), "killed (SIGTERM)");
    }

    /// The agent's working directory and variables reach the process, so a
    /// terminal is not quietly a different shell from `ide_exec`.
    #[tokio::test]
    async fn the_requested_cwd_and_env_reach_the_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "x").unwrap();
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));

        let mut request = request("sh", &["-c", "ls marker; echo $TASTE_PROBE"]);
        request.cwd = Some(dir.path().to_path_buf());
        request.env = vec![agent_client_protocol::schema::v1::EnvVariable::new(
            "TASTE_PROBE",
            "seen",
        )];
        let id = terminals.create(&request).unwrap();
        terminals.wait_for_exit(&id).await.unwrap();
        let output = terminals.output(&id).unwrap().output;
        assert!(output.contains("marker"), "{output}");
        assert!(output.contains("seen"), "{output}");
    }

    /// The pinned Claude Code adapter's shape, replayed exactly as it
    /// emits it (`toolInfoFromToolUse` → `toolUpdateFromToolResult` in
    /// `@agentclientprotocol/claude-agent-acp` 0.69.0): a Bash tool call
    /// whose content is a terminal reference and whose title is the
    /// command, then a result update carrying the output and exit in
    /// `_meta`. Getting a console tab out of that is the only way the
    /// default agent's commands are visible at all.
    #[test]
    fn the_adapters_own_terminals_become_roster_rows() {
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCall, ToolCallContent, ToolCallUpdate, ToolCallUpdateFields,
            ToolKind,
        };

        let roster = ShellRoster::new();
        let observed = AgentOwnedTerminals::new(host(&roster));

        // 1. Tool call: the terminal is announced under the tool-use id,
        //    and the title is the command the user should read.
        let call = ToolCall::new("toolu_01", "cargo test --workspace")
            .kind(ToolKind::Execute)
            .content(vec![ToolCallContent::Terminal(
                agent_client_protocol::schema::v1::Terminal::new("toolu_01"),
            )]);
        observed.observe(&SessionUpdate::ToolCall(call));

        let listed = roster.list(None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label(), "review · cargo test --workspace");
        assert!(listed[0].state.is_running(), "it appears while it runs");
        assert!(
            !listed[0].killable,
            "the process is inside the adapter; a Kill button would be a lie"
        );

        // 2. Result: output and exit arrive together in `_meta`.
        let mut meta = agent_client_protocol::schema::v1::Meta::new();
        meta.insert(
            "terminal_output".into(),
            serde_json::json!({"terminal_id": "toolu_01", "data": "test result: ok. 315 passed\n"}),
        );
        meta.insert(
            "terminal_exit".into(),
            serde_json::json!({"terminal_id": "toolu_01", "exit_code": 0, "signal": null}),
        );
        let (backlog, updates) = roster.watch(listed[0].id).unwrap();
        assert_eq!(backlog, "", "nothing was reported until the result");
        observed.observe(&SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new("toolu_01", ToolCallUpdateFields::new()).meta(meta),
        ));

        let entry = roster.get(listed[0].id).unwrap();
        assert_eq!(entry.state.summary(), "exited 0");
        let mut seen = String::new();
        while let Ok(update) = updates.try_recv() {
            if let taste_core::ShellUpdate::Output(bytes) = update {
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        assert!(seen.contains("315 passed"), "{seen}");

        // 3. One row per terminal, however many updates mention it.
        assert_eq!(roster.list(None).len(), 1);

        // 4. ...and it goes with the session.
        observed.release_all();
        assert!(roster.list(None).is_empty());
    }

    /// A signal death reported by the adapter reads as one, and updates
    /// that mention no terminal are ignored rather than inventing rows.
    #[test]
    fn only_terminal_bearing_updates_make_rows() {
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCall, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
        };

        let roster = ShellRoster::new();
        let observed = AgentOwnedTerminals::new(host(&roster));

        // A Read tool call: no terminal content, no `_meta` — no row.
        observed.observe(&SessionUpdate::ToolCall(
            ToolCall::new("toolu_read", "Read src/main.rs").kind(ToolKind::Read),
        ));
        assert!(roster.list(None).is_empty());

        let mut meta = agent_client_protocol::schema::v1::Meta::new();
        meta.insert(
            "terminal_info".into(),
            serde_json::json!({"terminal_id": "toolu_02"}),
        );
        meta.insert(
            "terminal_exit".into(),
            serde_json::json!({"terminal_id": "toolu_02", "exit_code": null, "signal": "SIGKILL"}),
        );
        observed.observe(&SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(
                "toolu_02",
                ToolCallUpdateFields::new().title("sleep 600".to_string()),
            )
            .meta(meta),
        ));

        let listed = roster.list(None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].command, "sleep 600");
        assert_eq!(listed[0].state.summary(), "killed (SIGKILL)");

        // An empty terminal id is no id at all — the adapter says so, and
        // a row labelled "" would be worse than none.
        let mut empty = agent_client_protocol::schema::v1::Meta::new();
        empty.insert(
            "terminal_info".into(),
            serde_json::json!({"terminal_id": ""}),
        );
        observed.observe(&SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new("toolu_03", ToolCallUpdateFields::new()).meta(empty),
        ));
        assert_eq!(roster.list(None).len(), 1);
    }

    /// A watcher sees terminal output live, which is what makes the console
    /// tab a live tab rather than a post-mortem.
    #[tokio::test]
    async fn output_streams_to_a_roster_watcher_while_it_runs() {
        let roster = ShellRoster::new();
        let terminals = Terminals::new(host(&roster));
        let id = terminals
            .create(&request("sh", &["-c", "echo one; sleep 0.2; echo two"]))
            .unwrap();
        let (_, updates) = roster.watch(roster.list(None)[0].id).unwrap();

        let mut seen = String::new();
        let mut ended = false;
        while let Ok(Ok(update)) =
            tokio::time::timeout(std::time::Duration::from_secs(10), updates.recv()).await
        {
            match update {
                taste_core::ShellUpdate::Output(bytes) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes))
                }
                taste_core::ShellUpdate::State(_) => {
                    ended = true;
                    break;
                }
            }
        }
        assert!(ended, "the watcher must be told when it ended");
        assert!(seen.contains("two"), "{seen}");
        terminals.release(&id).unwrap();
    }
}
