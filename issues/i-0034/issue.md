---
title: Count-tokens and reasoning content on the private route, once the spike reports
state: open
reporter: i-0028
created: 2026-09-16T01:59:51Z
updated: 2026-09-16T02:03:48Z
labels: authproxy, models, chat
---

**Held back from i-0028 deliberately.** The IDE half of the private model shipped — two upstreams in the auth proxy, a per-chat route, the picker row, and the header (i-0028, `agents/i-0028`). Two items in that issue's Shape depended on findings the spike was to leave as a comment, and the comment never landed, so they were left alone rather than guessed at.

## 1. Count-tokens

i-0028: "If the spike shows Claude Code needs `/v1/messages/count_tokens` and the server lacks it, the proxy answers from a local estimate for the private route only."

Two unknowns, and the second only matters if the first is yes:

- Does the pinned Claude Code adapter actually call `/v1/messages/count_tokens`, and on what — every turn, or only when it is deciding whether to compact?
- Does `llama-server`'s Anthropic-compatible surface implement it? README → "A private model on your own hardware" asks for that curl explicitly, and the answer decides everything here.

If it is needed and absent, the proxy answers it itself **for the private route only**, from a local estimate, and the estimate's error should be stated where the code lives rather than left for a reader to discover — a token count the agent uses to decide when to compact is a number that being wrong by 30% actually costs something.

## 2. Reasoning content

i-0028: "If the spike shows the server's thinking blocks arrive as something the chat cannot render, that is a second issue, not this one." This is that issue, for the case where it turns out to be true.

gpt-oss-20b through `--jinja` emits reasoning, and where it lands in an Anthropic-shaped response — a `thinking` block, a text block with tags in it, or something that fails to parse — is unknown. The chat's thought rendering is `render_update`'s `AgentThoughtChunk` path; what reaches it depends on what Claude Code makes of the wire.

## Before either

**One real turn through the shipped path.** Write `$XDG_STATE_HOME/taste-ide/private-model.json` (ENVIRONMENTS → The auth proxy → A private model), pick the new row in a scratch environment's model drop-down, and prompt it. That is now a smaller act than the spike was — no env var, no restart — and it answers both questions at once: the log says whether count-tokens was asked for, and the transcript says what the thinking looked like.

## Acceptance

Each half is either implemented or written down as not needed, with the observation that decided it. Nothing here is guessed at from the shape of the API.
