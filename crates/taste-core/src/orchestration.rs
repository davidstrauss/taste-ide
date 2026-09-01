//! The orchestrator's questions for the GTK main thread.
//!
//! An orchestrator chat drives *other chats*: it creates them, prompts
//! them, asks how they are doing and reads what they said. Every one of
//! those lives in the chat strip, on the GTK main thread; the tools that
//! ask are served from tokio by the MCP server. This is the request/reply
//! seam between them, and it is deliberately the same shape as
//! [`crate::ui_probe`] — tokio sends a [`OrchestrationRequest`] with a
//! reply channel, the app drains [`OrchestrationProbe::requests`] with
//! `glib::spawn_future_local` and answers with GTK-free types.
//!
//! Two things are worth saying about what does NOT cross here.
//!
//! - **No widget, no `Rc`, no agent handle.** The replies are plain data:
//!   ids, states, counters, lines of text. The chat strip stays the only
//!   owner of a chat, so a tool can never end up holding one.
//! - **No permission answer.** There is no request variant for approving
//!   a sub-chat's permission prompt, because the orchestrator may not
//!   answer for the user. [`ChatState::AwaitingPermission`] is how it
//!   learns to say so.
//!
//! The fleet reply is [`serde_json::Value`] rather than a struct, for the
//! same reason [`crate::ui_probe::UiReply::Geometry`] is: the rows are
//! assembled in the app (from the six places an environment's facts live)
//! and already have a published shape — the one the fleet varlink socket
//! serves. Re-declaring it here would be a second copy to keep in step.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::environment::EnvironmentId;

/// A chat, as the orchestration tools address it: **by its environment**.
///
/// Orchestrated chats are created with an environment of their own and
/// bound to it for life, so the binding is a name that already exists,
/// that the fleet view shows, that the user can say out loud, and that
/// survives a restart — where a tab ordinal is none of those. Unbound
/// chats are not addressable, and that is not a gap: they all share the
/// primary environment, so "the primary chat" names no particular
/// conversation.
pub type ChatId = EnvironmentId;

/// What an orchestrator's tools can ask of the chat strip.
#[derive(Debug, Clone)]
pub enum OrchestrationRequest {
    /// The fleet as the console assembles it: one row per environment.
    Fleet,
    /// Create an environment, and a chat bound to it, ready to prompt.
    ///
    /// Deliberately does not carry the task. The caller seeds the first
    /// prompt with an ordinary [`OrchestrationRequest::ChatSend`] once it
    /// has done whatever else the dispatch needed (claiming an issue,
    /// above all) — so a dispatch that cannot be completed leaves a chat
    /// sitting idle rather than one already working on the wrong thing.
    ChatCreate {
        /// Agent registry id; `None` takes the IDE's default agent.
        agent: Option<String>,
        /// Session config *value* id for the model; `None` follows the
        /// agent's default. Validated against what the session actually
        /// advertises once it is ready.
        model: Option<String>,
    },
    /// Prompt a chat. Mid-turn sends queue, as they do from the composer.
    ChatSend { chat: ChatId, text: String },
    /// One chat's state, without touching it.
    ChatStatus { chat: ChatId },
    /// The tail of a chat's transcript, as text.
    ChatTranscript { chat: ChatId, max: usize },
}

#[derive(Debug, Clone)]
pub enum OrchestrationReply {
    Fleet(serde_json::Value),
    Created(CreatedChat),
    Sent(SendOutcome),
    Status(ChatFacts),
    Transcript(TranscriptTail),
    /// The app refused, and why. Honest refusals travel this way rather
    /// than as a panic or an empty success.
    Error(String),
}

/// A chat that now exists, with an environment of its own behind it.
#[derive(Debug, Clone)]
pub struct CreatedChat {
    pub chat: ChatId,
    /// The agent actually spawned (the default, when none was asked for).
    pub agent: String,
    /// The model config value in force, when the session advertises one.
    pub model: Option<String>,
    /// What the caller should know that the ids do not say — above all
    /// that the environment's container is not running yet.
    pub note: String,
}

/// What became of a prompt.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    /// The chat was mid-turn, so the session layer queued this prompt and
    /// it starts when the current turn ends.
    pub queued: bool,
}

/// What a chat is doing, as five honest answers.
///
/// `Starting` is not in the orchestrator's vocabulary by accident: a chat
/// whose process is up but whose session has not reached `Ready` is
/// neither disconnected nor working, and calling it either would send the
/// orchestrator down the wrong path — retrying a dispatch, or waiting for
/// a turn that has not begun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatState {
    /// A process is up; the ACP session is not ready yet.
    Starting,
    Idle,
    /// A turn is in flight.
    Streaming,
    /// The agent asked the user for permission and nobody has answered.
    /// **The orchestrator cannot answer it** — tell the user.
    AwaitingPermission,
    /// No agent process. The pane reconnects on its own; a chat that
    /// stays here needs a person.
    Disconnected,
}

