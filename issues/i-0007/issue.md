---
title: Fix issue_reorder: its schema declares `issue`, its handler reads `id`, so every call fails
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-08T14:19:37Z
updated: 2026-09-08T16:50:06Z
labels: bug, mcp, coordinator
---

**What is wrong.** `issue_reorder` cannot be called successfully by anyone. Its advertised schema and its handler disagree about the name of the required argument, and no payload satisfies both:

- **Schema** (`crates/taste-mcp/src/orchestration.rs:122-129`) declares `{ issue, position }`, both required. There is no `id` property, so a client that validates against the schema cannot send one.
- **Handler** (`crates/taste-mcp/src/server.rs:2235`) calls the shared `issue_id_arg(&args)`, which reads `args["id"]` and nothing else (`server.rs:3072-3079`).

The result is that a schema-conforming call is refused with `this tool needs an `id` — issue_list shows them, they look like i-0001`, which is doubly confusing because it names an argument the tool does not accept.

**How to see it.** As the coordinator, call `issue_reorder` with `{"issue": "i-0006", "position": 0}` — the only shape the schema permits. It fails every time. Observed 2026-09-08 while triaging i-0006, which genuinely outranked the item above it and could not be moved.

**Why it survived.** The other users of `issue_id_arg` (`issue_update` at `server.rs:2247`, and `server.rs:2290`) declare `id` in their schemas and are consistent; `issue_reorder` is the lone mismatch. The tests that mention it — `server.rs:4823` and `crates/taste-acp/tests/orchestrator.rs:318` — only assert that a **non**-orchestrator is refused, so they never make a successful call and the argument name is never exercised.

**Which way to fix it.** Prefer changing the **handler** to read `issue`, not the schema to demand `id`. `issue_start`, `issue_status` and `issue_attachment` all take `issue`, so `issue` is the established name for "which issue" among the coordinator's tools, and `issue_reorder` sits with them. Changing the schema instead would make the tool consistent with `issue_update` but inconsistent with the three it is used alongside, and would silently break any client already written against the published schema.

**Done looks like.** `issue_reorder {issue, position}` moves an issue and returns the new order. A test that actually performs a reorder through the MCP surface and asserts the resulting order — not just the orchestrator-gating refusal — so this class of mismatch cannot return. Consider whether the same round-trip gap exists for the other orchestration tools while you are there; if it does, that is a separate issue, not a widening of this one.

**Rules in force.** CLAUDE.md, "intended interfaces only": the schema is the published surface and clients are entitled to trust it. The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
