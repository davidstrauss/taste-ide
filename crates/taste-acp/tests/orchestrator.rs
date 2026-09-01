//! Live proof that a real agent can orchestrate: the tools reach it, it
//! calls one, and a second agent starts working because it did.
//!
//! `#[ignore]`d: this spends real tokens on a real account, needs the
//! network, a credential on disk, and `node` (the MCP stdio bridge). The
//! rest of the suite proves the same seams against `fake_agent.py` with
//! none of that — what only a live run can answer is whether a *model*,
//! given these tool descriptions on its own socket, delegates with them.
//!
//! # The split, stated plainly
//!
//! The **orchestrator is real** (the pinned Claude Code adapter, a real
//! session, real tool calls). The **sub-agent is fake** (`fake_agent.py`),
//! and the **chat strip is a stand-in** — the GTK one lives in `taste-app`
//! and cannot be driven headless. So what this proves live is:
//!
//! - the orchestration tools are listed on the orchestrator's environment
//!   socket and on no other, to a real client's `tools/list`;
//! - a real model reads them and calls `chat_create` unprompted about
//!   *how*, with the task it was told to delegate;
//! - the IDE routes that call to the chat strip, creates a real
//!   environment (a real clone, off the real registry), and delivers the
//!   task into a *real ACP session* with another agent process, which runs
//!   a turn because of it.
//!
//! What it does not prove, and what covers it instead: that the GTK strip
//! creates tabs and binds environments correctly (`chat_tabs.rs`, and the
//! probe screenshots), and the tool semantics themselves (the unit tests
//! in `taste-mcp`, which run everywhere and need no credential).
//!
//! # Running it
//!
//! Provision a credential exactly as `live_proxy.rs` describes
//! (`ANTHROPIC_API_KEY`, or `claude setup-token` into the IDE's own
//! credential file), then:
//!
//! ```sh
//! cargo test -p taste-acp --test orchestrator -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use taste_core::environment::EnvironmentId;
use taste_core::orchestration::{
    ChatFacts, ChatState, CreatedChat, OrchestrationReply, OrchestrationRequest, SendOutcome,
    TranscriptTail,
};
use taste_core::ExecContext;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use taste_acp::{builtin_agents, AgentClient, AgentSpec, SessionEvent};

/// The orchestrator's own environment — the integration workspace.
const HUB: &str = "hub";
/// The environment the stand-in strip creates when the orchestrator
/// delegates.
const WORKER: &str = "worker-1";
/// A token the model has no reason to emit on its own: finding it in what
/// the sub-agent was sent is what proves the task travelled.
const TASK_MARKER: &str = "PING-42";

const READY_TIMEOUT: Duration = Duration::from_secs(180);
const TURN_TIMEOUT: Duration = Duration::from_secs(240);

fn describe(event: &SessionEvent) -> String {
    match event {
        SessionEvent::Ready { .. } => "Ready".into(),
        SessionEvent::AuthRequired { .. } => "AuthRequired".into(),
        SessionEvent::Closed(e) => format!("Closed({e:?})"),
        SessionEvent::Update(_) => "Update".into(),
        SessionEvent::TurnEnded { .. } => "TurnEnded".into(),
        SessionEvent::Permission { .. } => "Permission".into(),
        SessionEvent::ModeChangeFailed { message, .. } => format!("ModeChangeFailed({message})"),
        SessionEvent::CommandFailed { message } => format!("CommandFailed({message})"),
        SessionEvent::PromptFailed { message } => format!("PromptFailed({message})"),
    }
}

