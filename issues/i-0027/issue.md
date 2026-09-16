---
title: fs/read_text_file answers "File does not exist" for every path after an IDE restart
state: open
reporter: i-0022
created: 2026-09-14T10:19:18Z
updated: 2026-09-16T02:32:13Z
labels: bug, acp, agents
---

Found while finishing i-0022, in the i-0022 environment, after the IDE
restarted under a live agent session.

## What happens

Every ACP `fs/read_text_file` from this session fails with "File does not
exist", for every path, in both directions:

- `…/environments/799fd7acd369bf5c/i-0022/repo/crates/taste-mcp/src/protocol.rs` — fails
- `…/environments/799fd7acd369bf5c/i-0022/repo/CLAUDE.md` — fails
- `/var/home/straussd/Projects/taste-ide/crates/taste-mcp/src/protocol.rs` (the main
  checkout, which the IDE is itself displaying) — fails
- `/workspaces/taste-ide/…` (the container's path for the same tree) — fails

Meanwhile everything else about the session is healthy: `ide_search`,
`ide_find`, `ide_list_files`, `ide_open_file`, and `ide_exec` all answer
normally, and `ide_exec` reads and writes those exact files through the
container mount without trouble. So the checkout is there, the MCP
surface is there, and it is the fs leg specifically that is dead.

One clue: `ide_open_file` with a workspace-relative path resolved to
`/var/home/straussd/Projects/taste-ide/…` — the PRIMARY checkout — from a
session bound to the i-0022 environment. Whatever binding the fs leg
consults for "which workspace is this session's" may be the thing the
restart lost, or may be pointing at a root that no longer matches the
session.

## Why it matters more than it looks

The house rule every agent is given says to read this project's files
through the IDE's own calls and never by reaching into `ide_exec` for
`cat`, `grep`, or `sed`. When the fs leg is down, that rule cannot be
followed: the only remaining way to read a file whole is the shell the
rule forbids. The agent is then choosing between sitting idle and doing
the thing it was told not to do, and neither is what the rule is for.
It also silently removes the property the rule exists to buy — reads that
see the user's unsaved buffers.

An agent cannot tell a dead fs leg from a missing file, either: the error
is the same "File does not exist" either way, so the first symptom is an
agent concluding the file it is looking at does not exist.

## Reproduction

Restart the IDE while an agent session in a non-primary environment is
live, then have that session read any file over `fs/read_text_file`.

## Acceptance

- After a restart, `fs/read_text_file` answers for the session's own
  environment again — from the editor's buffer where there is one, and
  from that environment's clone otherwise.
- A session's workspace binding survives the restart, or the session is
  told it did not rather than being handed a path that resolves into
  somebody else's checkout.
- A read that fails because the leg is down says so, distinguishably from
  a file that is genuinely absent.
