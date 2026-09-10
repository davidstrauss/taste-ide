---
title: ide_exec calls miss the IN/OUT block, so no command run through the IDE renders as a command
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-10T06:39:10Z
updated: 2026-09-10T07:30:44Z
labels: bug, chat, ui
---

The chat's IN/OUT block exists, is correct, and is reached by almost nothing.
Five consecutive `ide_exec` calls in the coordinator's transcript render as
identical collapsed rows reading **"Run sh"** with the summary **"1 line"** and
a disclosure arrow — no command, no `IN`, no `OUT`.

David, 2026-09-10, with a screenshot of those five rows: *"This is obviously not
using the IN/OUT block for these commands."*

## It is provably the non-command branch

The summary is the tell. `chat.rs:5192-5195` is the generic branch and produces
`"{n} line{s}"`. A command step's summary is `chatdoc::digest`
(`chatdoc.rs:251-262`), the last line the command actually printed — "where a
build says `ok` or `FAILED`". Those five rows would read `CLIPPY_EXIT=0` and
`TEST_EXIT=0` if they were being treated as commands. They read "1 line".

## Root cause

`chat.rs:5106`:

```rust
if card.kind.get() == ToolKind::Execute {
```

`ToolKind` is what the *agent's adapter* reports for its own built-in tools —
Claude Code's Bash is `Execute`. `ide_exec` is an MCP tool, so it arrives with
no kind the adapter cares to type, and the card keeps its constructed default
of `ToolKind::Other` (`chat.rs:5022`). It falls to the `else` at
`chat.rs:5137-5200` and renders as a bare wrapped caption label.

The gate asks the wrong question. It asks what kind of tool the agent thinks it
called, when what matters is whether this is a command with output. And the
tool it misses is the IDE's own: `ide_exec` is how every agent in this
workspace is supposed to run anything, so in practice the IN/OUT block is dead
code on the path that matters, kept alive only by agents reaching for a shell
tool the project tells them not to use.

The title is the same fault one layer up. "Run sh" names the program the tool
call passed, not the command. Under IN/OUT, `IN` is the command line and the
title can stay short.

## The fix, and the precedent for it

Recognise the IDE's own command tools by name. The chat already does exactly
this for the IDE's other MCP tools — `ActKind` and `act_headline` map
`issue_reorder`, `issue_start` and the rest onto their own rendering
(`chat.rs:7871`). `ide_exec` and `ide_exec_output` want the same treatment,
mapping onto the command path rather than a new one.

Prefer widening what counts as a command over duplicating `command_block`'s
call site. Whether that is a second condition at `chat.rs:5106`, or setting
`card.kind` to `Execute` when the tool is one of ours, is the implementer's
call — but say in a comment why the adapter's `ToolKind` is not sufficient on
its own, so the next reader does not "simplify" it back.

## Take IN from the input, not from the result

`ide_exec`'s result JSON has a `command` field, and reading `IN` from it would
be the obvious shortcut. Do not: that field currently holds the full `podman
exec --env … --workdir … <container> sh -c …` wrapper, so this issue would end
up rendering the wrapper in the most prominent position on the card — the exact
opposite of **i-0016**, which is about that string not reaching the user at
all.

`IN` is the call's *input*: the `command` and `args` the agent passed. Either
take it from there, or land i-0016 first and take it from the result. The two
issues touch different crates and can proceed independently as long as this one
does not source `IN` from the result while i-0016 is open.

## Secondary, same gate

Once a command is a command, two smaller asymmetries in the `else` branch are
worth settling in the same pass:

- **The opener.** A command always gets the "open the pair in the editor"
  button (`chat.rs:5122-5134`, unconditional). Every other result gets one only
  when it was clipped (`hidden > 0`, `chat.rs:5176`). Same kind of content,
  different affordance, for a reason the reader cannot see.
- **The other kinds.** `Read`, `Search`, `Fetch` and the rest still render
  untagged. Whether they want `IN`/`OUT` too is a judgement — if the title
  already reads `Read src/main.rs`, an `IN` row repeating the path is noise.
  Decide per kind and record the decision in a comment rather than emitting an
  empty tag.

Diffs stay as they are: an edit is a diff view (`chat.rs:5139-5157`), not an
IN/OUT pair, and the side-by-side layout must not regress.

## The constraint that will bite

The chat's width is never computed from its content
(`docs/ARCHITECTURE.md:1139`). `command_block` is built from boxes rather than a
grid for precisely this reason — `chatdoc.rs:428-432` records that a `GtkGrid`
measured its wrapping labels at their minimum width and made a step's row
reserve ten thousand pixels for ten lines of output. Whoever widens this walks
into the same trap. Measure with `TASTE_MEASURE_MIN=1 TASTE_PROBE_CHECK=1
TASTE_PROBE_CHAT=busy`.

## Test

The seeded transcript already has an `Execute` step (`chat.rs:7402`,
`chat.rs:7438`), which is why the block looks right in the probe and wrong in
life. Add a fixture step for an IDE MCP command call — the shape `ide_exec`
actually arrives in, with no useful `ToolKind` — and assert it renders the
command block. That is the case no existing fixture covers, and it is why this
shipped.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. The chat pane is held to the highest
bar in the app, so pose it and look: `TASTE_PROBE_CHECK=1
TASTE_PROBE_VIEW=orchestrator`, and `TASTE_PROBE_CHAT=busy` for the transcript.
Oxford commas in everything written. Commit per verified batch; never push.
