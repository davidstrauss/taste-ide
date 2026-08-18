//! End-to-end ACP client test against a scripted agent (fake_agent.py).
//!
//! This exercises the real wire path — subprocess spawn, newline-delimited
//! JSON-RPC framing, initialize → session/new → session/prompt, streamed
//! session/update notifications, set_mode — with no network and no real
//! agent credentials.

use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, StopReason};
use taste_acp::{AgentClient, AgentSpec, SessionEvent};

fn fake_agent_spec() -> AgentSpec {
    let script = format!("{}/tests/fake_agent.py", env!("CARGO_MANIFEST_DIR"));
    AgentSpec::new("fake", "Fake Agent", "python3", &[&script], &[])
}

async fn next_event(client: &AgentClient) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), client.events.recv())
        .await
        .expect("timed out waiting for session event")
        .expect("event channel closed unexpectedly")
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_roundtrip() {
    let workspace = tempfile::tempdir().unwrap();
    let client =
        AgentClient::spawn_unconfined_for_tests(fake_agent_spec(), workspace.path().to_path_buf());

    // Session comes up and reports the agent's modes.
    let ready = next_event(&client).await;
    let SessionEvent::Ready { modes, .. } = ready else {
        let what = match &ready {
            SessionEvent::AuthRequired { .. } => "AuthRequired".to_string(),
            SessionEvent::Closed(e) => format!("Closed({e:?})"),
            SessionEvent::Update(_) => "Update".to_string(),
            SessionEvent::TurnEnded { .. } => "TurnEnded".to_string(),
            SessionEvent::Ready { .. } => unreachable!(),
            SessionEvent::Permission { .. } => "Permission".to_string(),
            SessionEvent::ModeChangeFailed { message, .. } => {
                format!("ModeChangeFailed({message})")
            }
            SessionEvent::CommandFailed { message } => format!("CommandFailed({message})"),
            SessionEvent::PromptFailed { message } => format!("PromptFailed({message})"),
        };
        panic!("expected Ready first, got {what}");
    };
    let modes = modes.expect("fake agent advertises modes");
    assert_eq!(modes.available_modes.len(), 2);
    assert_eq!(modes.current_mode_id.to_string(), "normal");

    // A prompt streams a chunk and ends the turn.
    client.prompt("hi").unwrap();
    let update = next_event(&client).await;
    let SessionEvent::Update(SessionUpdate::AgentMessageChunk(chunk)) = update else {
        panic!("expected an agent message chunk");
    };
    let ContentBlock::Text(text) = &chunk.content else {
        panic!("expected text content");
    };
    assert_eq!(text.text, "hello from fake agent");

    let ended = next_event(&client).await;
    let SessionEvent::TurnEnded { reason, .. } = ended else {
        panic!("expected TurnEnded");
    };
    assert_eq!(reason, StopReason::EndTurn);

    // Mode switching round-trips without error.
    let mode_id = modes.available_modes[1].id.clone();
    client.set_mode(mode_id).unwrap();
    // A second prompt after set_mode proves the connection is still healthy.
    client.prompt("again").unwrap();
    loop {
        match next_event(&client).await {
            SessionEvent::TurnEnded { .. } => break,
            SessionEvent::Update(_) => {}
            SessionEvent::Closed(error) => panic!("connection closed early: {error:?}"),
            _ => {}
        }
    }
}

