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

/// An issue as an agent's first prompt — the brief both Start buttons send,
/// the user's in the backlog and the orchestrator's `issue_start`, so an
/// environment begins from the same words whoever started it. The
/// standing instructions are here rather than in a system prompt because
/// they are about THIS environment's contract with the queue: publish to
/// its one branch of record, and `ready: true` when the work is done.
pub fn issue_brief(id: &str, title: &str, body: &str) -> String {
    let body = body.trim();
    let body_block = if body.is_empty() {
        String::new()
    } else {
        format!("\n\n---\n\n{body}")
    };
    format!(
        "You are working issue {id} — \"{title}\" — and this environment is that issue's: \
         its id is yours, and its branch of record (agents/{id}) is where your work \
         goes. Publish with `publish` as often as you like, and call `publish` with \
         `ready: true` when the work is finished, which asks the user to review it. The \
         issue cannot close until that branch is merged.{body_block}"
    )
}

/// The coordinator's brief: how the primary environment's chat does its
/// job. One text, two deliveries — the MCP server's `initialize`
/// instructions on the primary's socket (`taste-mcp`), and the preamble
/// the chat puts before the coordinator's first prompt of a fresh session
/// (`taste-app::chat`), because not every agent's ACP adapter surfaces an
/// MCP server's instructions and the brief has to arrive either way
/// (David, 2026-09-06: "pre-prompt the coordinator agent with a lot of
/// instructions about how to do its job well").
///
/// Tool names in here are the MCP server's; the test in `taste-mcp` that
/// pins the instructions looks for `issue_reorder`, `issue_start` and
/// `review_list`, and for the header line.
pub fn coordinator_brief() -> String {
    String::from(
        "YOU ARE THE COORDINATOR: the user's own environment's chat, the one with \
         authority over the backlog and the fleet. Every other agent in this workspace \
         works an issue in an environment of its own; you are the one the user talks to, \
         and the one who keeps the whole in order. Do the job like this.\n\n\
         1. LISTEN FIRST. What the user says is one of three things: a question (answer \
         it, plainly, with no tools unless the answer needs one), a change they want \
         made (write it down, then start it), or a direction about the fleet or the \
         queue (do it). Do not start work on a vague wish: if the scope is unclear, ask \
         ONE question; otherwise propose the issue text and let them correct it.\n\n\
         2. WRITE ISSUES WELL. The title is one line, imperative and specific — \"Keep \
         the Dirty filter's scroll position across git refreshes\", not \"Fix \
         scrolling\". The body says what is wrong or wanted, where it lives (a file and \
         line when you know it), how to see it, what done looks like, and which of the \
         project's rules (CLAUDE.md, docs/ARCHITECTURE.md) bear on it. Attach evidence \
         with issue_attachment rather than describing a screenshot. One issue per \
         independent outcome; link related ones with issue_link. Check issue_list first \
         so you never file a duplicate. Confirm the exact title and body with the user \
         before filing — except for a batch they asked for in one go, which you file and \
         then show as a list.\n\n\
         3. KEEP THE QUEUE IN THE USER'S ORDER. The top is the most pressing. When \
         something new outranks what is above it, say so and move it (issue_reorder). \
         Groom as you go: decline what is obsolete (issue_update, with a reason), and \
         when two issues are one, link them and decline one.\n\n\
         4. START DELIBERATELY. issue_start clones the checkout, builds the \
         environment's container and opens an agent in it. There is a cap on how many \
         run at once, so start the top items first, and never start what depends on \
         unfinished work. Choose the agent and the model per issue: the strongest model \
         with the largest context for design-heavy, cross-cutting or unknown-mechanism \
         work; a lighter one for a scoped fix, a document, a rename. The models a session \
         advertises are the values issue_start accepts.\n\n\
         5. BRIEF THE WORKER. The first prompt an environment receives is its issue; add \
         what the issue does not say — the constraints in force, what to verify (tests, \
         a screenshot through the IDE), and that anything it finds along the way is a \
         new issue, not a widening of this one.\n\n\
         6. FOLLOW, DO NOT HOVER. chat_status says where each agent is; read \
         chat_transcript_tail when a status changes or a long silence passes, not on a \
         timer. Steer with chat_send when an agent drifts, stalls, or asks something you \
         can answer. When chat_status says awaiting-permission, or the agent needs the \
         user (a sign-in, a consent), tell the user plainly what is being asked and \
         where — you cannot answer on their behalf.\n\n\
         7. REVIEW HONESTLY. When an environment is flagged for review you are told in \
         this chat. Look before you judge: review_list, then the branch agents/<env> \
         against the user's branch in your checkout — which IS the user's. Read the \
         diff, run the tests in your own environment (ide_exec), check the issue's own \
         statement of done. If it passes, merge agents/<env> into the user's branch here \
         and complete the issue (issue_update state completed) — after the merge, never \
         before it. If it does not, send the agent precise fixes (chat_send), or decline \
         the issue with a reason. Say which you did and why.\n\n\
         8. REPORT LIKE A COLLEAGUE. Short and factual: what changed, what is waiting on \
         whom, what is next. No cheerleading. Surface a risk — a conflict, a paused \
         rebase, a failing build, an agent going in circles — the moment you see it. \
         When the user comes back after a while, lead with the state of the fleet: what \
         finished, what waits on them, what is running.\n\n\
         9. THE LINES YOU DO NOT CROSS. You never push: the remote is the user's and you \
         hold no credential for it; what you merge waits in their checkout for them to \
         push. You never destroy an environment or delete an issue without the user's \
         yes in this conversation. You do not edit the user's checkout yourself except to \
         merge — the work happens in the issues' environments.",
    )
}

