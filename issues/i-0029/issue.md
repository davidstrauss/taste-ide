---
title: A model chosen at issue_start is lost when the first agent spawn dies, so the chat runs on the default and says so to nobody
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: opus
created: 2026-09-16T01:30:09Z
updated: 2026-09-16T02:28:14Z
labels: bug, orchestration, chat, models
---

**What was seen, 2026-09-16, i-0028.** The coordinator called `issue_start` with `model: "fable"`. The call was refused with the i-0011 message (the first prompt lost because the agent spawn beat its container), but the environment was created. `chat_status` for i-0028 then reported, in order:

- with no agent process yet: `"model": "fable"`, `"state": "disconnected"`, `"session": null`;
- after the retried spawn came up and the brief was hand-delivered: `"model": null`, `"state": "streaming"`, a real session id.

So the value was held on the pane and then dropped when the session actually came up. For comparison, i-0026 was started on 2026-09-13 with `model: "sonnet"` and `chat_status` has reported `"model": "sonnet"` on every read since, including while awaiting permission with zero turns. The difference between the two starts is that i-0028's first spawn died (`connection closed (Process exited with exit status: 255: … container state improper)`, app log 01:27:48) and was retried sixteen seconds later.

**What is probably happening.** A model is applied to the agent as ACP session config after the session exists. The first spawn never reached a session, so the pending choice was never applied; the retry path spawns again with `resume=none` and does not re-apply what the pane was holding. `Chats::create` (`crates/taste-app/src/chats.rs`, around line 960 onward, the `wanted` model check and `set_model_value`) is where the value is held and checked; the reconnect path in `chat.rs` is where it is not re-applied. Confirm before fixing: it may instead be that `fable` is not a value this agent advertises and the check that should have refused it never ran because the spawn died first — `issue_start`'s own description promises "unknown ids are refused with the list the agent advertises", and here nothing was refused and nothing was said.

**Why it matters.** The coordinator picks a model per issue deliberately (CLAUDE.md → "Reach for a lighter subagent"; the brief's rule 4), and a strong model was chosen for i-0028 because the mechanism is unknown. A silent fall back to the default undoes that decision, costs the wrong amount, and nothing in the chat, the header, or `chat_status` says the chosen model is not the one running. Supervision happens at the orchestrator level, so the coordinator has to be able to trust the value it asked for or be told it did not take.

**Done looks like.**

- A model chosen at `issue_start` survives a failed first spawn and is in force on the session that finally comes up, or the start is refused with the agent's list, as the description promises. Either is honest; silence is not.
- `chat_status` reports the model the session is actually running, and reports it the same way whether the pane is holding a pending choice or the session has confirmed it — two different facts should not both read as `"model"`.
- A test in `chats.rs` that makes the first spawn fail and the second succeed, and asserts the chosen model reaches the session.

**Interaction with i-0011.** i-0011 is the failed first spawn itself. This issue is what that failure loses on the way through; fixing i-0011 will make it rarer and will not fix it, since a reconnect after any disconnect takes the same path. Whoever takes this should read i-0011 first, and the two agents should not edit the same reconnect code at once — coordinate through the coordinator if both are running.

**Rules in force.** GTK objects never leave the main thread. Intended interfaces only: the model is set through ACP session config, not through the adapter's own files. Oxford commas in everything written. Gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Nothing visual changes unless the header is made to say which model is running, in which case a probe of it at both rungs. Commit per verified batch; never push. Anything found along the way is a new issue, not a widening of this one.
