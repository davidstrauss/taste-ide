//! Live probe against the real adapter: fresh session, then try mode
//! switches through taste's own client. Run inside the devcontainer with
//! the agent home available.

use std::time::Duration;

use taste_acp::{builtin_agents, AgentClient, SessionEvent};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let spec = builtin_agents().into_iter().next().expect("claude first");
    let client = AgentClient::spawn_unconfined_for_tests(
        spec,
        std::path::PathBuf::from("/workspaces/taste-ide"),
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(120), client.events.recv())
            .await
            .expect("timeout")
            .expect("closed");
        match event {
            SessionEvent::Ready {
                modes, restored, ..
            } => {
                let modes = modes.expect("modes");
                println!(
                    "READY restored={restored} modes={:?} current={}",
                    modes
                        .available_modes
                        .iter()
                        .map(|m| m.id.to_string())
                        .collect::<Vec<_>>(),
                    modes.current_mode_id
                );
                let auto = modes
                    .available_modes
                    .iter()
                    .find(|m| m.id.to_string() == "auto")
                    .map(|m| m.id.clone())
                    .expect("auto id");
                client.set_mode(auto).unwrap();
            }
            SessionEvent::ModeChangeFailed { mode, message } => {
                println!("MODE-FAILED mode={mode} message={message}");
                break;
            }
            SessionEvent::Update(update) => {
                let text = format!("{update:?}");
                if text.contains("CurrentModeUpdate") {
                    println!("MODE-UPDATE OK: {}", &text[..text.len().min(120)]);
                    break;
                }
            }
            SessionEvent::Closed(e) => {
                println!("CLOSED: {e:?}");
                break;
            }
            _ => {}
        }
    }
}
