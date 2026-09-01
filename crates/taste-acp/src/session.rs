//! One live agent connection and its session.
//!
//! `AgentClient::spawn` starts the agent subprocess (confined; see
//! `sandbox`), drives the ACP connection on the tokio runtime, and exposes
//! two channels: commands in, [`SessionEvent`]s out. The GTK chat pane holds
//! only the channel ends, so nothing UI-side blocks the protocol and nothing
//! protocol-side touches GTK.
//!
//! Session setup surfaces the agent's own knobs to the UI instead of hiding
//! them: authentication methods (from `initialize`), session modes (the
//! agent's permission approach — e.g. Claude Code's default/acceptEdits),
//! and config options (model selection travels here).

use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    AuthMethod, AuthMethodId, AuthenticateRequest, CancelNotification, ContentBlock,
    CreateTerminalRequest, CreateTerminalResponse, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest,
    PermissionOption, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOption, SessionConfigValueId,
    SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TerminalOutputRequest,
    TextContent, Usage, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, ConnectionTo};
use anyhow::Result;

use crate::AgentSpec;

/// The user's answer to an agent permission request.
pub type PermissionReply = tokio::sync::oneshot::Sender<RequestPermissionOutcome>;

/// Instructions from the UI to the session task.
pub enum Command {
    /// A full prompt: text plus any attached context blocks (files,
    /// selections, images).
    Prompt(Vec<ContentBlock>),
    /// Cancel the in-flight turn.
    Cancel,
    Authenticate(AuthMethodId),
    SetMode(SessionModeId),
    SetConfigOption(SessionConfigId, SessionConfigValueId),
    SetConfigBool(SessionConfigId, bool),
}

/// What the chat pane renders.
pub enum SessionEvent {
    /// The agent requires sign-in before a session can start; pick a method
    /// and send [`Command::Authenticate`].
    AuthRequired { methods: Vec<AuthMethod> },
    /// A prompt was rejected (auth expiry, transient agent error). The
    /// connection stays up; when it looks auth-related an AuthRequired
    /// precedes this.
    PromptFailed { message: String },
    /// The session is live. Carries the session id (persist it to restore
    /// the conversation later via `session/load`) and the agent's control
    /// surface: permission modes and config options (model choice included).
    Ready {
        session_id: String,
        /// True when this is a restored conversation (history replays as
        /// ordinary updates before this event's turn).
        restored: bool,
        /// True when a restore was attempted (or wanted) and this fresh
        /// session is the fallback — the UI should say the old
        /// conversation didn't come back, not silently present a blank.
        restore_failed: bool,
        modes: Option<SessionModeState>,
        config_options: Vec<SessionConfigOption>,
    },
    /// Streamed session update: message/thought chunks, tool calls, plans,
    /// mode/config change notifications.
    Update(SessionUpdate),
    /// The agent asked for permission; answer through `reply`.
    Permission {
        request: RequestPermissionRequest,
        reply: PermissionReply,
    },
    /// A turn finished, with the agent's session-cumulative token usage
    /// when it reports one.
    TurnEnded {
        reason: StopReason,
        usage: Option<Usage>,
    },
    /// A mode change was rejected (e.g. an advertised mode that this
    /// session cannot actually enter). Non-fatal.
    ModeChangeFailed {
        mode: SessionModeId,
        message: String,
    },
    /// A config/auth command failed. Non-fatal.
    CommandFailed { message: String },
    /// The connection ended (process exit or protocol error).
    Closed(Option<String>),
}

pub struct AgentClient {
    pub spec: AgentSpec,
    commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<SessionEvent>,
    /// This session's client-served terminals, when the extension is served
    /// at all (container mode only — see [`crate::terminal`]).
    terminals: Option<crate::terminal::Terminals>,
}

/// A session's terminals do not outlive it.
///
/// Each one is a `podman exec` the IDE started and nothing else will reap:
/// an agent that died cannot release its own, and a respawn (relocation, a
/// container rebuild, the user switching agents) drops this client. Both
/// ends are covered — the connection task releases when it returns, and
/// this releases when the handle goes — because a respawn is both, in
/// either order.
impl Drop for AgentClient {
    fn drop(&mut self) {
        if let Some(terminals) = &self.terminals {
            terminals.release_all();
        }
    }
}

/// Whose home this agent gets, and under whose name it spends.
///
/// The two travel together because they are the same fact from two sides:
/// the environment id is what the auth proxy attributes a request to, and
/// the volume is where that environment's agent keeps its history. Passing
/// one without the other is how a spawn ends up spending as `primary` out
/// of a clone's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHome {
    /// The environment id, as the auth proxy's placeholder records it.
    pub environment: String,
    /// The podman volume mounted at `policy::AGENT_HOME_IN_DEVCONTAINER`.
    pub volume: String,
}

