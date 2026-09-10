---
title: The coordinator's brief omits two standing rules: work goes to the backlog, and reading goes through the IDE
state: open
reporter: primary
created: 2026-09-10T06:22:50Z
updated: 2026-09-10T06:29:53Z
labels: orchestration, prompt
---

`taste_core::orchestration::coordinator_brief()`
(`crates/taste-core/src/orchestration.rs:80-141`) is the coordinator's operating
instructions. Two rules the user has had to state by hand in conversation are
missing from it, so every fresh coordinator session starts without them.

## Gap 1: implementation belongs to agents on the backlog

David, this session: *"Actual implementation tasks should always be handled by
agents on the backlog. If your prompt is unclear on that, we should also update
that."*

Rule 9 currently says only:

> You do not edit the user's checkout yourself except to merge — the work
> happens in the issues' environments.

That is a rule about editing a checkout. The rule David stated is broader and is
about *where implementation work goes*, which today has to be inferred from a
sentence about file edits. State it directly, in rule 1 or rule 9, so the
coordinator's response to "please change X" is always: write the issue, confirm
it, start it — never do it here.

## Gap 2: read through the IDE's own calls, not a shell

David, this session: *"Why do you keep using shell commands to access files in
this project? It's really important that you read through high-level calls so I
can supervise."*

The brief's only tool guidance is rule 7's "run the tests in your own
environment (ide_exec)", so the one pointer it offers aims at the shell. Nothing
tells the coordinator to inspect code with `Read` (ACP `fs/read_text_file`),
`ide_search`, `ide_references`, `ide_semantic_search`, and `ide_list_files`, or
to use `ide_open_file` to put a file in front of the user.

Both reasons belong in the text, because the second is not obvious:

- The user supervises through those calls. A `sed` inside a container shell is
  opaque in a way a Read is not.
- `fs/read_text_file` sees the user's **unsaved editor buffers**; a shell in the
  devcontainer sees only what is on disk. The user edits this codebase while the
  coordinator works, so a shell read can quote stale content back with nothing
  to signal it.

`ide_exec` keeps its place for *running* things — builds, tests, probes, git.
The distinction is inspect versus execute, not "avoid ide_exec".

## Constraints on the change

This text is a system prompt, and it is long already. Add the rules in the
brief's existing voice — imperative, concrete, no hedging — and look for
sentences that can carry the new clause rather than appending two more numbered
rules. Do not restructure the nine rules; they are referred to by number in
conversation.

It ships in two places, and both must keep working:
`crates/taste-mcp/src/server.rs:214` (the MCP server's `initialize`
instructions on the primary's socket) and `crates/taste-app/src/chat.rs:3845`
(`with_coordinator_brief`, the preamble before the coordinator's first prompt of
a fresh session). The doc comment at `orchestration.rs:68-79` explains why there
are two; keep it accurate if the text moves.

## Test

A test in `taste-mcp` pins the instructions. Per the doc comment at
`orchestration.rs:77-79` it looks for `issue_reorder`, `issue_start`,
`review_list`, and the header line. Find it, and extend it to pin whatever
names the new rules introduce, so a future edit cannot quietly drop them the way
these two were never added.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. Oxford commas in everything written —
the brief is prose and the rule applies to it. Commit per verified batch; never
push.
