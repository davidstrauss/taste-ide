//! The GTK side of `taste_core::orchestration`: the chat strip, answering
//! an orchestrator's tools.
//!
//! Same shape as [`crate::ui_probe`] — a `glib::spawn_future_local` loop
//! draining requests the MCP server sent from tokio — and the same rule
//! about what crosses: plain data out, never a pane.
//!
//! Two things this module deliberately does not do:
//!
//! - **It does not decide who may ask.** By the time a request arrives,
//!   the server has already established that it came in on the
//!   orchestrator's socket. This end would have no way to tell: the probe
//!   carries no caller, exactly as the MCP wire carries no environment.
//! - **It does not create environments.** [`crate::chat_tabs::ChatTabs`]
//!   does, through the same path the user's own "Give This Chat Its Own
//!   Environment" takes, because a chat created by an agent must be an
//!   ordinary tab in every respect — including how it was made.

use std::rc::Rc;

use gtk::glib;
use taste_core::orchestration::{OrchestrationReply, OrchestrationRequest};
use taste_core::Workspace;

use crate::chat_tabs::ChatTabs;

/// The fleet, as rows, for `env_list` and `env_status`. The console
/// assembles it (it is the one place that knows all six sources), so this
/// is a getter the window installs rather than a second derivation.
pub type FleetLookup = Rc<dyn Fn() -> serde_json::Value>;

/// Start answering orchestration requests on the main thread.
pub fn attach(workspace: &Workspace, chats: Rc<ChatTabs>, fleet: FleetLookup) {
    let requests = workspace.orchestration.requests();
    glib::spawn_future_local(async move {
        while let Ok((request, reply)) = requests.recv().await {
            match request {
                OrchestrationRequest::Fleet => {
                    let _ = reply.send(OrchestrationReply::Fleet(fleet())).await;
                }
                OrchestrationRequest::ChatCreate { agent, model } => {
                    // The only request answered off this loop: creating a
                    // chat clones a repository and waits for an agent
                    // session to come up, and a second orchestration call
                    // queued behind it would wait for all of that. The
                    // reply channel is what keeps them in step.
                    let reply = reply.clone();
                    chats.clone().create_orchestrated(
                        agent,
                        model,
                        Box::new(move |outcome| {
                            let answer = match outcome {
                                Ok(created) => OrchestrationReply::Created(created),
                                Err(message) => OrchestrationReply::Error(message),
                            };
                            glib::spawn_future_local(async move {
                                let _ = reply.send(answer).await;
                            });
                        }),
                    );
                }
                OrchestrationRequest::ChatSend { chat, text } => {
                    let answer = match chats.pane_for_environment(&chat) {
                        None => OrchestrationReply::Error(no_such_chat(&chats, &chat)),
                        Some(pane) => match pane.submit_prompt(text) {
                            Ok(outcome) => OrchestrationReply::Sent(outcome),
                            Err(message) => OrchestrationReply::Error(message),
                        },
                    };
                    let _ = reply.send(answer).await;
                }
                OrchestrationRequest::ChatStatus { chat } => {
                    let answer = match chats.pane_for_environment(&chat) {
                        None => OrchestrationReply::Error(no_such_chat(&chats, &chat)),
                        Some(pane) => OrchestrationReply::Status(pane.chat_facts(chat)),
                    };
                    let _ = reply.send(answer).await;
                }
                OrchestrationRequest::ChatTranscript { chat, max } => {
                    let answer = match chats.pane_for_environment(&chat) {
                        None => OrchestrationReply::Error(no_such_chat(&chats, &chat)),
                        Some(pane) => OrchestrationReply::Transcript(pane.transcript_tail(max)),
                    };
                    let _ = reply.send(answer).await;
                }
            }
        }
    });
}

/// A chat id nothing answers to — with the ids that do, because the
/// difference between "it finished" and "you spelled it wrong" is not
/// something an orchestrator can work out from silence.
fn no_such_chat(chats: &Rc<ChatTabs>, chat: &taste_core::environment::EnvironmentId) -> String {
    let live = chats.bound_environments();
    let known: Vec<&str> = live.iter().map(|env| env.as_str()).collect();
    format!(
        "no chat is working in {chat} — the chats with environments of their own are \
         {known:?}. A chat whose tab the user closed is gone; its environment (and its \
         work) is not, and env_list still shows it."
    )
}