/// The agent asks the CLIENT for file contents (fs/read_text_file), and the
/// client answers from the editor: an open buffer outranks the disk, and a
/// file the editor knows nothing about falls back to it. The other half of
/// what this proves is that the exchange COMPLETES — an unanswered client
/// request is an agent that hangs mid-tool-call, forever.
#[tokio::test(flavor = "multi_thread")]
async fn agent_file_reads_come_from_the_editor_then_the_disk() {
    use taste_core::ui_probe::{UiProbe, UiReply, UiRequest};

    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::write(root.join("open.md"), "what the disk has\n").unwrap();
    std::fs::write(root.join("closed.md"), "only on disk\n").unwrap();

    // Stand in for the editor: one file is open with unsaved edits, and
    // nothing else is.
    let probe = UiProbe::new();
    let requests = probe.requests();
    let responder = std::thread::spawn(move || {
        while let Ok((request, reply)) = requests.recv_blocking() {
            let UiRequest::BufferText { path } = request else {
                let _ = reply.send_blocking(UiReply::Error("unexpected request".into()));
                continue;
            };
            let answer = path
                .ends_with("open.md")
                .then(|| "what the user is looking at\n".to_string());
            let _ = reply.send_blocking(UiReply::BufferText(answer));
        }
    });

    let client = AgentClient::spawn_unconfined_with_ui_for_tests(
        fake_agent_spec(),
        root.clone(),
        probe.clone(),
    );
    assert!(matches!(
        next_event(&client).await,
        SessionEvent::Ready { .. }
    ));

    async fn read(client: &AgentClient, path: &std::path::Path) -> String {
        client.prompt(format!("/read {}", path.display())).unwrap();
        loop {
            match next_event(client).await {
                SessionEvent::Update(SessionUpdate::AgentMessageChunk(chunk)) => {
                    let ContentBlock::Text(text) = &chunk.content else {
                        panic!("expected text content");
                    };
                    return text.text.clone();
                }
                SessionEvent::Closed(error) => panic!("connection closed early: {error:?}"),
                _ => {}
            }
        }
    }

    // The open buffer wins; the unopened file comes off the disk.
    assert_eq!(
        read(&client, &root.join("open.md")).await,
        "what the user is looking at\n"
    );
    assert_eq!(
        read(&client, &root.join("closed.md")).await,
        "only on disk\n"
    );
    // Outside the workspace is refused, not served — this handler runs
    // unconfined, so the boundary is its own to enforce.
    let escape = read(&client, std::path::Path::new("/etc/hostname")).await;
    assert!(escape.contains("outside the workspace"), "{escape}");

    drop(client);
    drop(probe);
    let _ = responder.join();
}

/// The other direction: the agent hands the CLIENT the new contents
/// (fs/write_text_file) and the IDE applies them. Beyond what the unit
/// tests cover, this proves the exchange completes on the wire and that a
/// refusal comes back as an error the agent can read — a write that
/// silently no-ops is how an agent ends up confidently reporting work it
/// never did.
#[tokio::test(flavor = "multi_thread")]
async fn agent_file_writes_go_through_the_client() {
    use taste_core::ui_probe::{UiProbe, UiReply, UiRequest};

    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();

    // Stand in for the editor: apply the write, the way Editor::buffer_write
    // does for a file with no open tab.
    let probe = UiProbe::new();
    let requests = probe.requests();
    let responder = std::thread::spawn(move || {
        while let Ok((request, reply)) = requests.recv_blocking() {
            let UiRequest::BufferWrite { path, content } = request else {
                let _ = reply.send_blocking(UiReply::Error("unexpected request".into()));
                continue;
            };
            let applied = std::fs::write(&path, &content).map_err(|e| e.to_string());
            let _ = reply.send_blocking(UiReply::BufferWrite(applied));
        }
    });

    let client = AgentClient::spawn_unconfined_with_ui_for_tests(
        fake_agent_spec(),
        root.clone(),
        probe.clone(),
    );
    assert!(matches!(
        next_event(&client).await,
        SessionEvent::Ready { .. }
    ));

    async fn write(client: &AgentClient, path: &std::path::Path, text: &str) -> String {
        client
            .prompt(format!("/write {} {text}", path.display()))
            .unwrap();
        loop {
            match next_event(client).await {
                SessionEvent::Update(SessionUpdate::AgentMessageChunk(chunk)) => {
                    let ContentBlock::Text(text) = &chunk.content else {
                        panic!("expected text content");
                    };
                    return text.text.clone();
                }
                SessionEvent::Closed(error) => panic!("connection closed early: {error:?}"),
                _ => {}
            }
        }
    }

    let target = root.join("written.md");
    assert_eq!(write(&client, &target, "from the agent").await, "OK");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "from the agent",
        "the client applied the agent's write"
    );

    // The mount that used to refuse this is gone; the policy check is what
    // refuses it now, and the agent is told so in words.
    let escape = write(&client, std::path::Path::new("/tmp/escaped.md"), "no").await;
    assert!(
        escape.contains("outside the writable workspace"),
        "{escape}"
    );
    assert!(
        !std::path::Path::new("/tmp/escaped.md").exists(),
        "a refused write must not have happened"
    );

    drop(client);
    drop(probe);
    let _ = responder.join();
}
