---
title: The permission card drops the agent's "don't ask again", and a standing answer must reach every environment
state: open
reporter: primary
created: 2026-09-13T08:41:53Z
updated: 2026-09-13T08:41:53Z
labels: bug, ui, chat, permissions, acp, environments
---

David, 2026-09-13: *"Auto mode is still asking me to review read-only ops."* The IDE's own permission log has `Ide Search` approved by hand twice in six seconds:

```
08:36:51  Ide Search (IDE)      approved — the user clicked "Yes"
08:36:57  Ide Search (IDE)      approved — the user clicked "Yes"
```

Nothing an agent does can make a read stop asking. Same complaint on 2026-09-08 — *"it keeps giving me these prompts even though the agent is set to use AI review"* — and it is recorded in `protocol.rs`'s `Effect` doc as the reason the annotations were written. The annotations were not the whole of it.

## Why the loop has no exit

Claude Code gates MCP tools on the user's allowlist **before** the auto classifier runs, so `readOnlyHint: true` does not exempt them. Our annotations are complete and correct (`crates/taste-mcp/src/protocol.rs:117-189`; `ide_environment` and `ide_search` are both in the Read arm) and they are simply not what decides. The client's own answer to this is a third permission option, `AllowAlways` — and the IDE never shows it.

`allow_option` (`crates/taste-acp/src/session.rs:1383`) returns `AllowOnce` and falls back to `AllowAlways` only when there is no one-shot, and the card builds exactly one allow button and one deny button from it (`crates/taste-app/src/chat.rs:5784-5789`). So when the agent offers yes, yes-and-don't-ask-again, and no, the user is shown the first and the third. The comment three lines above that code already states the intent — *"'don't ask again' is a different answer from 'yes, this once' and must not read alike"* — and the button was never built.

**`allow_option` must not change.** `first_allow_outcome` uses it for the IDE's own auto-approve, where preferring the one-shot is right: approving a call is not rewriting the agent's standing policy. The card is what needs the third answer.

## The second half: a standing answer is worthless if it is one environment's

David, 2026-09-13: *"Ensure that any 'don't ask again' policies get encoded for the project — including all environments."*

This is the requirement, not a nice-to-have. An environment is a clone with its own agent process and its own agent home, so whatever an agent persists when it is told "always" lands in that one environment's world. With eleven environments restored at startup, an answer given once has to be given eleven times, and again for every environment `issue_start` makes tomorrow. A per-chat "don't ask again" would fix the symptom in the tab the user happened to be looking at and leave the complaint exactly where it is.

So the policy belongs to **the project**, and a new environment must be born holding it.

Two mechanisms, to be argued rather than assumed — and they are not exclusive:

1. **The IDE holds the policy and answers for every chat.** The IDE already mediates every permission request; a standing answer it keeps is in force the moment it is given, in running environments and new ones alike, and it is agent-agnostic — ACP serves Gemini and Copilot too, and neither has Claude Code's settings file. The cost is that it is IDE state, so it needs a home that survives a restart and is per project rather than per machine (ARCHITECTURE → Conventions; `~/.config/taste-ide/` is user-level, which is the wrong scope for "this project's reads are fine").
2. **The IDE writes the documented project surface**, `.claude/settings.json`'s `permissions.allow`. It is tracked, so `issue_start`'s clone inherits it and `update_from_main` carries it to environments already running, and it is the mechanism the client actually consults — which is the thing that stops the prompt. The costs are that it is Claude Code-specific, and that it is a write into the user's tracked files, which becomes a commit they did not make.

**Whichever it is, the user answers and the IDE writes. The agent never widens its own permissions.** CLAUDE.md's "configuration authority is execution authority" is the adjacent rule and it points the same way: a permissions file an agent can write is a permissions file an agent can widen, and that is a hole rather than a convenience. If the chosen mechanism is a file in the checkout, `taste_core::policy::write_allowed` is not the thing that protects it — decide explicitly what does.

## What else has to be decided

- **The grain of a policy.** "Don't ask again" about the tool, or the tool with these arguments? For the read set the tool is the right grain and is what the complaint is about. For `ide_exec` it is plainly not — `Effect::Destructive` is honest there, and a blanket always-allow on it is a shell with no gate.
- **`RejectAlways` has the mirror-image problem.** `reject_option` (`session.rs:1391`) has the same fallback shape, so a standing *no* is dropped exactly as a standing yes is. Whatever the card grows, it grows on both sides or the asymmetry needs a reason.
- **A policy with no way to see or undo it is a trap.** `ide_permission_log` shows decisions as they went over the wire; standing policies are a different list and need somewhere the user reads them and takes one back.
- **Where the third answer goes on the card.** The permission bar is held to the highest bar in the app and lives in a pane whose minimum is 320px. A third button, a split button, or a menu on the allow button — decide, then look at it. `TASTE_PROBE_CHAT=permission` and `permission-edit` pose both variants.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus:

- a test that a request carrying all three options renders all three and sends the right `option_id` for each, including that the always-option is not silently answered as a one-shot;
- a test that a policy answered in one environment is in force in another, which is the requirement above and the thing that will regress;
- a test that `allow_option` and `first_allow_outcome` still prefer the one-shot, so auto-approve is not widened by this change;
- a probe of both permission variants at the narrow rung and at 1440x900, because this is chat-pane work and it is not done until it has been looked at.

`docs/ARCHITECTURE.md`'s permissions section (the Auto-mode-as-classifier passage) gains whatever is decided, and `docs/ENVIRONMENTS.md` gains the sentence about what a new environment is born holding. Oxford commas in everything written. Commit per verified batch; never push.
