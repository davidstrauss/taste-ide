---
title: ide_exec reports the podman wrapper it built, not the command it was asked to run
state: open
reporter: primary
created: 2026-09-10T06:37:30Z
updated: 2026-09-10T06:37:30Z
labels: bug, mcp, exec
---

Every `ide_exec` result carries the resolved command line as its `command`
field:

```
podman exec --env GIT_CONFIG_COUNT=5 --env GIT_CONFIG_KEY_0=url.push-blocked-0://.pushInsteadOf … --workdir /workspaces/taste-ide taste-799fd7acd369bf5c-primary sh -c cargo fmt --all -- --check
```

The agent asked for `sh -c "cargo fmt --all -- --check"`. Everything before that
is the IDE's own sandboxing — a constant of the system — reproduced on every
single exec, in the chat, where the user reads it.

David, 2026-09-10: *"I'm often seeing podman in your commands. That should be
implicit from the sandboxing of agents into containers. I should only see the
container-internal command being run."*

## Where it is

The clean string already exists and is already threaded most of the way. Three
of the four sites that handle it agree the wrapper is noise:

- `crates/taste-mcp/src/server.rs:1576-1581` builds `display` — the command and
  its args, joined — commented "The console tab shows what the agent asked for;
  the wrapper `spec` carries is for the agent's own eyes."
- `crates/taste-mcp/src/exec.rs:99-103` documents `display` as "the command line
  as the USER should read it", and notes that recovering it from the wrapper
  would be a guess, "and a guess in a tab title is a lie that looks like a
  feature."
- `crates/taste-mcp/src/exec.rs:135-138` hands the console's mirror `display`,
  "not the `podman exec --env …` wrapper … it is noise in a tab title."

The fourth keeps it. `crates/taste-mcp/src/exec.rs:47-50` puts the wrapper on
`Job.command` — "echoed back so the agent can see what actually ran, `podman
exec …` and all." `Snapshot.command` (`exec.rs:64`) carries it, and
`exec_result` (`server.rs:3243-3262`) puts it in the tool result on both
shapes, the finished one and the still-running one.

## Why the original reasoning does not hold

"So the agent can see what actually ran" assumes the agent has a use for it. It
has none: it never composes the wrapper, cannot change it, and cannot act on it.
The flags are facts about the environment — the container id, the workdir, the
push-blocking `GIT_CONFIG_*` pairs — and an agent that needs those has
`ide_environment` and `ide_write_policy` to ask. Meanwhile it costs context on
every exec, and it reaches the user, because a tool result renders in the chat
card. Being a JSON field rather than a tab title does not make it not a display
surface.

## Done looks like

`ide_exec` and `ide_exec_output` report the command as asked for. `Job` carries
`display`, and the snapshot reports that.

**The resolved line stays** (David, 2026-09-10: "it's okay to keep"). Nothing
here is about losing the record of what actually ran — only about it no longer
being the thing reported by default. Keep it on the job, and keep reporting it
where it is genuinely diagnostic: when the spawn itself fails, it is the only
evidence of why, and it should be in that error. If it is worth exposing on a
successful run as well, make it a distinct field that says what it is rather
than occupying `command`, which callers read as "the command".

While in there, check the other leaks. `exec.rs:128` builds its spawn error as
`format!("spawning {}", spec.program)`, which names `podman` and not the
program the agent asked for.

## Test

The existing exec tests spawn with `container: None, inside_container: true`, so
no wrapper is ever built and they cannot catch this. Add one that resolves a
command against a container target and asserts the reported command is the one
asked for, with the wrapper nowhere in it — the assertion that would have
failed from the start.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. Oxford commas in everything written.
Commit per verified batch; never push.
