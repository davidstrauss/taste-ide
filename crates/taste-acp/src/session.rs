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
    InitializeRequest, LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOption, SessionConfigValueId,
    SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TextContent, Usage,
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

    /// Spawn the agent subprocess and run its session on the tokio runtime.
    ///
    /// `cwd` is the workspace root. `mcp_bridge` is the stdio-bridge command
    /// registered so the agent can reach the IDE's MCP server. `safe_mode`
    /// fixes the sandbox's mount set for the life of the session.
    ///
    /// The agent always runs confined (see [`crate::sandbox`]); if
    /// bubblewrap is unavailable this returns an error instead of launching
    /// unconfined.
    pub fn spawn(
        spec: AgentSpec,
        cwd: PathBuf,
        mcp_bridge: Option<(String, Vec<String>)>,
        mcp_socket: Option<PathBuf>,
        safe_mode: bool,
        resume_session: Option<String>,
    ) -> Result<Self> {
        let git_policy = crate::sandbox::ensure_git_policy_file()?;
        let (url_script, url_dir) = crate::sandbox::ensure_url_bridge()?;

        // Self-hosting bootstrap: the IDE itself runs inside its own
        // devcontainer. bwrap cannot nest there — and the container already
        // provides the confinement contract (the user's real home is not
        // mounted). Spawn directly, keeping the env-level protections.
        if crate::sandbox::inside_container() {
            let mut spec = spec;
            spec.env
                .push(("GIT_CONFIG_GLOBAL".into(), git_policy.display().to_string()));
            spec.env
                .push(("BROWSER".into(), url_script.display().to_string()));
            let program = spec.command.clone();
            let args = spec.args.clone();
            return Ok(Self::spawn_with_command(
                spec,
                cwd,
                mcp_bridge,
                resume_session,
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
            mcp_socket.as_deref(),
            (&url_script, &url_dir),
            false,
        ) {
            // The IDE binary's path means nothing inside the agent's
            // container; socat carries the MCP stdio bridge instead.
            let bridge = mcp_socket.as_ref().map(|socket| {
                (
                    "socat".to_string(),
                    vec!["STDIO".into(), format!("UNIX-CONNECT:{}", socket.display())],
                )
            });
            return Ok(Self::spawn_with_command(
                spec,
                cwd,
                bridge.or(mcp_bridge),
                resume_session,
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
            safe_mode,
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
        Self::spawn_with_command(spec, cwd, None, None, program, args)
    }

    fn spawn_with_command(
        spec: AgentSpec,
        cwd: PathBuf,
        mcp_bridge: Option<(String, Vec<String>)>,
        resume_session: Option<String>,
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

        tokio::spawn(async move {
            let result = agent_client_protocol::Client
                .builder()
                .name("taste-ide")
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
                .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                    run_session(
                        connection,
                        cwd,
                        mcp_bridge,
                        resume_session,
                        command_rx,
                        event_tx,
                    )
                    .await
                })
                .await;

            let error = result.err().map(|e| e.to_string());
            let _ = events_for_close.send(SessionEvent::Closed(error)).await;
        });

        Self {
            spec,
            commands: command_tx,
            events: event_rx,
        }
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
) -> Result<(), agent_client_protocol::Error> {
    // First contact can include npx downloading the adapter; generous but
    // bounded, so a dead registry doesn't leave the pane spinning forever.
    let init = match tokio::time::timeout(std::time::Duration::from_secs(180), {
        let mut request = InitializeRequest::new(ProtocolVersion::V1);
        // Terminal-auth capability: the Claude Code adapter only
        // advertises sign-in methods to clients that can run the login
        // TUI in a terminal — the console can.
        request.client_capabilities.auth.terminal = true;
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

/// Convenience: approve a permission request by taking its first
/// "allow"-kinded option (used by the auto-approve policy).
pub fn first_allow_outcome(request: &RequestPermissionRequest) -> RequestPermissionOutcome {
    match request.options.first() {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
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

pub fn login_command(
    spec: &AgentSpec,
    cwd: &std::path::Path,
    safe_mode: bool,
    extra_args: &[String],
    extra_env: &[(String, String)],
) -> Result<LoginCommand> {
    let git_policy = crate::sandbox::ensure_git_policy_file()?;
    let (url_script, url_dir) = crate::sandbox::ensure_url_bridge()?;
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
        safe_mode,
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
