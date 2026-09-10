---
title: An agent's own tools reach the checkout directly, and only prompt text steers them to the IDE's calls
state: open
reporter: primary
created: 2026-09-10T07:00:35Z
updated: 2026-09-10T07:00:35Z
labels: design, mediation, supervision
---

The coordinator ran `grep -n "fn parse_instant" -A 40
/var/home/straussd/Projects/taste-ide/crates/taste-authproxy/src/quota.rs`
through its harness's own Bash tool, and it worked. The user's question was
"how is this even an accessible path to you?", and the answer is that the IDE
told the agent that path and then mounted the files under it.

## What is actually true

The agent is confined as designed. `TASTE_IDE_CONFINEMENT=container-exec`, uid
1000, root on fuse-overlayfs. `/proc/self/mountinfo` inside the container:

```
/home/straussd/Projects/taste-ide -> /var/home/straussd/Projects/taste-ide rw btrfs
```

The mount source is the project subvolume, not the home directory:
`/var/home/straussd/.ssh`, `.config`, `.bashrc`, and `Documents` are all absent
inside the container. **The host boundary held.** Nothing here is a sandbox
escape, and this issue should not be read as one.

What did happen is that the checkout is reachable at two paths — `/workspaces/taste-ide`
and the host-side name — and the IDE hands the agent the host-side one:
`ide_environment` reports `workspace: /var/home/straussd/Projects/taste-ide`,
and every `ide_search` hit comes back as an absolute host path. So an agent that
takes the IDE at its word and pastes a returned path into its own shell or its
own file tools succeeds, and never touches `fs/read_text_file`, `ide_search`, or
`ide_exec`.

## Why it matters

Not for the boundary. ARCHITECTURE and CLAUDE.md are explicit that mediation is
user experience rather than a gate, that the agent and the repo are one
principal, and that in container mode `ide_exec` is a shell with the workspace
writable. All of that stands.

It matters for the three things mediation actually buys, each of which is lost
on the unmediated path:

- `fs/read_text_file` sees the user's unsaved editor buffers. A shell read sees
  only what is on disk, so an agent can quote a file back at the user that is
  not the file they are looking at.
- The IDE-applied write lands in the user's undo stack. A direct write does not.
- `ide_exec` is why one environment is of record: the command is mirrored into
  the console tab, it appears in the environment's shells list, and it can be
  killed from there. A harness-native shell call has none of that.

The agent's own tool calls do still reach the chat as ACP tool-call cards, so
this is not invisibility — but they render as bare captions today (see i-0017),
which is how five of them scrolled past unread.

## The tension to resolve

Either the host path should not resolve inside the container, or the IDE should
not be handing it out. Both ends are currently true, and that combination is
what makes the shortcut work by accident rather than by choice.

Neither direction is obviously right, and the decision is the work here:

1. **Mount the checkout only at `/workspaces/taste-ide`.** A host-absolute path
   then fails, and the mediated calls become the only ones that work. But the
   IDE would have to stop returning host paths from `ide_environment`,
   `ide_search`, `ide_find`, and `ide_semantic_search`, and those paths are
   useful precisely because they name the file the user sees in their editor.
2. **Keep the mount, and return workspace-relative paths.** The agent gets a
   name that means something to the IDE and nothing to a shell.
3. **Keep both, and make the cost visible instead.** Nothing structural changes;
   the unmediated path stays available, and the chat renders those calls
   legibly enough that the user notices (i-0017), perhaps marking a file read
   that bypassed the buffer.

## What has been tried

Prompt text, as of `f2eedcc`: `instructions()` in `crates/taste-mcp/src/server.rs`
tells every agent to read through the IDE's own calls and never through the
shell, with both reasons. `0b4e335` pins it in a test. It is the right thing to
say and it is not enforcement — the coordinator broke the rule two turns after
landing it, under a session-level instruction that pushed the other way and
re-asserts itself after every context compaction. Prompt text loses that
argument often enough to plan around.

## First step

A decision, not a patch. Whoever picks this up should establish the cost of
option 1 first: how many IDE surfaces return host paths, and what breaks in the
chat, the file tree, and the editor if they return workspace-relative ones
instead. `ide_search`, `ide_find`, `ide_semantic_search`, `ide_environment`,
`ide_list_files`, and `ide_open_file` are the ones to enumerate. Report that
before changing anything.

## Gate

If it ends in code: `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, and `cargo test --workspace`. Oxford commas in
everything written. Commit per verified batch; never push.
