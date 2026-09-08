---
title: An environment with unpublished commits and no live agent reads as "working", so finished work sits invisible
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-08T16:42:35Z
updated: 2026-09-08T17:25:54Z
labels: bug, orchestration, review
---

**What is wrong.** An environment that has finished its issue — gate green, work committed — is indistinguishable from one still thinking, for as long as it has not published. Nothing in `review_list`, the panel, or the coordinator's view changes when an agent commits and stops. The only way anyone learns is a person noticing the chat has gone quiet and asking.

**How it was found.** i-0004, 2026-09-08. Its agent completed the change to `crates/taste-app/src/notify.rs`, ran the full gate clean (`fmt --check`, `clippy -D warnings`, `cargo test --workspace`, 32 test binaries, 0 failures), committed `8ed1a5a`, and stopped without publishing. At that moment:

- `issue_status i-0004` reported `review: "working"`, `published: 0`.
- `git branch` in the user's checkout had no `agents/i-0004` at all — `agents/i-0006` was the only `agents/*` branch, and it belonged to a declined issue.
- `review_list` returned `count: 1`, listing only i-0006. The environment that had actually finished was not in it.

So the commit existed in exactly one place, inside that environment's own checkout, with nothing anywhere pointing at it. David found it by looking at the tab: "I think the turn completed env completed, but it's not marked for review."

**Why it matters beyond the one case.** The commit is unreplicated. If that environment is destroyed — and a coordinator reading `review: "working"`, `published: 0` has every reason to think it holds nothing — the work is gone with it. The failure mode is silent and it is destructive, which is a bad pair.

**The fact is already available; nothing consumes it.** `issue_list` and `issue_status` carry `unpublished` and `dirty` per environment. What is missing is that no derived state, no light, and no notice depends on them. `review` is computed as though publishing were the only thing that could ever indicate progress.

**Done looks like.** An environment holding commits its branch of record does not have is visibly distinct from one still working — in `review_list`'s `review` field and in the panel — and that state is reachable by the coordinator without polling every environment's git. A committed-and-idle environment should be chased, not discovered. Whether the right signal is a new review state, a light, or a coordinator notice is open; pick one and say why in the commit message.

**Related.** The cause in this instance is that the agent read "publish" as "push" and skipped it — filed separately, since the surfacing gap would matter even with a perfectly behaved agent.

**One thing to check while in here, not to fix here.** After i-0006's environment was destroyed at 16:24, `review_list` still listed `agents/i-0006` with `review` reset from `flagged-for-review` to `working`. That is the same field being unreliable in the other direction. If it turns out to be a separate defect, file it; do not widen this change to cover it.

**Rules in force.** The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. One build at a time.