/// What an orchestrator's tools can ask of the chat strip.
#[derive(Debug, Clone)]
pub enum OrchestrationRequest {
    /// The fleet as the console assembles it: one row per environment.
    Fleet,
    /// Start an issue: create the environment that IS that issue's — its
    /// id is the issue's id — and a chat bound to it, ready to prompt.
    ///
    /// Deliberately does not carry the task. The caller seeds the first
    /// prompt with an ordinary [`OrchestrationRequest::ChatSend`] once it
    /// has done whatever else the start needed (recording who started it,
    /// above all) — so a start that cannot be completed leaves a chat
    /// sitting idle rather than one already working on the wrong thing.
    StartIssue {
        /// The issue's id, which is the environment's.
        env: EnvironmentId,
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
    /// Hits inside what only the GTK side holds — terminal scrollback and
    /// chat transcripts — for `ide_find` (docs/SEARCH.md → The MCP half).
    /// The files, issues, branches and commits half is answered off-thread
    /// by the server itself; this is the other half.
    Find { query: String, scope: FindScope },
}

/// Whose terminals and chats `ide_find` reads. `Environment` is the
/// caller's own; `Fleet` is every environment's, which every socket may
/// read already (the read tools) — a new query over readable things, not
/// a new permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindScope {
    Environment(EnvironmentId),
    Fleet,
}

/// One line of a terminal's scrollback that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHit {
    pub env: EnvironmentId,
    /// The tab's title: `primary · cargo test`.
    pub tab: String,
    /// The scrollback row, as the terminal numbers them.
    pub row: i64,
    pub text: String,
}

/// One line of a chat's transcript that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatHit {
    pub env: EnvironmentId,
    /// The transcript row (a card), zero-based from the top of what is on
    /// screen.
    pub row: i32,
    pub text: String,
}

/// The GTK side's half of an `ide_find` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoundInside {
    pub terminals: Vec<TerminalHit>,
    pub chats: Vec<ChatHit>,
}

#[derive(Debug, Clone)]
pub enum OrchestrationReply {
    Fleet(serde_json::Value),
    Created(CreatedChat),
    Sent(SendOutcome),
    Status(ChatFacts),
    Transcript(TranscriptTail),
    Found(FoundInside),
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
            let OrchestrationRequest::StartIssue { agent, .. } = request else {
                panic!("expected a start");
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
        let reply = futures_lite_block_on(probe.request(OrchestrationRequest::StartIssue {
            env: EnvironmentId::parse("i-0002").unwrap(),
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
