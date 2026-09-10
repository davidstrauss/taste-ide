---
title: A reviewing coordinator has nowhere to run a branch, so it builds out of tree in the user's own environment
state: open
reporter: primary
created: 2026-09-10T07:12:40Z
updated: 2026-09-10T07:12:40Z
labels: orchestration, mcp, review, design
---

Reviewing an environment's work means running its gate on the *merged* result.
The coordinator has no way to run anything outside its own environment, which is
the user's own checkout — so it improvised, and the improvisation landed in the
user's workspace.

## What actually happened, 2026-09-10, reviewing i-0008

The coordinator needed `cargo fmt`, `clippy`, and `cargo test` over
`main` + `agents/i-0008`. What it did:

```sh
git worktree add --detach /tmp/rev-i-0008 main
cd /tmp/rev-i-0008 && git merge --no-edit agents/i-0008
export CARGO_TARGET_DIR=/workspaces/taste-ide/target
cargo clippy --workspace --all-targets -- -D warnings
```

Three costs, none of them intended:

- **The user's build artifacts were rebuilt from a foreign source path.** The
  shared `CARGO_TARGET_DIR` was the point — a cold target directory would have
  meant a full build — but it means the user's next `cargo build` recompiles the
  workspace crates.
- **A worktree entry was written into the user's `.git`.** Reversible, and
  reverted, but it is a mutation of their repository state, and the coordinator's
  standing rule is that it edits the user's checkout only to merge.
- **It competed for the machine.** CLAUDE.md is explicit that two concurrent
  cargo builds have frozen this host twice. Two agent environments were building
  at the time. The coordinator had no way to know that and no way to queue behind
  them.

Meanwhile i-0008's own environment already had the branch checked out, its own
container, and a warm target directory, and was sitting stopped with nothing to
do.

## Why there was no better move

The MCP contract binds every checkout, container, and shell tool to the
connection's own environment: "every tool that names a checkout, a container or
a shell means yours." `ide_exec` runs in the coordinator's container, against
the coordinator's checkout, and there is no parameter that says otherwise.

The alternative on offer is `chat_send` — ask the environment's own agent to run
its gate again. That is not a review. The agent has already reported that its
gate passed; a reviewer whose only instrument is asking the author to re-run
their own tests has verified nothing that was in doubt.

The coordinator's own briefing tells it to "read the branch against the user's
branch in your checkout (`git log` and `git diff`)", which is right for reading
and silent about running. Reading is the half that works.

## The complication: the branch is not what gets reviewed

Running the gate *in the issue's environment* is the obvious fix and it is not
quite right. That clone is at the branch tip, which is behind the merge target —
i-0008 was `behind: 2`. What has to pass is `main` with the branch merged in,
which exists in neither the coordinator's checkout nor the environment's.

So whatever this becomes has to name a place where that merge can exist and be
built, and that place must not be the user's checkout or their target directory.

## Directions, none of them decided

1. **`ide_exec` gains an environment argument**, honoured only for the
   orchestrator's connection. Smallest surface, and it makes every other
   environment's container reachable — which is a real widening of what one
   connection can do, and wants saying out loud.
2. **A purpose-built review tool.** Something that takes an environment, brings
   its clone up to the merge target, merges, runs the project's gate, and
   returns the result. Narrow, hard to misuse, and it encodes what a review is
   rather than leaving each coordinator to invent it.
3. **A scratch area that belongs to the coordinator**, explicitly outside the
   user's checkout and with its own target directory. Solves the "out of tree in
   the personal env" complaint directly and leaves the build cost where it is.

## Constraints any of them has to respect

- **A flagged environment's container is stopped.** Bringing it back up to run a
  gate consumes a slot under the environment cap, which is itself wrong today —
  see **i-0013**.
- **One build at a time, capped.** CLAUDE.md's rule exists because the host has
  frozen. Whatever runs the gate needs to know what else is building, or to
  serialize.
- **Nothing on the host, ever.** Every candidate above already runs inside a
  container, so this does not touch the boundary — but a scratch area is a new
  path and must not become the exception.

## Also worth checking while in here

`review_list`'s own description tells the reader to "pull the branches into your
own clone with `update_from_main`, merge and test there". That tool is not
exposed to the coordinator's connection, so either the description is addressed
to an audience that is not the one reading it, or the coordinator is missing a
tool it is being told to use. Establish which; a tool description that names a
call the reader cannot make is the same class of defect as the
`issue_attachment` mismatch (rule 2 of the coordinator's brief says to "attach
evidence with `issue_attachment`", and that tool only reads attachments — there
is no way to create one).

## Done looks like

A coordinator can produce a gate result for a branch merged into the target
without writing anything into the user's checkout, their `.git`, or their build
artifacts — and without having to invent the procedure each time.

## Gate

Mostly `taste-mcp`. `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, and `cargo test --workspace`. If the answer grants
one connection reach into another environment, a test that the grant is refused
from a non-orchestrator connection is the one that matters. Oxford commas in
everything written. Commit per verified batch; never push.
