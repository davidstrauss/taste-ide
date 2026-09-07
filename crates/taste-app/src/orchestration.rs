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

/// The fleet, as rows, for `issue_list` and `issue_status`. The console
/// assembles it (it is the one place that knows all six sources), so this
/// is a getter the window installs rather than a second derivation.
pub type FleetLookup = Rc<dyn Fn() -> serde_json::Value>;

/// Hits inside terminals and chats for `ide_find`: the console and the
/// chat strip answer, and the window composes the two, so this is a getter
/// the window installs rather than a third owner of either pane.
pub type FindInside = Rc<
    dyn Fn(
        &taste_core::search::Query,
        &taste_core::orchestration::FindScope,
    ) -> taste_core::orchestration::FoundInside,
>;

/// Start answering orchestration requests on the main thread.
pub fn attach(
    workspace: &Workspace,
    chats: Rc<Chats>,
    environments: std::sync::Arc<taste_devcontainer::EnvironmentRegistry>,
    fleet: FleetLookup,
    find: FindInside,
) {
    let requests = workspace.orchestration.requests();
    glib::spawn_future_local(async move {
        while let Ok((request, reply)) = requests.recv().await {
            match request {
                OrchestrationRequest::Fleet => {
                    let _ = reply.send(OrchestrationReply::Fleet(fleet())).await;
                }
                OrchestrationRequest::StartIssue {
                    env: id,
                    agent,
                    model,
                } => {
                    if let Some(reopens) = chats.allowance_exhausted() {
                        let _ = reply
                            .send(OrchestrationReply::Error(exhausted(&reopens)))
                            .await;
                        continue;
                    }
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
                    // The environment is the issue's and takes its id;
                    // there is nothing to generate.
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
                    if let Some(reopens) = chats.allowance_exhausted() {
                        let _ = reply
                            .send(OrchestrationReply::Error(exhausted(&reopens)))
                            .await;
                        continue;
                    }
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
                OrchestrationRequest::Find { query, scope } => {
                    let query = taste_core::search::Query::new(&query);
                    let _ = reply.send(OrchestrationReply::Found(find(&query, &scope))).await;
                }
            }
        }
    });
}

/// The refusal every IDE-started prompt gets while the allowance is
/// exhausted: nothing new runs until the user says so.
fn exhausted(reopens: &str) -> String {
    format!(
        "the account's session allowance is exhausted and the API is refusing turns \
         (it reopens {reopens}). The IDE starts nothing new on its own until the user \
         resumes — tell them, and stop here."
    )
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
         one there yet); issue_list shows those too."
    )
}
