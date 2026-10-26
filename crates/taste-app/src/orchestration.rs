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
    let root = workspace.root().to_path_buf();
    let events = workspace.events.clone();
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
                    host,
                    brief,
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
                    let answer = answer_or_finish(
                        reply.clone(),
                        chats.clone(),
                        root.clone(),
                        events.clone(),
                        id.clone(),
                        brief,
                    );
                    // An environment, then its chat — in that order,
                    // because a chat is an environment's conversation and
                    // there is nowhere to put one until the clone exists.
                    // The environment is the issue's and takes its id;
                    // there is nothing to generate.
                    let chats = chats.clone();
                    crate::environments::create(
                        environments.clone(),
                        id,
                        host,
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
                // A stranded start's second half: the environment exists,
                // so only its chat is opened.
                OrchestrationRequest::FinishIssue { env, model, brief } => {
                    if let Some(reopens) = chats.allowance_exhausted() {
                        let _ = reply
                            .send(OrchestrationReply::Error(exhausted(&reopens)))
                            .await;
                        continue;
                    }
                    let answer = answer_or_finish(
                        reply.clone(),
                        chats.clone(),
                        root.clone(),
                        events.clone(),
                        env.clone(),
                        brief,
                    );
                    chats.create_orchestrated(
                        env,
                        None,
                        model,
                        Box::new(move |outcome| {
                            answer(match outcome {
                                Ok(created) => OrchestrationReply::Created(created),
                                Err(message) => OrchestrationReply::Error(message),
                            })
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
                // The hook's: what waits behind the chat's turn, taken from
                // its session for the agent to read within the turn. Off this
                // loop, since the session answers after its own queue.
                OrchestrationRequest::TakeWaiting { chat } => {
                    // Nobody asking any more — the hook's time ran out while
                    // this thread was busy: nothing is taken.
                    if reply.is_closed() {
                        continue;
                    }
                    let pane = chats.pane_for(&chat);
                    let taking = pane.as_ref().and_then(|pane| pane.take_waiting_for_hook());
                    glib::spawn_future_local(async move {
                        let taken = match taking {
                            Some(taking) => taking.await.unwrap_or_default(),
                            None => Vec::new(),
                        };
                        let texts = taken.iter().map(|(_, text)| text.clone()).collect();
                        // Taken, and the hook gone before it had them: sent
                        // again, after the turn, rather than read by nobody.
                        if reply.send(OrchestrationReply::Taken(texts)).await.is_err() {
                            if let Some(pane) = pane {
                                pane.give_back_taken(taken);
                            }
                        }
                    });
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
                    let _ = reply
                        .send(OrchestrationReply::Found(find(&query, &scope)))
                        .await;
                }
            }
        }
    });
}

/// How a start's answer is given: to the caller, while it waits — or, once
/// it has stopped waiting, by finishing the start here, as the caller would
/// have on that answer. `issue_start` waits two minutes for the
/// environment and its chat, and php-src's clone took all of them; the
/// tool gave up, the strip went on and made the chat, and its agent sat
/// made and idle, never told the issue, with the issue never recorded as
/// started (2026-10-26). So a `Created` nobody receives is followed through
/// here: the start recorded, then the brief sent — in that order, so a
/// start that cannot be recorded leaves the chat idle rather than working
/// on an issue nobody knows is taken.
fn answer_or_finish(
    reply: async_channel::Sender<OrchestrationReply>,
    chats: Rc<Chats>,
    root: std::path::PathBuf,
    events: taste_core::EventBus,
    issue: taste_core::environment::EnvironmentId,
    brief: String,
) -> impl FnOnce(OrchestrationReply) + 'static {
    move |answer: OrchestrationReply| {
        glib::spawn_future_local(async move {
            let Err(async_channel::SendError(OrchestrationReply::Created(created))) =
                reply.send(answer).await
            else {
                return;
            };
            tracing::info!(
                "{issue}: issue_start stopped waiting before its chat was made; recording the \
                 start and sending the brief here"
            );
            let agent = created.agent.clone();
            let model = created.model.clone().or(created.model_pending.clone());
            let id = issue.to_string();
            let recorded = crate::runtime::runtime()
                .spawn_blocking(move || {
                    let git = taste_git::GitWorkspace::discover(&root)
                        .ok_or_else(|| anyhow::anyhow!("this workspace is not a git repository"))?;
                    git.issue_start_with(
                        &id,
                        &git.starter_identity(),
                        Some(&agent),
                        model.as_deref(),
                    )
                    .map(|_| ())
                })
                .await;
            match recorded {
                Ok(Ok(())) => events.publish(taste_core::Event::GitStatusChanged),
                Ok(Err(e)) => {
                    tracing::warn!(
                        "{issue}: the start was not recorded, so no brief was sent: {e:#}"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!("{issue}: recording the start did not finish: {e}");
                    return;
                }
            }
            match chats
                .pane_for(&created.chat)
                .map(|pane| pane.submit_prompt(brief))
            {
                Some(Ok(_)) => tracing::info!("{issue}: the brief went to its agent"),
                Some(Err(e)) => tracing::warn!("{issue}: the brief was not taken: {e}"),
                None => tracing::warn!("{issue}: no chat to send the brief to"),
            }
        });
    }
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
