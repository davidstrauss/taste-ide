//! The orchestration tools: definitions and result shaping.
//!
//! These seven tools are **execution authority**. `chat_create` spawns an
//! agent that will run code in a container; `chat_send` puts words in its
//! mouth. So they are served on exactly one socket — the orchestrator
//! chat's environment — and are absent from `tools/list` everywhere else,
//! the same way `publish_branch` is absent from the primary's. Presence,
//! not refusal: a tool an agent can see is a tool it will spend turns
//! trying, and the honest statement of "you are not the orchestrator" is
//! that these do not exist for you.
//!
//! Why the *environment* socket and not the chat: the per-environment
//! sockets tell environments apart, not chats. Every chat with no
//! environment of its own shares the primary's socket, so serving these
//! there would hand orchestration to every unbound chat in the workspace,
//! including ones the user opened for something else entirely. That is
//! why the designation UI insists on a bound chat and offers to create the
//! environment in the same gesture.
//!
//! What is deliberately NOT here: any way to answer a sub-chat's
//! permission prompt. Those surface in the sub-chat's own tab, to the
//! user. `chat_status` reporting `awaiting-permission` is how an
//! orchestrator learns to say "this one needs you" instead.

use serde_json::{json, Value};
use taste_core::orchestration::{ChatFacts, TranscriptTail};
use taste_core::CappedOutput;

/// Transcript tail budget, in bytes of rendered text. Generous enough for
/// a real exchange, small enough that a runaway tool dump cannot turn one
/// supervision call into a megabyte of the orchestrator's context.
pub(crate) const TRANSCRIPT_BUDGET: usize = 24 * 1024;

/// Lines a `chat_transcript_tail` returns when the caller does not say.
pub(crate) const TRANSCRIPT_DEFAULT_LINES: usize = 40;
/// ...and the most it will return however loudly they ask.
pub(crate) const TRANSCRIPT_MAX_LINES: usize = 200;

/// The tools an orchestrator's socket serves, on top of everything every
/// socket serves.
pub(crate) fn tools() -> Vec<Value> {
    let empty = json!({ "type": "object", "properties": {} });
    let chat_arg = |what: &str| {
        json!({
            "type": "object",
            "properties": {
                "chat": { "type": "string", "description": what }
            },
            "required": ["chat"]
        })
    };
    vec![
        crate::protocol::tool(
            "env_list",
            "Every environment in this workspace, as the user's own fleet view sees \
             it: mode (container or safe), container state, the chat bound to it and \
             whether that chat is working, its branch, how many branches it has \
             published, whether it holds unpublished work, disk footprint and token \
             spend. This is your map — read it before creating anything, because \
             every environment is a clone, a container and a share of the user's \
             subscription.",
            empty.clone(),
        ),
        crate::protocol::tool(
            "env_status",
            "One environment's row from env_list, by name. Use it to watch a \
             sub-agent's environment come up, or to check for unpublished work \
             before you suggest destroying it.",
            json!({
                "type": "object",
                "properties": {
                    "env": { "type": "string", "description": "environment id, e.g. calm-3" }
                },
                "required": ["env"]
            }),
        ),
        crate::protocol::tool(
            "chat_create",
            "Delegate: create an environment (a fresh clone of the user's checkout), \
             open a chat bound to it, and give that chat its first task. Returns the \
             chat id, which IS its environment id. \
             The new chat is an ordinary tab the user can read and take over at any \
             time. Its container is NOT started — a fresh environment is in safe mode \
             until the user starts it, so the sub-agent can read, write and think but \
             cannot run commands yet; say so when the work needs a build. \
             You cannot answer its permission prompts: those go to the user, and \
             chat_status reports awaiting-permission so you can tell them. \
             Pass `issue` to dispatch an open issue: the new environment claims it \
             first, and the claim failing (someone else holds it) means nothing is \
             created and nothing is prompted.",
            json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "the sub-agent's first prompt: what to do, in enough detail to start without asking you"
                    },
                    "agent": {
                        "type": "string",
                        "description": "agent registry id (default: the IDE's default agent)"
                    },
                    "model": {
                        "type": "string",
                        "description": "session config value id for the model, e.g. a smaller model for a mechanical task. Unknown ids are refused with the list the agent advertises."
                    },
                    "issue": {
                        "type": "string",
                        "description": "issue id (e.g. i-0003) to claim for the new environment and hand to it with the task"
                    }
                },
                "required": ["task"]
            }),
        ),
        crate::protocol::tool(
            "chat_send",
            "Send a prompt to a chat you created. Mid-turn it queues — the session \
             layer runs it when the current turn ends, and the result says which \
             happened. This is a message to another agent, not a command: it will \
             answer in its own tab, where the user can see both halves.",
            json!({
                "type": "object",
                "properties": {
                    "chat": { "type": "string", "description": "chat id (its environment id)" },
                    "text": { "type": "string", "description": "the prompt" }
                },
                "required": ["chat", "text"]
            }),
        ),
        crate::protocol::tool(
            "chat_status",
            "What one chat is doing: idle, streaming, awaiting-permission (the USER \
             must answer — tell them), disconnected, or starting. Plus its agent, \
             model, session id, how long it has been quiet, turns completed and the \
             token usage its agent reports. Poll this instead of guessing from \
             silence.",
            chat_arg("chat id (its environment id)"),
        ),
        crate::protocol::tool(
            "chat_transcript_tail",
            "The recent transcript of a chat, as plain text: who said what, newest \
             last. Capped at both ends and honest about it — the pane keeps a bounded \
             mirror, so lines it has forgotten are counted rather than invented. Read \
             this before deciding a sub-agent is stuck.",
            json!({
                "type": "object",
                "properties": {
                    "chat": { "type": "string", "description": "chat id (its environment id)" },
                    "max": {
                        "type": "integer",
                        "description": format!("lines to return (default {TRANSCRIPT_DEFAULT_LINES}, max {TRANSCRIPT_MAX_LINES})")
                    }
                },
                "required": ["chat"]
            }),
        ),
        crate::protocol::tool(
            "review_list",
            "Where every environment stands for review: its branch of record \
             (agents/<env> — each environment has exactly one, and publishing moves \
             it), whether that branch is already merged into the user's current \
             branch, and its review state — working, flagged-for-review, merged or \
             rejected. \
             An environment flagged for review is DONE and its container is stopped; \
             one the user has merged or rejected is safe to destroy. This is also what \
             integration works from: pull the branches into your own clone with \
             update_from_main, merge and test there, and publish the combined result \
             as your own environment's branch.",
            json!({
                "type": "object",
                "properties": {
                    "flagged_only": {
                        "type": "boolean",
                        "description": "only environments waiting on the user's review (default: all)"
                    }
                }
            }),
        ),
    ]
}

