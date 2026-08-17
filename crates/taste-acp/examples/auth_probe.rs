//! Live probe: does the typed client see the adapter's terminal auth
//! methods, and does the connection survive a signed-out prompt?

use std::time::Duration;

use taste_acp::{builtin_agents, AgentClient, SessionEvent};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let spec = builtin_agents().into_iter().next().expect("claude first");
    let client =
        AgentClient::spawn_unconfined_for_tests(spec, std::path::PathBuf::from("/tmp/auth-probe"));
    std::fs::create_dir_all("/tmp/auth-probe").unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(90), client.events.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        match event {
            SessionEvent::Ready { restored, .. } => {
                println!("READY restored={restored}");
                client.prompt("hi").unwrap();
            }
            SessionEvent::AuthRequired { methods } => {
                println!("AUTH-REQUIRED methods={}", methods.len());
                for method in &methods {
                    println!("  method: {:?} name={}", method.id(), method.name());
                }
            }
            SessionEvent::PromptFailed { message } => {
                println!("PROMPT-FAILED: {message}");
                // Prove the connection is still alive with a mode switch.
                client
                    .set_mode(agent_client_protocol::schema::v1::SessionModeId::new(
                        "plan",
                    ))
                    .unwrap();
            }
            SessionEvent::ModeChangeFailed { message, .. } => {
                println!("MODE-FAILED (connection alive): {message}");
                break;
            }
            SessionEvent::Update(update) => {
                let text = format!("{update:?}");
                if text.contains("CurrentModeUpdate") {
                    println!("MODE-OK (connection alive)");
                    break;
                }
            }
            SessionEvent::TurnEnded { .. } => println!("TURN-ENDED"),
            SessionEvent::Closed(e) => {
                println!("CLOSED: {e:?}");
                break;
            }
            _ => {}
        }
    }
}