async fn next_event(client: &AgentClient, within: Duration) -> SessionEvent {
    tokio::time::timeout(within, client.events.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("event channel closed unexpectedly")
}

/// One repository with one commit — enough for the registry to clone.
fn init_repo(root: &Path) {
    let repo = git2::Repository::init(root).unwrap();
    std::fs::write(root.join("README.md"), "orchestrator live test\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
        .unwrap();
}

/// What the stand-in strip did, in order — the assertion surface.
#[derive(Default)]
struct Seen {
    created: bool,
    sent: Vec<String>,
}

/// The stand-in chat strip.
///
/// It answers the same probe the GTK strip answers, and it does the same
/// two real things the real one does: it creates the environment through
/// the registry (a real clone), and it delivers the prompt into a real ACP
/// session. Everything else — tabs, titles, transcripts — is UI this test
/// has no window for.
fn attach_strip(
    workspace: &taste_core::Workspace,
    environments: Arc<taste_devcontainer::EnvironmentRegistry>,
    sub_agent: AgentSpec,
    mcp_socket: PathBuf,
) -> Arc<Mutex<Seen>> {
    let requests = workspace.orchestration.requests();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let recorder = seen.clone();
    let root = workspace.root().to_path_buf();
    tokio::spawn(async move {
        // The sub-agent's client, created on the first chat_create and
        // kept for the sends that follow — a chat outlives one prompt.
        let mut sub: Option<AgentClient> = None;
        while let Ok((request, reply)) = requests.recv().await {
            let answer = match request {
                OrchestrationRequest::Fleet => OrchestrationReply::Fleet(serde_json::json!([
                    {"environment": HUB, "name": HUB, "mode": "safe"},
                ])),
                OrchestrationRequest::ChatCreate { agent, model } => {
                    println!("strip: chat_create agent={agent:?} model={model:?}");
                    let worker = EnvironmentId::parse(WORKER).unwrap();
                    environments
                        .create(worker.clone())
                        .expect("cloning the worker environment");
                    // The clone's path comes from the REGISTRY, not from
                    // `AgentAim`: a test registry keeps its environments
                    // under a tempdir, and the aim derives the real XDG
                    // state path. Everything else about the spawn is the
                    // aim's shape.
                    let client = AgentClient::spawn(
                        sub_agent.clone(),
                        environments.env_repo(&worker),
                        root.clone(),
                        Some(taste_acp::sandbox::mcp_bridge_command(&mcp_socket)),
                        Some(mcp_socket.clone()),
                        taste_acp::AgentHome {
                            environment: worker.to_string(),
                            volume: taste_core::environment::env_home_volume(&root, &worker),
                        },
                        None,
                        None,
                        true,
                        None,
                        None,
                    )
                    .expect("spawning the sub-agent");
                    // Wait for its session, as the real strip does before
                    // it validates a model or seeds a task.
                    loop {
                        match next_event(&client, READY_TIMEOUT).await {
                            SessionEvent::Ready { .. } => break,
                            SessionEvent::Update(_) => continue,
                            other => panic!("sub-agent did not come up: {}", describe(&other)),
                        }
                    }
                    recorder.lock().unwrap().created = true;
                    sub = Some(client);
                    OrchestrationReply::Created(CreatedChat {
                        chat: worker,
                        agent: sub_agent.id.clone(),
                        model,
                        note: "Its container is NOT running.".into(),
                    })
                }
                OrchestrationRequest::ChatSend { chat, text } => {
                    println!("strip: chat_send {chat}: {text}");
                    recorder.lock().unwrap().sent.push(text.clone());
                    match &sub {
                        Some(client) => {
                            client.prompt(text).expect("prompting the sub-agent");
                            // Drain to the sub-agent's turn end: the proof
                            // is that another agent process ran because
                            // the orchestrator said so.
                            loop {
                                match next_event(client, TURN_TIMEOUT).await {
                                    SessionEvent::TurnEnded { .. } => break,
                                    _ => continue,
                                }
                            }
                            OrchestrationReply::Sent(SendOutcome { queued: false })
                        }
                        None => OrchestrationReply::Error("no such chat".into()),
                    }
                }
                OrchestrationRequest::ChatStatus { chat } => {
                    OrchestrationReply::Status(ChatFacts {
                        chat,
                        agent: sub_agent.display_name.clone(),
                        model: None,
                        session: None,
                        state: if sub.is_some() {
                            ChatState::Idle
                        } else {
                            ChatState::Disconnected
                        },
                        idle_for_secs: Some(0),
                        turns: 1,
                        usage: None,
                        orchestrator: false,
                    })
                }
                OrchestrationRequest::ChatTranscript { .. } => {
                    OrchestrationReply::Transcript(TranscriptTail::default())
                }
            };
            let _ = reply.send(answer).await;
        }
    });
    seen
}

/// `tools/list` over a raw connection — what a client sees on this socket.
async fn tools_on(socket: &Path) -> Vec<String> {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let mut payload = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    }))
    .unwrap();
    payload.push(b'\n');
    stream.write_all(&payload).await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends real tokens, needs a credential, network and node"]