impl AgentClient {
    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("agent connection is closed"))
    }

    /// Queue a plain-text prompt for the agent's session.
    pub fn prompt(&self, text: impl Into<String>) -> Result<()> {
        self.prompt_blocks(vec![ContentBlock::Text(TextContent::new(text.into()))])
    }

    /// Queue a prompt with attached context blocks.
    pub fn prompt_blocks(&self, blocks: Vec<ContentBlock>) -> Result<()> {
        self.send(Command::Prompt(blocks))
    }

    /// Cancel the in-flight turn (no-op when idle).
    pub fn cancel(&self) -> Result<()> {
        self.send(Command::Cancel)
    }

    /// Authenticate with one of the methods from [`SessionEvent::AuthRequired`].
    pub fn authenticate(&self, method: AuthMethodId) -> Result<()> {
        self.send(Command::Authenticate(method))
    }

    /// Switch the agent's session mode (its permission approach).
    pub fn set_mode(&self, mode: SessionModeId) -> Result<()> {
        self.send(Command::SetMode(mode))
    }

    /// Set a session config option (model selection travels here).
    pub fn set_config_option(
        &self,
        config: SessionConfigId,
        value: SessionConfigValueId,
    ) -> Result<()> {
        self.send(Command::SetConfigOption(config, value))
    }

    /// Set a boolean session config option.
    pub fn set_config_bool(&self, config: SessionConfigId, value: bool) -> Result<()> {
        self.send(Command::SetConfigBool(config, value))
    }

    /// Spawn an agent aimed at one environment.
    ///
    /// The IDE's entry point: everything a binding decides — the checkout
    /// the agent works in, the MCP socket that tells the IDE which
    /// environment it is, and the mode it starts in — arrives together in
    /// one [`AgentAim`], so no caller can pair one environment's socket
    /// with another's working directory.
    ///
    /// The confinement is NOT part of the aim: `relocation` is. Pass
    /// `Some` when this chat's environment has a container up that can host
    /// the agent, and the process runs inside it, beside the files
    /// ([`crate::relocate`]); pass `None` and it runs outside-confined
    /// against the stand-in workspace. Every value in the aim is the same
    /// either way, which is what lets `session/load` carry a conversation
    /// across the move.
    /// `terminals` is the ACP terminal extension, served or not. It is
    /// deliberately a sibling of `relocation` rather than part of the aim:
    /// the aim is identical in both topologies, and terminals exist in
    /// exactly one of them. Callers build it from the same gate relocation
    /// passed — see `ChatPane::terminal_host` and [`crate::terminal`].
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_aimed(
        spec: AgentSpec,
        aim: crate::AgentAim,
        relocation: Option<crate::Relocation>,
        terminals: Option<crate::TerminalHost>,
        resume_session: Option<String>,
        ui_probe: Option<taste_core::ui_probe::UiProbe>,
    ) -> Result<Self> {
        Self::spawn(
            spec,
            aim.cwd,
            Some(aim.mcp_bridge),
            Some(aim.mcp_socket),
            AgentHome {
                environment: aim.environment.to_string(),
                volume: aim.home_volume,
            },
            relocation,
            terminals,
            aim.safe_mode,
            resume_session,
            ui_probe,
        )
    }

    /// Spawn the agent subprocess and run its session on the tokio runtime.
    ///
    /// `cwd` is the checkout of the environment this agent is aimed at —
    /// the main one for the primary, that environment's clone otherwise. It
    /// bounds the agent's file reads and writes and keys its stand-in
    /// workspace, so it is the environment as far as everything below is
    /// concerned. `mcp_bridge` is the stdio-bridge command registered so the
    /// agent can reach the IDE's MCP server, over the socket that tells the
    /// IDE which environment is calling. `safe_mode` is that environment's
    /// mode. Prefer [`Self::spawn_aimed`], which computes the three of them
    /// from one binding.
    /// `ui_probe` is the editor's live-buffer lookup: when present, the
    /// client declares `fs.readTextFile` and serves agent reads from open
    /// buffers — the agent sees the user's unsaved truth, not stale disk.
    ///
    /// The agent always runs confined (see [`crate::sandbox`]); if
    /// bubblewrap is unavailable this returns an error instead of launching
    /// unconfined.
    ///
    /// `relocation` is the one thing that selects the topology: `Some`
    /// means the environment's container is up and hosting the agent, and
    /// the cascade below is skipped for a `podman exec` into it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        spec: AgentSpec,
        cwd: PathBuf,
        mcp_bridge: Option<(String, Vec<String>)>,
        mcp_socket: Option<PathBuf>,
        home: AgentHome,
        relocation: Option<crate::Relocation>,
        terminals: Option<crate::TerminalHost>,
        safe_mode: bool,
        resume_session: Option<String>,
        ui_probe: Option<taste_core::ui_probe::UiProbe>,
    ) -> Result<Self> {
        let git_policy = crate::sandbox::ensure_git_policy_file()?;
        let (url_script, url_dir) = crate::sandbox::ensure_url_bridge()?;

        // One seam for every confinement: `spec.env` is what each path
        // below turns into process environment. Empty unless the auth
        // proxy is switched on (see `crate::authproxy`). The environment id
        // is what the placeholder is minted against, which is what makes
        // per-environment spend and per-environment revocation mean
        // anything.
        let proxy_env = crate::authproxy::spawn_env(&spec, &home.environment);
        let mut spec = spec;
        spec.env.extend(proxy_env);

        // Relocation, first because it is the preferred topology whenever
        // it is available: the agent runs inside its environment's own
        // container, where the workspace is real and its native file tools
        // work. Nothing about the ADDRESS changes — same cwd, same MCP
        // socket, same home volume — so the conversation crosses over on
        // `session/load` (see `crate::relocate`).
        //
        // There is no stand-in workspace here and no `write_allowed` wall
        // around the agent's own shell: in container mode there never was
        // one (`ide_exec` is a shell with the workspace writable), and
        // pretending otherwise is what CLAUDE.md refuses to defend.
        if let Some(relocation) = &relocation {
            let sandboxed = std::path::Path::new("/.flatpak-info").exists();
            let (program, args) =
                crate::relocate::relocated_agent_command(&spec, &cwd, relocation, sandboxed);
            // The IDE binary's path means nothing inside a container — and
            // neither does the aim's socket path, which is a HOST socket the
            // unconfined IDE bound and a confined container may not dial.
            // What the agent dials is its environment channel's in-container
            // endpoint, which is why that address rides on the relocation
            // and not on the aim.
            let bridge = crate::sandbox::mcp_bridge_command(&relocation.mcp_socket);
            return Ok(Self::spawn_with_command(
                spec,
                cwd,
                Some(bridge),
                resume_session,
                ui_probe,
                terminals,
                safe_mode,
                program,
                args,
            ));
        }

        let workspace_stub = crate::sandbox::ensure_workspace_stub(&cwd)?;

        // Self-hosting bootstrap: the IDE itself runs inside its own
        // devcontainer. bwrap cannot nest there — and the container already
        // provides the confinement contract (the user's real home is not
        // mounted). Spawn directly, keeping the env-level protections.
        if crate::sandbox::inside_container() {
            spec.env
                .push(("GIT_CONFIG_GLOBAL".into(), git_policy.display().to_string()));
            spec.env
                .push(("BROWSER".into(), url_script.display().to_string()));
            // The environment announces itself (see container_agent_command).
            spec.env
                .push(("TASTE_IDE_VERSION".into(), env!("CARGO_PKG_VERSION").into()));
            spec.env
                .push(("TASTE_IDE_CONFINEMENT".into(), "direct".into()));
            let program = spec.command.clone();
            let args = spec.args.clone();
            return Ok(Self::spawn_with_command(
                spec,
                cwd,
                mcp_bridge,
                resume_session,
                ui_probe,
                terminals,
                safe_mode,
                program,
                args,
            ));
        }

        // Preferred confinement everywhere else: the agent runs inside
        // the devcontainer image via host podman. Neither the packaged
        // Flatpak's host nor a bare Silverblue has node/npx — the image
        // does, and a container out-isolates bwrap.
        if let Some((program, args)) = crate::sandbox::container_agent_command(
            &spec,
            &cwd,
            &git_policy,
            &workspace_stub,
            &home.volume,
            mcp_socket.as_deref(),
            (&url_script, &url_dir),
            false,
        ) {
            // The IDE binary's path means nothing inside a container;
            // see `sandbox::mcp_bridge_command`.
            let bridge = mcp_socket
                .as_deref()
                .map(crate::sandbox::mcp_bridge_command);
            return Ok(Self::spawn_with_command(
                spec,
                cwd,
                bridge.or(mcp_bridge),
                resume_session,
                ui_probe,
                terminals,
                safe_mode,
                program,
                args,
            ));
        }

        // Inside the Flatpak-packaged IDE the sandbox PATH has no bwrap;
        // the host's is used via flatpak-spawn below.
        let flatpak = std::path::Path::new("/.flatpak-info").exists();
        if !flatpak && !crate::sandbox::bwrap_available() {
            anyhow::bail!(
                "bubblewrap (bwrap) not found: agents only run confined, never unconfined"
            );
        }
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        // Sign-in persistence: the sandbox home is tmpfs, so the agent's
        // auth/cache dirs must exist to be bound back in. Create the
        // directory-shaped ones (never files — an empty ~/.claude.json
        // would be corrupt, and auth lives in the dirs).
        for rel in &spec.home_paths {
            if !rel.ends_with(".json") {
                let _ = std::fs::create_dir_all(home.join(rel));
            }
        }
        let (mut program, mut args) = crate::sandbox::wrap(
            &spec,
            &cwd,
            &workspace_stub,
            &home,
            &git_policy,
            mcp_socket.as_deref(),
            Some((&url_script, &url_dir)),
        );
        // Inside the Flatpak-packaged IDE, bwrap lives on the host.
        if flatpak {
            args.insert(0, program);
            args.insert(0, "--host".into());
            program = "flatpak-spawn".into();
        }
        Ok(Self::spawn_with_command(
            spec,
            cwd,
            mcp_bridge,
            resume_session,
            ui_probe,
            terminals,
            safe_mode,
            program,
            args,
        ))
    }

    /// Test seam: run the agent command directly, no sandbox. Never used by
    /// the application — integration tests run inside containers where
    /// bubblewrap cannot nest.
    #[doc(hidden)]
    pub fn spawn_unconfined_for_tests(spec: AgentSpec, cwd: PathBuf) -> Self {
        let program = spec.command.clone();
        let args = spec.args.clone();
        Self::spawn_with_command(spec, cwd, None, None, None, None, false, program, args)
    }

    /// Test seam that asks for a restore: drives the `session/load` path
    /// the IDE takes after any restart that ended the agent process — an
    /// IDE relaunch today, a devcontainer rebuild if agents ever move in
    /// there. The conversation outlives the process because the session id
    /// is persisted (`taste_core::state`) and the agent keeps its history
    /// under its own home, not in the container.
    #[doc(hidden)]
    pub fn spawn_unconfined_resuming_for_tests(
        spec: AgentSpec,
        cwd: PathBuf,
        resume_session: String,
    ) -> Self {
        let program = spec.command.clone();
        let args = spec.args.clone();
        Self::spawn_with_command(
            spec,
            cwd,
            None,
            Some(resume_session),
            None,
            None,
            false,
            program,
            args,
        )
    }

    /// Test seam with the editor attached: declares `fs.readTextFile`, so
    /// the agent's file reads come back through [`read_text_file`].
    #[doc(hidden)]
    pub fn spawn_unconfined_with_ui_for_tests(
        spec: AgentSpec,
        cwd: PathBuf,
        ui_probe: taste_core::ui_probe::UiProbe,
    ) -> Self {
        let program = spec.command.clone();
        let args = spec.args.clone();
        Self::spawn_with_command(
            spec,
            cwd,
            None,
            None,
            Some(ui_probe),
            None,
            false,
            program,
            args,
        )
    }

    /// The three confinements below converge here, each having composed its
    /// own program and args; the rest is the session itself. Grouping the
    /// tail into a struct would put the confinements' shared work behind a
    /// second type that means nothing to anyone else.
    #[allow(clippy::too_many_arguments)]
    fn spawn_with_command(
        spec: AgentSpec,
        cwd: PathBuf,
        mcp_bridge: Option<(String, Vec<String>)>,
        resume_session: Option<String>,
        ui_probe: Option<taste_core::ui_probe::UiProbe>,
        terminal_host: Option<crate::TerminalHost>,
        safe_mode: bool,
        program: String,
        args: Vec<String>,
    ) -> Self {
        let (command_tx, command_rx) = async_channel::unbounded::<Command>();
        let (event_tx, event_rx) = async_channel::unbounded::<SessionEvent>();

        // Preflight: a missing launcher must fail with its NAME, not a bare
        // ENOENT — "os error 2" cost a debugging session once.
        if resolve_program(&program).is_none() {
            let _ = event_tx.try_send(SessionEvent::Closed(Some(format!(
                "agent launcher '{program}' is not installed in this environment                  — self-hosting runs start via ./bootstrap.sh (inside the                  devcontainer, which provides it)"
            ))));
        }

        let mut config = AcpAgentConfig::new(&program);
        for arg in &args {
            config = config.arg(arg);
        }
        for (k, v) in &spec.env {
            config = config.env(k, v);
        }
        let agent = AcpAgent::new(config);

        let events_for_notify = event_tx.clone();
        let events_for_perm = event_tx.clone();
        let events_for_close = event_tx.clone();
        // The IDE mediates the agent's filesystem, always. Reads were
        // once gated on a UI being attached; writes cannot be, because
        // the agent's sandbox no longer binds the workspace — mediation
        // is its ONLY way to touch a file, window or no window. Without a
        // UI both paths still work, just without live-buffer awareness.
        let fs_probe = ui_probe.clone();
        let fs_probe_write = ui_probe;
        let fs_root = cwd.clone();
        let fs_root_write = cwd.clone();

        // The terminal extension, served or not. `None` is safe mode and the
        // outside-confined topology: the handlers below are still
        // registered (there is nowhere else to put them), but the
        // capability goes unadvertised, so a conforming agent never calls
        // them — and each one refuses on its own if a non-conforming one
        // does. See `crate::terminal` for why the position changed for
        // container mode and holds everywhere else.
        let terminals = terminal_host.map(crate::terminal::Terminals::new);
        let serves_terminals = terminals.is_some();
        let terminals_for_create = terminals.clone();
        let terminals_for_output = terminals.clone();
        let terminals_for_wait = terminals.clone();
        let terminals_for_kill = terminals.clone();
        let terminals_for_release = terminals.clone();
        let terminals_for_close = terminals.clone();

        tokio::spawn(async move {
            let result = agent_client_protocol::Client
                .builder()
                .name("taste-ide")
                .on_receive_request(
                    async move |request: ReadTextFileRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        // fs/read_text_file: the agent asks the CLIENT for
                        // file content, so open editor buffers — unsaved
                        // edits included — are what it reads. Answered
                        // out-of-band; a read must never block the
                        // dispatch loop.
                        let probe = fs_probe.clone();
                        let root = fs_root.clone();
                        cx.spawn(async move {
                            let content = read_text_file(
                                probe.as_ref(),
                                &root,
                                &request.path,
                                request.line,
                                request.limit,
                            )
                            .await;
                            match content {
                                Ok(content) => {
                                    responder.respond(ReadTextFileResponse::new(content))?
                                }
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WriteTextFileRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        // fs/write_text_file: the agent hands the CLIENT
                        // the new contents and the IDE applies them, so an
                        // open file takes the edit into the user's own
                        // buffer and undo stack instead of changing under
                        // their cursor. Answered out-of-band; a write must
                        // never block the dispatch loop.
                        let probe = fs_probe_write.clone();
                        let root = fs_root_write.clone();
                        cx.spawn(async move {
                            let written = write_text_file(
                                probe.as_ref(),
                                &root,
                                safe_mode,
                                &request.path,
                                &request.content,
                            )
                            .await;
                            match written {
                                Ok(()) => responder.respond(WriteTextFileResponse::new())?,
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        let _ = events_for_notify
                            .send(SessionEvent::Update(notification.update))
                            .await;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        let (reply_tx, reply_rx) =
                            tokio::sync::oneshot::channel::<RequestPermissionOutcome>();
                        let forwarded = events_for_perm
                            .send(SessionEvent::Permission {
                                request,
                                reply: reply_tx,
                            })
                            .await
                            .is_ok();
                        // Answer out-of-band so the dispatch loop stays free
                        // while the user decides.
                        cx.spawn(async move {
                            let outcome = if forwarded {
                                reply_rx
                                    .await
                                    .unwrap_or(RequestPermissionOutcome::Cancelled)
                            } else {
                                RequestPermissionOutcome::Cancelled
                            };
                            responder.respond(RequestPermissionResponse::new(outcome))?;
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                // ---- The terminal extension (`terminal/*`), container
                // mode only. Five requests, all answered out-of-band: a
                // `wait_for_exit` on a `cargo build` parks for minutes, and
                // parking the dispatch loop would stop the whole session.
                .on_receive_request(
                    async move |request: CreateTerminalRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        // No permission prompt here, deliberately: creating
                        // a terminal is the exec authority the agent
                        // already holds in container mode, and a dialog per
                        // command is one whose only answer is yes. The
                        // reasoning is in `crate::terminal`'s module docs;
                        // the user's control is the Kill button on the tab.
                        let terminals = terminals_for_create.clone();
                        cx.spawn(async move {
                            match serve_terminal(terminals.as_ref(), |t| t.create(&request)) {
                                Ok(id) => responder.respond(CreateTerminalResponse::new(id))?,
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: TerminalOutputRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        let terminals = terminals_for_output.clone();
                        cx.spawn(async move {
                            match serve_terminal(terminals.as_ref(), |t| {
                                t.output(&request.terminal_id)
                            }) {
                                Ok(response) => responder.respond(response)?,
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WaitForTerminalExitRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        let terminals = terminals_for_wait.clone();
                        cx.spawn(async move {
                            let exit = match terminals.as_ref() {
                                Some(terminals) => terminals
                                    .wait_for_exit(&request.terminal_id)
                                    .await
                                    .map_err(|e| e.to_string()),
                                None => Err(UNSERVED.to_string()),
                            };
                            match exit {
                                Ok(exit) => {
                                    responder.respond(WaitForTerminalExitResponse::new(exit))?
                                }
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: KillTerminalRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        let terminals = terminals_for_kill.clone();
                        cx.spawn(async move {
                            match serve_terminal(terminals.as_ref(), |t| {
                                t.kill(&request.terminal_id)
                            }) {
                                Ok(()) => responder.respond(KillTerminalResponse::new())?,
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReleaseTerminalRequest,
                                responder,
                                cx: ConnectionTo<Agent>| {
                        let terminals = terminals_for_release.clone();
                        cx.spawn(async move {
                            match serve_terminal(terminals.as_ref(), |t| {
                                t.release(&request.terminal_id)
                            }) {
                                Ok(()) => responder.respond(ReleaseTerminalResponse::new())?,
                                Err(message) => responder.respond_with_internal_error(message)?,
                            }
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                    run_session(
                        connection,
                        cwd,
                        mcp_bridge,
                        resume_session,
                        command_rx,
                        event_tx,
                        serves_terminals,
                    )
                    .await
                })
                .await;

            // The connection is over: whatever the agent left running dies
            // with it. `AgentClient::drop` does this too — a respawn is
            // both events, and neither is guaranteed to come first.
            if let Some(terminals) = &terminals_for_close {
                terminals.release_all();
            }
            let error = result.err().map(|e| e.to_string());
            let _ = events_for_close.send(SessionEvent::Closed(error)).await;
        });

        Self {
            spec,
            commands: command_tx,
            events: event_rx,
            terminals,
        }
    }
}

/// What an agent hears when it calls a `terminal/*` method the IDE did not
/// advertise. A conforming agent never sees this — the capability is false
/// in safe mode — and one that tries anyway gets the same refusal
/// `ide_exec` gives, for the same reason.
const UNSERVED: &str = "this session does not serve terminals: its environment has no \
     devcontainer running, so there is nowhere to run a command — and agent commands \
     never fall back to the user's host. Start the devcontainer and the agent respawns \
     inside it with terminals served.";

/// Run one terminal operation, or say why there are no terminals.
fn serve_terminal<T>(
    terminals: Option<&crate::terminal::Terminals>,
    operation: impl FnOnce(&crate::terminal::Terminals) -> Result<T>,
) -> Result<T, String> {
    match terminals {
        Some(terminals) => operation(terminals).map_err(|e| e.to_string()),
        None => Err(UNSERVED.to_string()),
    }
}

/// PATH-style lookup so spawn failures can say which program is missing.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(program);
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.exists())
    })
}

/// The session driver: initialize → (authenticate?) → session/new → serve
/// commands until the UI drops its handle.
async fn run_session(
    connection: ConnectionTo<Agent>,
    cwd: PathBuf,
    mcp_bridge: Option<(String, Vec<String>)>,
    resume_session: Option<String>,
    command_rx: async_channel::Receiver<Command>,
    event_tx: async_channel::Sender<SessionEvent>,
    serves_terminals: bool,
) -> Result<(), agent_client_protocol::Error> {
    // First contact can include npx downloading the adapter; generous but
    // bounded, so a dead registry doesn't leave the pane spinning forever.
    let init = match tokio::time::timeout(std::time::Duration::from_secs(180), {
        let mut request = InitializeRequest::new(ProtocolVersion::V1);
        // Terminal-auth capability: the Claude Code adapter only
        // advertises sign-in methods to clients that can run the login
        // TUI in a terminal — the console can.
        request.client_capabilities.auth.terminal = true;
        // The IDE serves the agent's filesystem, both directions. Reads
        // come to us so open editor buffers answer them (unsaved edits
        // included); writes come to us so they land in the buffer the user
        // is looking at, and so `taste_core::policy::write_allowed` — not
        // the topology of a bind mount — is what decides whether a write
        // is permitted. That check is the same one the user's own edits
        // pass through, which is the point: one rule, one implementation,
        // whoever is asking.
        request.client_capabilities.fs.read_text_file = true;
        request.client_capabilities.fs.write_text_file = true;
        // The terminal extension, advertised only when this session's
        // environment can host the agent's processes — which, since the
        // agent only relocates under exactly that condition, means "the
        // agent is already inside this container".
        //
        // ACP v1 has one capability flag, sent once, in `initialize`: there
        // is no per-session advertisement and no renegotiation. That is the
        // honest mechanism here rather than a limitation, because a chat's
        // topology change IS a respawn (ENVIRONMENTS.md → Relocation: "the
        // chat never restarts; the process does"), bridged by
        // `session/load`. A session that relocates comes back advertising
        // terminals; one that drops to safe mode comes back without them.
        // The window in between — a container dying under a live session —
        // is covered by the handlers refusing per request.
        request.client_capabilities.terminal = serves_terminals;
        connection.send_request(request).block_task()
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = event_tx
                .send(SessionEvent::Closed(Some(
                    "agent did not respond to initialize within 3 minutes \
                     (first run downloads the adapter — check network access)"
                        .into(),
                )))
                .await;
            return Ok(());
        }
    };
    let auth_methods = init.auth_methods;
    let can_load = init.agent_capabilities.load_session;
    // Commands that arrive before the session exists (the user's first
    // prompt racing the sign-in round-trip) are deferred, never dropped.
    let mut deferred: std::collections::VecDeque<Command> = std::collections::VecDeque::new();

    // Restore first when asked and possible: `session/load` replays history
    // as ordinary session/update notifications. Any failure (expired id,
    // agent restarted) falls through to a fresh session.
    if let (Some(previous), true) = (&resume_session, can_load) {
        let mut request = LoadSessionRequest::new(previous.clone(), cwd.clone());
        if let Some((command, args)) = &mcp_bridge {
            request.mcp_servers.push(mcp_server_entry(command, args));
        }
        match connection.send_request(request).block_task().await {
            Ok(loaded) => {
                let _ = event_tx
                    .send(SessionEvent::Ready {
                        session_id: previous.clone(),
                        restored: true,
                        restore_failed: false,
                        modes: loaded.modes.clone(),
                        config_options: loaded.config_options.clone().unwrap_or_default(),
                    })
                    .await;
                return serve_commands(
                    &connection,
                    previous.clone().into(),
                    deferred,
                    command_rx,
                    event_tx,
                    auth_methods,
                )
                .await;
            }
            Err(error) => {
                tracing::info!("session/load failed ({error}); starting fresh");
            }
        }
    }

    // session/new, with an authentication round-trip if the agent wants one.
    let session = loop {
        let mut request = NewSessionRequest::new(cwd.clone());
        if let Some((command, args)) = &mcp_bridge {
            request.mcp_servers.push(mcp_server_entry(command, args));
        }
        match connection.send_request(request).block_task().await {
            Ok(session) => break session,
            Err(error) => {
                if auth_methods.is_empty() {
                    return Err(error);
                }
                let _ = event_tx
                    .send(SessionEvent::AuthRequired {
                        methods: auth_methods.clone(),
                    })
                    .await;
                // Wait for the user to pick a method. Anything else that
                // arrives meanwhile (the first prompt, typically) is
                // deferred and replayed once the session is up.
                loop {
                    match command_rx.recv().await {
                        Ok(Command::Authenticate(method)) => {
                            connection
                                .send_request(AuthenticateRequest::new(method))
                                .block_task()
                                .await?;
                            break;
                        }
                        Ok(other) => deferred.push_back(other),
                        Err(_) => return Ok(()), // UI dropped the client
                    }
                }
            }
        }
    };

    let _ = event_tx
        .send(SessionEvent::Ready {
            session_id: session.session_id.to_string(),
            restored: false,
            // A wanted restore that didn't happen (load failed, or the
            // agent can't load at all) — either way the conversation the
            // user expected is not the one on screen.
            restore_failed: resume_session.is_some(),
            modes: session.modes.clone(),
            config_options: session.config_options.clone().unwrap_or_default(),
        })
        .await;
    let session_id = session.session_id;
    serve_commands(
        &connection,
        session_id,
        deferred,
        command_rx,
        event_tx,
        auth_methods,
    )
    .await
}

/// The post-setup command loop, shared by fresh and restored sessions.
/// Commands arriving while a turn is in flight are queued (Cancel excepted),
/// never dropped: a prompt typed during a turn runs right after it.
async fn serve_commands(
    connection: &ConnectionTo<Agent>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    mut queued: std::collections::VecDeque<Command>,
    command_rx: async_channel::Receiver<Command>,
    event_tx: async_channel::Sender<SessionEvent>,
    auth_methods: Vec<AuthMethod>,
) -> Result<(), agent_client_protocol::Error> {
    loop {
        let command = match queued.pop_front() {
            Some(command) => command,
            None => match command_rx.recv().await {
                Ok(command) => command,
                Err(_) => return Ok(()), // UI dropped the client
            },
        };
        match command {
            Command::Prompt(blocks) => {
                // Keep serving Cancel while the turn runs; the prompt then
                // resolves with StopReason::Cancelled on its own.
                let request =
                    connection.send_request(PromptRequest::new(session_id.clone(), blocks));
                let mut turn = std::pin::pin!(request.block_task());
                let response = loop {
                    tokio::select! {
                        result = &mut turn => match result {
                            Ok(response) => break Some(response),
                            Err(error) => {
                                // A rejected prompt (auth expiry, agent
                                // hiccup) must not kill the connection.
                                let message = error.to_string();
                                let auth_failure = message.contains("Authentication required");
                                if auth_failure && !auth_methods.is_empty() {
                                    let _ = event_tx
                                        .send(SessionEvent::AuthRequired {
                                            methods: auth_methods.clone(),
                                        })
                                        .await;
                                }
                                let _ = event_tx
                                    .send(SessionEvent::PromptFailed { message })
                                    .await;
                                if auth_failure {
                                    // Probed behavior: after rejecting a
                                    // prompt for auth, the adapter stops
                                    // answering requests entirely. End the
                                    // session; the next send spawns fresh.
                                    return Ok(());
                                }
                                break None;
                            }
                        },
                        command = command_rx.recv() => match command {
                            Ok(Command::Cancel) => {
                                connection.send_notification(
                                    CancelNotification::new(session_id.clone()),
                                )?;
                            }
                            Ok(other) => queued.push_back(other),
                            Err(_) => return Ok(()), // UI dropped the client
                        },
                    }
                };
                let Some(response) = response else {
                    continue;
                };
                let _ = event_tx
                    .send(SessionEvent::TurnEnded {
                        reason: response.stop_reason,
                        usage: response.usage,
                    })
                    .await;
            }
            Command::Cancel => {} // nothing in flight
            // Control commands are never fatal: a rejected mode or config
            // change must not tear down a healthy conversation.
            Command::SetMode(mode) => {
                // Bounded: a catatonic adapter must not leave the mode UI
                // optimistically switched forever.
                let request = connection
                    .send_request(SetSessionModeRequest::new(session_id.clone(), mode.clone()));
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(10), request.block_task())
                        .await;
                let message = match result {
                    Ok(Ok(_)) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(_) => Some("agent did not respond within 10s".into()),
                };
                if let Some(message) = message {
                    let _ = event_tx
                        .send(SessionEvent::ModeChangeFailed { mode, message })
                        .await;
                }
            }
            Command::SetConfigOption(config, value) => {
                if let Err(e) = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        config,
                        value,
                    ))
                    .block_task()
                    .await
                {
                    let _ = event_tx
                        .send(SessionEvent::CommandFailed {
                            message: e.to_string(),
                        })
                        .await;
                }
            }
            Command::SetConfigBool(config, value) => {
                use agent_client_protocol::schema::v1::SessionConfigOptionValue;
                if let Err(e) = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        config,
                        SessionConfigOptionValue::Boolean { value },
                    ))
                    .block_task()
                    .await
                {
                    let _ = event_tx
                        .send(SessionEvent::CommandFailed {
                            message: e.to_string(),
                        })
                        .await;
                }
            }
            Command::Authenticate(method) => {
                // Late re-auth (e.g. expired credentials mid-session).
                if let Err(e) = connection
                    .send_request(AuthenticateRequest::new(method))
                    .block_task()
                    .await
                {
                    let _ = event_tx
                        .send(SessionEvent::CommandFailed {
                            message: e.to_string(),
                        })
                        .await;
                }
            }
        }
    }
}

/// The IDE's MCP stdio bridge as a session mcp_servers entry.
fn mcp_server_entry(command: &str, args: &[String]) -> McpServer {
    McpServer::Stdio(McpServerStdio::new("taste-ide", command).args(args.to_vec()))
}

/// Serve `fs/read_text_file`: the editor's live buffer when the UI reports
/// one, else the disk. Confined to the workspace either way — this handler
/// runs UNCONFINED in the IDE process, so the boundary the agent's sandbox
/// enforces for its own fs must be enforced here too, not assumed.
async fn read_text_file(
    probe: Option<&taste_core::ui_probe::UiProbe>,
    root: &std::path::Path,
    path: &std::path::Path,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, String> {
    // Canonicalize both sides so neither `..` nor a symlink steps outside.
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| format!("{}: {e}", root.display()))?;
    if !canonical.starts_with(&root) {
        return Err(format!(
            "{} is outside the workspace; agents read workspace files only",
            path.display()
        ));
    }
    let buffered = match probe {
        Some(probe) => match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            probe.request(taste_core::ui_probe::UiRequest::BufferText {
                path: canonical.clone(),
            }),
        )
        .await
        {
            Ok(Ok(taste_core::ui_probe::UiReply::BufferText(text))) => text,
            // Unattached, slow, or surprised UI: the disk answers. A read
            // must degrade to slightly-stale, never to failure.
            _ => None,
        },
        None => None,
    };
    let content = match buffered {
        Some(text) => text,
        None => tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|e| format!("{}: {e}", canonical.display()))?,
    };
    Ok(slice_lines(&content, line, limit))
}

/// Serve `fs/write_text_file`: the agent hands over new contents and the
/// IDE applies them.
///
/// **This is the enforcement point.** The agent's sandbox no longer binds
/// the workspace, so no mount stands between the agent and a file —
/// `taste_core::policy::write_allowed` is what permits or refuses, which
/// is what CLAUDE.md has always claimed it was. It resolves symlinks
/// before deciding, because the repo is untrusted and can commit them.
///
/// With a UI attached the editor applies the write, so a file the user has
/// open takes the edit into their buffer and undo stack. Headless (tests,
/// no window) it goes through the same `taste_core::textfile` save the
/// editor would have used — same policy check, same `.editorconfig`
/// handling, same bytes.
async fn write_text_file(
    probe: Option<&taste_core::ui_probe::UiProbe>,
    root: &std::path::Path,
    safe_mode: bool,
    path: &std::path::Path,
    content: &str,
) -> Result<(), String> {
    // Refuse before bothering the UI, and say which wall was hit: outside
    // the workspace is a different mistake from safe mode, and an agent
    // that knows which one can do something about it.
    if !taste_core::policy::write_allowed(root, safe_mode, path) {
        return Err(if safe_mode {
            format!(
                "{} is read-only in safe mode — the devcontainer is not running, so writes \
                 are confined to the devcontainer setup and workspace dotfiles. Author \
                 .devcontainer/, reload, and the workspace unlocks; ide_write_policy has \
                 the details.",
                path.display()
            )
        } else {
            format!(
                "{} is outside the writable workspace; agents write workspace files only \
                 (and never .git). See ide_write_policy.",
                path.display()
            )
        });
    }
    let Some(probe) = probe else {
        // No window: apply it ourselves, through the editor's own save.
        // Off-thread because this is blocking IO and the caller is a
        // protocol dispatch task — the same rule the GTK side lives by.
        let (root, path, content) = (root.to_path_buf(), path.to_path_buf(), content.to_string());
        return tokio::task::spawn_blocking(move || {
            let (_, format) = taste_core::textfile::load(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            taste_core::textfile::save(&root, safe_mode, &path, &content, &format).map(|_| ())
        })
        .await
        .map_err(|e| format!("write task failed: {e}"))?;
    };
    // A write must not silently half-land, so unlike a read this does NOT
    // fall back to the disk on timeout: the editor may have applied it
    // already, and writing again behind it could clobber the user's
    // buffer. An error is honest and the agent can retry — the request
    // carries whole file contents, so a retry is idempotent.
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.request(taste_core::ui_probe::UiRequest::BufferWrite {
            path: path.to_path_buf(),
            content: content.to_string(),
        }),
    )
    .await
    {
        Ok(Ok(taste_core::ui_probe::UiReply::BufferWrite(result))) => result,
        Ok(Ok(taste_core::ui_probe::UiReply::Error(message))) => Err(message),
        Ok(Ok(_)) => Err("editor answered a write with the wrong reply".into()),
        Ok(Err(e)) => Err(format!("{}: {e}", path.display())),
        Err(_) => Err(format!(
            "{}: the editor did not answer within 10s; the write may or may not have \
             landed — read the file back before retrying",
            path.display()
        )),
    }
}

/// ACP's read window: `line` is 1-based, `limit` caps the line count.
/// Untouched text passes through byte-identical (trailing newline kept).
fn slice_lines(text: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
    let take = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    text.lines()
        .skip(skip)
        .take(take)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The option that says yes to this call.
///
/// Option ORDER means nothing — agents list them however they like, and a
/// reject-kinded option is regularly first. Taking `options.first()` sent a
/// refusal about as often as an approval, silently, because the caller
/// announced "approved" either way and only the failed tool call gave it
/// away. Match on the kind.
///
/// A one-shot allow beats "always": saying yes to a call means "yes, this
/// one", not "rewrite the agent's standing policy".
pub fn allow_option(options: &[PermissionOption]) -> Option<&PermissionOption> {
    find_kind(options, PermissionOptionKind::AllowOnce)
        .or_else(|| find_kind(options, PermissionOptionKind::AllowAlways))
}

/// The option that says no. Preferred over [`RequestPermissionOutcome::Cancelled`]
/// when the agent offers one: "the user declined" and "the turn was
/// cancelled" are different facts, and agents act on the difference.
pub fn reject_option(options: &[PermissionOption]) -> Option<&PermissionOption> {
    find_kind(options, PermissionOptionKind::RejectOnce)
        .or_else(|| find_kind(options, PermissionOptionKind::RejectAlways))
}

fn find_kind(
    options: &[PermissionOption],
    kind: PermissionOptionKind,
) -> Option<&PermissionOption> {
    options.iter().find(|option| option.kind == kind)
}

/// Answer a request by selecting `option`.
pub fn outcome_for(option: &PermissionOption) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
}

/// Approve a request by taking its allow option. Cancels when the agent
/// offered no way to allow — callers must surface THAT as the unanswered
/// question it is, never as an approval.
pub fn first_allow_outcome(request: &RequestPermissionRequest) -> RequestPermissionOutcome {
    match allow_option(&request.options) {
        Some(option) => outcome_for(option),
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// The agent's sign-in command, wrapped in the SAME confinement the agent
/// itself runs under. Credentials must land in the exact home the agent
/// reads: running the login in any other execution context (the
/// devcontainer, the host shell) signs in a different universe — the
/// original bug was `npx … auth login` failing inside the devcontainer
/// while the agent lived in its own container with `taste-agent-home`.
///
/// Mirrors [`AgentClient::spawn`]'s confinement cascade minus the MCP
/// wiring (a login needs no IDE bridge). Env is empty where the wrapper
/// already bakes it in.
pub struct LoginCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The sign-in command, run in a console tab so a login TUI gets a real
/// pty. Confinement matches a normal agent spawn — including the workspace
/// stand-in, since a sign-in flow has no business reading the project.
///
/// It takes no `safe_mode`: the agent's mount set no longer encodes the
/// mode, because every write is checked when it is made.
pub fn login_command(
    spec: &AgentSpec,
    cwd: &std::path::Path,
    home_volume: &str,
    extra_args: &[String],
    extra_env: &[(String, String)],
) -> Result<LoginCommand> {
    let git_policy = crate::sandbox::ensure_git_policy_file()?;
    let (url_script, url_dir) = crate::sandbox::ensure_url_bridge()?;
    let workspace_stub = crate::sandbox::ensure_workspace_stub(cwd)?;
    let mut spec = spec.clone();
    spec.args.extend(extra_args.iter().cloned());
    spec.env.extend(extra_env.iter().cloned());

    // Self-hosting bootstrap: agent and IDE share this container already.
    if crate::sandbox::inside_container() {
        let mut env = spec.env.clone();
        env.push(("GIT_CONFIG_GLOBAL".into(), git_policy.display().to_string()));
        env.push(("BROWSER".into(), url_script.display().to_string()));
        return Ok(LoginCommand {
            program: spec.command.clone(),
            args: spec.args.clone(),
            env,
        });
    }

    // Preferred: the agent container (env baked in as podman -e flags).
    if let Some((program, args)) = crate::sandbox::container_agent_command(
        &spec,
        cwd,
        &git_policy,
        &workspace_stub,
        home_volume,
        None,
        (&url_script, &url_dir),
        true,
    ) {
        return Ok(LoginCommand {
            program,
            args,
            env: Vec::new(),
        });
    }

    let flatpak = std::path::Path::new("/.flatpak-info").exists();
    if !flatpak && !crate::sandbox::bwrap_available() {
        anyhow::bail!("bubblewrap (bwrap) not found: agents only run confined, never unconfined");
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
    for rel in &spec.home_paths {
        if !rel.ends_with(".json") {
            let _ = std::fs::create_dir_all(home.join(rel));
        }
    }
    let (mut program, mut args) = crate::sandbox::wrap(
        &spec,
        cwd,
        &workspace_stub,
        &home,
        &git_policy,
        None,
        Some((&url_script, &url_dir)),
    );
    if flatpak {
        args.insert(0, program);
        args.insert(0, "--host".into());
        program = "flatpak-spawn".into();
    }
    Ok(LoginCommand {
        program,
        args,
        env: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(id.to_string(), id.to_string(), kind)
    }

    #[test]
    fn approval_follows_the_kind_not_the_order() {
        // The shape that caused silent refusals: reject listed first.
        let options = [
            option("no", PermissionOptionKind::RejectOnce),
            option("yes", PermissionOptionKind::AllowOnce),
        ];
        assert_eq!(allow_option(&options).unwrap().name, "yes");
        assert_eq!(reject_option(&options).unwrap().name, "no");
    }

    #[test]
    fn once_beats_always_in_both_directions() {
        let options = [
            option("always", PermissionOptionKind::AllowAlways),
            option("once", PermissionOptionKind::AllowOnce),
            option("never", PermissionOptionKind::RejectAlways),
            option("not now", PermissionOptionKind::RejectOnce),
        ];
        assert_eq!(allow_option(&options).unwrap().name, "once");
        assert_eq!(reject_option(&options).unwrap().name, "not now");
    }

    #[test]
    fn always_is_taken_when_it_is_all_there_is() {
        let options = [option("always", PermissionOptionKind::AllowAlways)];
        assert_eq!(allow_option(&options).unwrap().name, "always");
        assert!(reject_option(&options).is_none());
    }

    #[test]
    fn no_allow_option_is_not_an_approval() {
        let options = [option("no", PermissionOptionKind::RejectOnce)];
        assert!(allow_option(&options).is_none());
    }

    #[test]
    fn slice_lines_is_one_based_and_identity_without_params() {
        let text = "a\nb\nc\n";
        assert_eq!(slice_lines(text, None, None), "a\nb\nc\n");
        assert_eq!(slice_lines(text, Some(2), None), "b\nc");
        assert_eq!(slice_lines(text, Some(2), Some(1)), "b");
        assert_eq!(slice_lines(text, None, Some(2)), "a\nb");
        assert_eq!(slice_lines(text, Some(9), None), "");
    }

    #[tokio::test]
    async fn agent_reads_are_confined_to_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("a.txt");
        std::fs::write(&inside, "buffered truth\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "no").unwrap();

        let content = read_text_file(None, root.path(), &inside, None, None)
            .await
            .unwrap();
        assert_eq!(content, "buffered truth\n");

        // Straight path outside, and a `..` escape: both refused.
        let denied = read_text_file(None, root.path(), &secret, None, None).await;
        assert!(denied.unwrap_err().contains("outside the workspace"));
        let dotdot = root.path().join("..").join(
            secret
                .strip_prefix(outside.path().parent().unwrap())
                .unwrap(),
        );
        let denied = read_text_file(None, root.path(), &dotdot, None, None).await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn a_dirty_buffer_outranks_the_disk() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.rs");
        std::fs::write(&file, "on disk").unwrap();
        let probe = taste_core::ui_probe::UiProbe::new();
        let requests = probe.requests();
        tokio::spawn(async move {
            while let Ok((request, reply)) = requests.recv().await {
                let taste_core::ui_probe::UiRequest::BufferText { .. } = request else {
                    continue;
                };
                let _ = reply
                    .send(taste_core::ui_probe::UiReply::BufferText(Some(
                        "unsaved in the editor".into(),
                    )))
                    .await;
            }
        });
        let content = read_text_file(Some(&probe), root.path(), &file, None, None)
            .await
            .unwrap();
        assert_eq!(content, "unsaved in the editor");
    }

    /// Headless (no window): the write lands through taste-core's save,
    /// creating the file if the agent is making a new one.
    #[tokio::test]
    async fn agent_writes_land_on_disk_and_can_create_files() {
        let root = tempfile::tempdir().unwrap();
        let new = root.path().join("src").join("new.rs");
        write_text_file(None, root.path(), false, &new, "fn main() {}\n")
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "fn main() {}\n");
    }

    /// The mount that used to stand between the agent and the disk is
    /// gone, so this check is the only thing left. It has to hold for the
    /// same escapes the read path refuses — plus the git object store,
    /// which neither mode ever opens.
    #[tokio::test]
    async fn agent_writes_are_confined_to_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");

        let denied = write_text_file(None, root.path(), false, &secret, "no").await;
        assert!(denied
            .unwrap_err()
            .contains("outside the writable workspace"));
        assert!(!secret.exists(), "a refused write must not have happened");

        // `..` escape, and the git object store.
        let dotdot = root.path().join("..").join("escaped");
        assert!(write_text_file(None, root.path(), false, &dotdot, "no")
            .await
            .is_err());
        let git = root.path().join(".git").join("config");
        assert!(write_text_file(None, root.path(), false, &git, "no")
            .await
            .is_err());
    }

    /// Safe mode binds the agent exactly as it binds the user: the
    /// devcontainer setup is writable so the way out stays open, and
    /// project source is not.
    #[tokio::test]
    async fn safe_mode_confines_agent_writes_to_the_devcontainer_scope() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".devcontainer").join("devcontainer.json");
        write_text_file(None, root.path(), true, &config, "{}\n")
            .await
            .unwrap();
        assert!(config.exists());

        let source = root.path().join("src").join("main.rs");
        let denied = write_text_file(None, root.path(), true, &source, "no").await;
        let message = denied.unwrap_err();
        assert!(message.contains("read-only in safe mode"), "{message}");
        assert!(message.contains("ide_write_policy"), "{message}");
    }

    /// With an editor attached the write goes THROUGH it — the agent's
    /// edit lands in the buffer the user is looking at. And when the
    /// editor refuses, that is the answer: no going around it to the disk.
    #[tokio::test]
    async fn a_write_goes_through_the_editor_and_its_refusal_is_final() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.rs");
        std::fs::write(&file, "original\n").unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let probe = taste_core::ui_probe::UiProbe::new();
        let requests = probe.requests();
        let recorder = seen.clone();
        tokio::spawn(async move {
            while let Ok((request, reply)) = requests.recv().await {
                let taste_core::ui_probe::UiRequest::BufferWrite { path, content } = request else {
                    continue;
                };
                recorder.lock().unwrap().push((path, content.clone()));
                // The editor takes the first write and refuses the second.
                let answer = if content.contains("refuse me") {
                    Err("file has unsaved changes".to_string())
                } else {
                    Ok(())
                };
                let _ = reply
                    .send(taste_core::ui_probe::UiReply::BufferWrite(answer))
                    .await;
            }
        });

        write_text_file(Some(&probe), root.path(), false, &file, "from the agent\n")
            .await
            .unwrap();
        let refused = write_text_file(Some(&probe), root.path(), false, &file, "refuse me\n").await;
        assert_eq!(refused.unwrap_err(), "file has unsaved changes");

        // The editor owns the write both times: we never touched the disk
        // ourselves, so what is on it is still what we started with.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original\n");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
