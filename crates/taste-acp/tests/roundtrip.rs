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