async fn an_orchestrator_delegates_and_a_second_agent_starts_working() {
    if taste_authproxy::discover().await.is_err() {
        panic!(
            "no credential provisioned for the IDE — set ANTHROPIC_API_KEY, or write {} \
             (see live_proxy.rs for `claude setup-token`)",
            taste_authproxy::credential_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the IDE credential file".into()),
        );
    }
    // The agent runs confined, with a home of its own: the proxy is how a
    // credential reaches it at all. Set before the first spawn — the
    // handle is a OnceLock.
    std::env::set_var("TASTE_AUTH_PROXY", "1");

    let workspace_dir = tempfile::tempdir().unwrap();
    let root = workspace_dir.path().canonicalize().unwrap();
    init_repo(&root);
    let state_dir = tempfile::tempdir().unwrap();

    let mut workspace = taste_core::Workspace::open(root.clone());
    workspace.exec = ExecContext::host_unsandboxed_for_tests();
    let environments = taste_devcontainer::EnvironmentRegistry::new_for_tests(
        root.clone(),
        workspace.events.clone(),
        workspace.exec.clone(),
        state_dir.path().to_path_buf(),
    );
    let hub = EnvironmentId::parse(HUB).unwrap();
    let hub_root = environments
        .create(hub.clone())
        .expect("cloning the orchestrator's own environment")
        .root()
        .to_path_buf();

    let packager = taste_flatpak::Packager::new(root.clone(), workspace.events.clone());
    let server = taste_mcp::McpServer::new(environments.clone(), packager, workspace.clone());
    // Sockets are named explicitly: the derived paths live in the
    // process-global $XDG_RUNTIME_DIR, shared with every other test.
    let hub_socket = root.join("hub.sock");
    let primary_socket = root.join("primary.sock");
    {
        let server = server.clone();
        let (env, socket) = (hub.clone(), hub_socket.clone());
        tokio::spawn(async move { server.serve(env, socket).await });
    }
    {
        let server = server.clone();
        let socket = primary_socket.clone();
        tokio::spawn(async move { server.serve(EnvironmentId::primary(), socket).await });
    }
    for _ in 0..100 {
        if hub_socket.exists() && primary_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The designation, which the user makes in the chat pane's own row.
    server.set_orchestrator(Some(hub.clone()));

    // Proof #1, live and before any model is involved: presence follows
    // the role, on the real socket, over the real protocol.
    let on_hub = tools_on(&hub_socket).await;
    let on_primary = tools_on(&primary_socket).await;
    for tool in ["env_list", "chat_create", "chat_send", "branches_published"] {
        assert!(
            on_hub.contains(&tool.to_string()),
            "{tool} missing: {on_hub:?}"
        );
        assert!(
            !on_primary.contains(&tool.to_string()),
            "{tool} leaked onto the primary's socket: {on_primary:?}"
        );
    }

    // The stand-in strip, holding a real sub-agent behind it.
    let fake = AgentSpec::new(
        "fake",
        "Fake Agent",
        "python3",
        &[concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_agent.py")],
        &[],
    );
    let seen = attach_strip(
        &workspace,
        environments.clone(),
        fake,
        // The sub-agent connects on ITS environment's socket in the real
        // IDE; here it needs a socket that exists, and what it does with
        // its tools is not what this test is about.
        hub_socket.clone(),
    );

    // The orchestrator: a real agent, aimed at the hub, reaching the IDE
    // through the same node stdio bridge a relocated agent uses.
    let spec = builtin_agents()
        .into_iter()
        .find(|s| s.id == "claude-code")
        .expect("the claude-code spec is built in");
    let orchestrator = AgentClient::spawn(
        spec,
        hub_root.clone(),
        root.clone(),
        Some(taste_acp::sandbox::mcp_bridge_command(&hub_socket)),
        Some(hub_socket.clone()),
        taste_acp::AgentHome {
            environment: hub.to_string(),
            volume: taste_core::environment::env_home_volume(&root, &hub),
        },
        None,
        None,
        true,
        None,
        None,
    )
    .expect("spawning the orchestrator");

    loop {
        match next_event(&orchestrator, READY_TIMEOUT).await {
            SessionEvent::Ready { config_options, .. } => {
                // Printed rather than asserted: these ids are the
                // adapter's business and they move with its releases.
                // What `chat_create`'s `model` argument means is exactly
                // "one of these", so a live run is the only place the
                // real list can be read off.
                for option in &config_options {
                    println!(
                        "advertised config option {:?} ({})",
                        option.id.to_string(),
                        option.name
                    );
                    if let agent_client_protocol::schema::v1::SessionConfigKind::Select(select) =
                        &option.kind
                    {
                        if let agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(
                            values,
                        ) = &select.options
                        {
                            for value in values {
                                println!("  value {:?} — {}", value.value.to_string(), value.name);
                            }
                        }
                    }
                }
                break;
            }
            SessionEvent::Update(_) => continue,
            other => panic!("the orchestrator did not come up: {}", describe(&other)),
        }
    }

    // Deliberately says nothing about HOW: if the tool descriptions do
    // not carry their own meaning, this fails, which is the point of
    // asking a model rather than a mock.
    orchestrator
        .prompt(format!(
            "Delegate this to a new sub-agent: \"{TASK_MARKER}: reply with the word pong\". \
             When it is dispatched, reply with the chat id you were given and nothing else."
        ))
        .unwrap();

    loop {
        match next_event(&orchestrator, TURN_TIMEOUT).await {
            SessionEvent::TurnEnded { reason, .. } => {
                println!("orchestrator turn ended: {reason:?}");
                break;
            }
            // The user answers the ORCHESTRATOR's prompts; this test is
            // standing in for the user. Note what is NOT here: any way to
            // answer a *sub-chat's* prompt, because no such path exists.
            SessionEvent::Permission { request, reply } => {
                println!(
                    "orchestrator asked permission: {:?}",
                    request.tool_call.fields.title
                );
                let allow = request
                    .options
                    .iter()
                    .find(|o| {
                        matches!(
                            o.kind,
                            agent_client_protocol::schema::v1::PermissionOptionKind::AllowOnce
                                | agent_client_protocol::schema::v1::PermissionOptionKind::AllowAlways
                        )
                    })
                    .expect("a permission request with nothing to allow");
                let _ = reply.send(
                    agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(
                        agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                            allow.option_id.clone(),
                        ),
                    ),
                );
            }
            SessionEvent::Update(_) => continue,
            other => panic!("the orchestrator's turn failed: {}", describe(&other)),
        }
    }

    let seen = seen.lock().unwrap();
    assert!(
        seen.created,
        "the orchestrator never called chat_create: the tools were listed but not usable"
    );
    assert!(
        seen.sent.iter().any(|text| text.contains(TASK_MARKER)),
        "the task never reached the sub-agent: {:?}",
        seen.sent
    );
    // The environment is real: a clone on disk, in the registry, with a
    // supervisor of its own.
    let worker = EnvironmentId::parse(WORKER).unwrap();
    assert!(
        environments.get(&worker).is_some(),
        "the worker environment is not in the registry"
    );
    assert!(
        environments.env_repo(&worker).join(".git").exists(),
        "the worker environment has no clone"
    );
    println!(
        "live orchestration: chat_create -> {WORKER} cloned, task delivered ({} send(s))",
        seen.sent.len()
    );
}
