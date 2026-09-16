---
title: A chat can run against a private Anthropic-compatible model, chosen per chat and routed by the auth proxy
state: open
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: fable
created: 2026-09-16T01:27:37Z
updated: 2026-09-16T02:00:14Z
labels: feature, authproxy, chat, orchestration, models
---

**Wanted.** A private llama.cpp server speaking the Anthropic Messages API, on a LAN host of the user's, usable as a model choice for any chat and for `issue_start`, with Claude Code unchanged as the agent. David, 2026-09-16: "I like the idea of using the Anthropic interface and existing client implementation." The README already carries the server half (README → "A private model on your own hardware": gpt-oss-20b on an RTX 3080 through `llama-server`, with the expert offload and the reasoning effort explained) and says plainly that the IDE half is not finished. This issue is the IDE half.

## Why the proxy is the only thing that changes route

Taste never talks to a model; it talks to agents over ACP, and the agents talk to models. Every Claude Code spawn already goes through `taste-authproxy` (`crates/taste-acp/src/authproxy.rs`, `crates/taste-authproxy`), which holds the Anthropic credential, hands the agent a per-environment placeholder token, and swaps the real header in on the way out. The proxy therefore already knows which environment is spending, from the placeholder — so the choice of upstream can be per chat and take effect on the next request, with no respawn and no change to the agent. The containers still reach nothing on the LAN; only the host-side proxy does, which is the boundary this codebase defends (CLAUDE.md → "The boundary is the host, not the agent").

Today the upstream is one process-wide override, `TASTE_AUTH_PROXY_UPSTREAM` (`authproxy.rs:95`), read once at start, and the proxy injects the Anthropic credential whatever the upstream is. That is the spike path, not the feature.

## Shape

- **A project-level setting for the private endpoint and its key.** Held in IDE state beside the Anthropic credential (`taste_authproxy::credentials`, `$XDG_STATE_HOME/taste-ide/anthropic.json` or a sibling), never in the checkout, never in an environment variable the agent sees. The user provisions it the way they provision the Anthropic credential; the IDE reads no other program's storage.
- **Two upstreams in the proxy, chosen per request from the placeholder.** The Anthropic credential goes to one, the private server's key (`x-api-key` or bearer, whichever `llama-server --api-key` accepts) to the other. A placeholder's route is a setting the chat pane flips; spend still lands in that environment's counters.
- **The chat's model picker gains a "private" value**, beside the values the agent advertises, and `issue_start` accepts it as `model`, so the coordinator can send scoped work to the free rung (CLAUDE.md → "Reach for a lighter subagent"). Decide and write down how a value the agent did not advertise composes with ACP session config: the agent still needs some model name to send, and `llama-server` serves whatever it loaded regardless of the name in the request.
- **The chat header says which upstream a session is on.** A turn against the private model must not look like a turn against Anthropic; the utilization gauge is the natural neighbour.
- **Count-tokens.** If the spike shows Claude Code needs `/v1/messages/count_tokens` and the server lacks it, the proxy answers from a local estimate for the private route only.
- **Reasoning content.** If the spike shows the server's thinking blocks arrive as something the chat cannot render, that is a second issue, not this one.

## What the spike will have told us

David runs the server and one turn through `TASTE_AUTH_PROXY_UPSTREAM` before this starts (README → "Then try a turn"). Its findings — whether the server's key check rejects the injected Anthropic credential, whether count-tokens exists, what the prompt and generation speeds are — land here as a comment before the agent begins, and the agent reads them first.

## Rules in force

Documented env vars and documented Anthropic client mechanisms only (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`); nothing read from Claude Code's own config. GTK objects never leave the main thread; no IO on it. The setting is the user's to write and the IDE's to hold — an agent never widens its own route. Oxford commas in everything written. Commit per verified batch; never push.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus:

- a proxy test that two placeholders route to two upstreams with the right credential on each, and that a placeholder with no private route never reaches the private upstream;
- a test that `issue_start` with the private value produces a chat whose placeholder is routed privately;
- a probe of the chat header at 1440x900 and at the consolidated rung, since it is chat-pane work and is not done until it has been looked at.

`docs/ENVIRONMENTS.md`'s auth proxy section gains the second upstream and the per-placeholder route; `docs/ARCHITECTURE.md`'s Conventions gain where the setting lives; the README's "The IDE half is not finished" paragraph is rewritten to say what is now true.

## Not in scope

A non-Claude ACP agent. A judgement on model quality, which is the spike's and David's. Anything found along the way is a new issue, not a widening of this one.