impl ChatState {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatState::Starting => "starting",
            ChatState::Idle => "idle",
            ChatState::Streaming => "streaming",
            ChatState::AwaitingPermission => "awaiting-permission",
            ChatState::Disconnected => "disconnected",
        }
    }
}

/// Session token usage as the agent itself reports it. Nothing here is
/// inferred from a model name — an unreported figure is absent, not
/// guessed.
#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Tokens currently in context, per the agent's own usage updates.
    pub context_used: u64,
    pub context_limit: u64,
}

/// One chat, as the orchestrator observes it.
#[derive(Debug, Clone)]
pub struct ChatFacts {
    pub chat: ChatId,
    pub agent: String,
    pub model: Option<String>,
    /// The ACP session id, once there is one.
    pub session: Option<String>,
    pub state: ChatState,
    /// Seconds since anything happened in this chat (a prompt, a chunk, a
    /// turn ending). `None` before anything has.
    pub idle_for_secs: Option<u64>,
    /// Turns completed in this session.
    pub turns: u64,
    pub usage: Option<UsageSummary>,
    /// True when this chat is the orchestrator itself.
    pub orchestrator: bool,
}

/// One line of a chat's plain-text mirror of its transcript.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    /// `you`, `agent`, `tool` or `note` — who put this line there.
    pub speaker: &'static str,
    pub text: String,
    /// Unix seconds.
    pub at: u64,
}

/// A chat's recent transcript, capped at both ends and honest about it.
#[derive(Debug, Clone, Default)]
pub struct TranscriptTail {
    pub lines: Vec<TranscriptLine>,
    /// Lines the pane has already forgotten (its mirror is capped).
    pub dropped_by_the_pane: u64,
    /// Lines dropped to honour the caller's `max`.
    pub elided_by_the_cap: u64,
}

type Envelope = (
    OrchestrationRequest,
    async_channel::Sender<OrchestrationReply>,
);

/// Cloneable handle carried on the [`crate::Workspace`], exactly as
/// [`crate::ui_probe::UiProbe`] is.
#[derive(Clone)]
pub struct OrchestrationProbe {
    tx: async_channel::Sender<Envelope>,
    rx: async_channel::Receiver<Envelope>,
    /// Set once the chat strip starts draining. Requests before that fail
    /// fast rather than hanging a tool call against a headless workspace.
    attached: Arc<AtomicBool>,
}

impl Default for OrchestrationProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl OrchestrationProbe {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            tx,
            rx,
            attached: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The app's end. Calling this declares "a chat strip is listening".
    pub fn requests(&self) -> async_channel::Receiver<Envelope> {
        self.attached.store(true, Ordering::Release);
        self.rx.clone()
    }

    /// Ask the chat strip and await its answer. Callers add their own
    /// timeout — a wedged main thread must show up as a tool error, not a
    /// hang.
    pub async fn request(&self, request: OrchestrationRequest) -> Result<OrchestrationReply> {
        if !self.attached.load(Ordering::Acquire) {
            anyhow::bail!("no chat strip is attached to this workspace");
        }
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.tx
            .send((request, reply_tx))
            .await
            .map_err(|_| anyhow::anyhow!("orchestration channel closed"))?;
        reply_rx
            .recv()
            .await
            .map_err(|_| anyhow::anyhow!("the chat strip dropped the request without answering"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattached_probe_fails_fast() {
        let probe = OrchestrationProbe::new();
        let result = futures_lite_block_on(probe.request(OrchestrationRequest::Fleet));
        assert!(
            result.is_err(),
            "a tool call must not hang waiting for a UI that is not there"
        );
    }

    #[test]
    fn a_creation_round_trips() {
        let probe = OrchestrationProbe::new();
        let requests = probe.requests();
        let responder = std::thread::spawn(move || {
            let (request, reply) = requests.recv_blocking().unwrap();
            let OrchestrationRequest::ChatCreate { agent, .. } = request else {
                panic!("expected a creation");
            };
            reply
                .send_blocking(OrchestrationReply::Created(CreatedChat {
                    chat: EnvironmentId::parse("calm-2").unwrap(),
                    agent: agent.unwrap_or_else(|| "claude".into()),
                    model: None,
                    note: "container not started".into(),
                }))
                .unwrap();
        });
        let reply = futures_lite_block_on(probe.request(OrchestrationRequest::ChatCreate {
            agent: Some("claude".into()),
            model: None,
        }))
        .unwrap();
        match reply {
            OrchestrationReply::Created(created) => assert_eq!(created.chat.as_str(), "calm-2"),
            other => panic!("unexpected reply: {other:?}"),
        }
        responder.join().unwrap();
    }

    /// A minimal block_on (park/unpark): these futures only await channels.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        struct Unparker(std::thread::Thread);
        impl std::task::Wake for Unparker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::from(Arc::new(Unparker(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }
}