/// One chat's state, as `chat_status` and `chat_create` report it.
pub(crate) fn chat_facts_json(facts: &ChatFacts) -> Value {
    json!({
        "chat": facts.chat.as_str(),
        "environment": facts.chat.as_str(),
        "agent": facts.agent,
        "model": facts.model,
        "session": facts.session,
        "state": facts.state.as_str(),
        "idle_for_seconds": facts.idle_for_secs,
        "turns": facts.turns,
        "orchestrator": facts.orchestrator,
        "usage": facts.usage.as_ref().map(|usage| json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "context_used": usage.context_used,
            "context_limit": usage.context_limit,
        })),
        "note": match facts.state {
            taste_core::orchestration::ChatState::AwaitingPermission =>
                "this chat is waiting on a PERMISSION PROMPT that only the user can \
                 answer — tell them which chat and what it is asking",
            taste_core::orchestration::ChatState::Disconnected =>
                "no agent process; the pane reconnects on its own, and a chat that \
                 stays here needs a person",
            _ => "",
        },
    })
}

/// A transcript tail as text, capped by lines up front and by bytes at the
/// end. Both elisions are reported: an orchestrator that cannot tell a
/// quiet agent from a truncated view will draw the wrong conclusion.
pub(crate) fn transcript_json(chat: &str, tail: &TranscriptTail) -> Value {
    let mut capped = CappedOutput::with_budget(TRANSCRIPT_BUDGET);
    for line in &tail.lines {
        capped.push(format!("[{}] {}\n", line.speaker, line.text).as_bytes());
    }
    json!({
        "chat": chat,
        "lines": tail.lines.len(),
        "elided_by_max": tail.elided_by_the_cap,
        "forgotten_by_the_pane": tail.dropped_by_the_pane,
        "truncated": capped.truncated(),
        "transcript": capped.render(),
        "note": "a bounded mirror of what the tab shows; the full conversation lives \
                 with the agent, and the user can read it in that tab",
    })
}
