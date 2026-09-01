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
//! - **It does not decide how an environment is made.**
//!   [`crate::environments::create`] does, the same call the panel's own
//!   New Environment makes, because an environment an agent created must be
//!   an ordinary environment in every respect — including how it was made.
//!   `chat_create` is that call followed by starting an agent in the
//!   result, which is exactly what a person does by hand.

use std::rc::Rc;

use gtk::glib;
use taste_core::orchestration::{OrchestrationReply, OrchestrationRequest};
use taste_core::Workspace;

use crate::chats::Chats;

/// The fleet, as rows, for `env_list` and `env_status`. The console
/// assembles it (it is the one place that knows all six sources), so this
/// is a getter the window installs rather than a second derivation.
pub type FleetLookup = Rc<dyn Fn() -> serde_json::Value>;

/// Start answering orchestration requests on the main thread.
pub fn attach(
    workspace: &Workspace,
    chats: Rc<Chats>,
    environments: std::sync::Arc<taste_devcontainer::EnvironmentRegistry>,
    fleet: FleetLookup,
) {
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
                    let answer = move |answer: OrchestrationReply| {
                        glib::spawn_future_local(async move {
                            let _ = reply.send(answer).await;
                        });
                    };
                    // An environment, then its chat — in that order,
                    // because a chat is an environment's conversation and
                    // there is nowhere to put one until the clone exists.
                    let id = match crate::environments::next_id(&environments) {
                        Ok(id) => id,
                        Err(e) => {
                            answer(OrchestrationReply::Error(format!("{e:#}")));
                            continue;
                        }
                    };
                    let chats = chats.clone();
                    crate::environments::create(
                        environments.clone(),
                        id,
                        Box::new(move |outcome| match outcome {
                            Err(reason) => answer(OrchestrationReply::Error(format!(
                                "the environment could not be created: {reason}"
                            ))),
                            Ok(env) => chats.create_orchestrated(
                                env,
                                agent,
                                model,
                                Box::new(move |outcome| {
                                    answer(match outcome {
                                        Ok(created) => OrchestrationReply::Created(created),
                                        Err(message) => OrchestrationReply::Error(message),
                                    })
                                }),
                            ),
                        }),
                    );
                }
                OrchestrationRequest::ChatSend { chat, text } => {
                    let answer = match chats.pane_for(&chat) {
                        None => OrchestrationReply::Error(no_such_chat(&chats, &chat)),
                        Some(pane) => match pane.submit_prompt(text) {
                            Ok(outcome) => OrchestrationReply::Sent(outcome),
                            Err(message) => OrchestrationReply::Error(message),
                        },
                    };
                    let _ = reply.send(answer).await;
                }
                OrchestrationRequest::ChatStatus { chat } => {
                    let answer = match chats.pane_for(&chat) {
                        None => OrchestrationReply::Error(no_such_chat(&chats, &chat)),
                        Some(pane) => OrchestrationReply::Status(pane.chat_facts(chat)),
                    };
                    let _ = reply.send(answer).await;
                }
                OrchestrationRequest::ChatTranscript { chat, max } => {
                    let answer = match chats.pane_for(&chat) {
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
fn no_such_chat(chats: &Rc<Chats>, chat: &taste_core::environment::EnvironmentId) -> String {
    let live = chats.bound_environments();
    let known: Vec<&str> = live.iter().map(|env| env.as_str()).collect();
    format!(
        "no chat is working in {chat} — the environments with a chat in them are \
         {known:?}. An environment can exist with no agent in it (nobody has started \
         one there yet); env_list shows those too."
    )
}
