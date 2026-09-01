# Roadmap & UX ledger

Candidate work, ordered by how much it advances "superlean but
hyperfunctional." Each item must clear the bar in ARCHITECTURE.md: no
extension points, conventions over configuration, agents through ACP.

## UX-convention ledger (2026-08 audit)

Done in this pass:
- Primary menu (hamburger) with About and Keyboard Shortcuts — the HIG
  staple the window was missing.
- Ctrl+P quick-open over the existing search index; Ctrl+Q graceful quit.
- Markup escaping for file names in row titles.

Standing conventions to keep honoring:
- Disable, never hide; no modal dialogs for dirty-file workflows (bottom
  intervention panel); toasts for outcomes; StatusPage for empty states;
  destructive styling + confirmation only for destructive actions.
- Known debt: the file pane header is crowded (new-file, three git
  filters, ignored toggle); consider a split into a toolbar row once one
  more control appears. The chat "Auto-approve" switch overlaps
  conceptually with the agent's Auto mode — consolidate when the ACP
  permission story settles. One stray `GtkGizmo snapshot without
  allocation` warning remains unexplained (cosmetic; watch it).

## The multi-environment program (approved 2026-08-31)

`docs/ENVIRONMENTS.md` is the design of record: an arbitrary number of
named environments per workspace (one per agent chat, plus human ones),
each with its own clone of the main checkout and its own devcontainer;
mediated publish of agent branches into the main checkout for review;
per-environment safe mode (the devcontainer repair loop applies to every
environment); an orchestrator chat with fleet view; issues on
`refs/taste/issues`. Hard invariant throughout: **agents never push to
the real remote (GitHub) — that is the user's act alone.**

Phases (each lands green and independently useful):

0. Multi-chat tabs (Near-term feature 0 below — unchanged design).
1. Auth proxy (Agent hardening 1 below — now a prerequisite, not
   queued hardening).
2. Environment core: registry, N supervisors, per-env
   naming/volumes/sockets/ExecContexts, tagged events, clone lifecycle,
   WorkspaceState v2.
3. Mediated publish + review inbox (taste-git plumbing, publish/update
   MCP tools, agents/* filter).
4. Agent relocation into the env container, outside-confined safe-mode
   fallback, session/load bridge.
5. Fleet view (Containers tab → environments view).
6. Orchestrator chat + orchestration MCP tools, per-level models.
7. Issues ref + tools + user-push ride-along.

## Where the agent runs — RESOLVED

> **Resolved 2026-08-31** as option C (relocation), gated on the auth
> proxy, by the multi-environment program above. The analysis below is
> kept because its reasoning — especially the trust question and the
> history-keying pitfalls — constrains the implementation.

The mediated topology (agent outside the devcontainer, workspace served by
the IDE) ships, and one assumption under it is false: `claude-agent-acp`
does not route its Read/Edit/Write tools through the ACP client.
`dist/acp-agent.js` defines `readTextFile`/`writeTextFile` and **nothing
calls them**; Claude Code reads the filesystem directly. With no workspace
mounted, an agent native file tools fail outright and it works only
through `ide_search`, `ide_list_files` and `ide_exec`. Three ways out, and
the choice is a commitment rather than a fix.

**A. MCP file tools.** `ide_read_file` / `ide_write_file` /
`ide_edit_file`, wrapping handlers that already exist and are tested.
Keeps every property the mediation was for, including reads answered from
the editor live buffers — the one thing only mediation delivers. Costs:
the model must choose them over its native tools, and each session opens
with a failing Read. Evidence it works: every MCP tool here has been used
successfully by an agent that had to choose it.

**B. Read-only workspace bind.** Native reads work; writes fail EROFS and
go through the IDE. Cheap, but reads come off stale disk (no unsaved-edit
awareness, no audit trail) and the stand-in plus `agent_context_scope`
become dead code.

**C. Relocate the agent into the devcontainer** — VS Code model. VS Code
does not mediate file access as a boundary: for Dev Containers and
Remote-SSH it moves the extension host to where the files are, so its
agent gets an ordinary filesystem. It does mediate *writes* through the
document layer (undo, dirty buffers, diff review) — as UX and correctness
machinery, never confinement; its security gate is Workspace Trust,
coarse and up front. Relocation makes the problem vanish: native tools
work because the files are there.

### What C requires, deliberately

Continuity is not the obstacle. `session/load` round-trips and there is a
test for it: the IDE persists the session id (`taste_core::state`), the
agent keeps the history. But three things must be handled or relocation
silently loses that history.

1. **History is keyed by cwd.** The adapter calls `listSessions({dir:
   params.cwd})` and stores under `~/.claude/projects/<flattened-cwd>/`.
   The devcontainer workdir is `/workspaces/<name>`, not the host path, so
   relocating changes the key and every past conversation becomes
   unfindable.
2. **History lives under HOME**, on the `taste-agent-home` volume, which
   survives container removal. The devcontainer home is not that volume:
   mount it in, or a rebuild takes the history with it.
3. **Path translation.** The agent container mounts the workspace at its
   REAL host path on purpose, so paths mean the same on both sides of ACP
   and MCP. A relocated agent speaks `/workspaces/...` while the IDE
   speaks the host path. `ExecContext::container_workdir()` and `lsp.rs`
   already map this, so there is precedent — and a class of bugs the
   current design avoids by construction.

### The trust question, correctly stated

It is tempting to say C is bad because it puts the agent in the same
container as the repo build and test code, which CLAUDE.md calls
untrusted. That argument does not hold. **The agent writes that code.** It
authors `build.rs`, the tests and the devcontainer config, and `ide_exec`
lets it run them — an agent wanting code to execute in the devcontainer
writes a test and calls `cargo test`. Agent and agent-authored code are
one principal, and no boundary between them means anything.

The boundary that does survive is a different pair: **the agent's
credentials against code the REPO supplied.** `~/.claude/.credentials.json`
(mode 600) lives on the `taste-agent-home` volume. A repo cloned from
anywhere can carry a hostile `build.rs` that runs during an ordinary
build, and the case worth defending is that build reading the user's
Anthropic token. An agent writing hostile code is self-inflicted — it
already holds the credential. A third-party repo is not.

That separation exists today, and mostly by accident: before the mediated
topology the agent container held the workspace, the toolchain AND the
credentials, so the agent could run repo code next to its own token. Now
repo code executes in the devcontainer while credentials stay in the
agent container. It is the strongest security property of that change and
was not the reason given for it.

So C is not blocked by trust — it is blocked by one concrete requirement:
keep the credential store out of reach of anything the repo supplies.
The promising mechanism is a second uid inside the devcontainer: the
agent process runs as its own user with a mode-700 home, while `ide_exec`
and builds keep running as `dev`. Rootless podman maps `dev` to the host
user and the agent uid into subuids, so repo code cannot become the agent
without privilege it does not have. The alternative is VS Code's answer —
accept it, and let Workspace Trust be the gate — which is coherent but
strictly weaker than what we have now.

Weigh all of it knowing what write enforcement is worth today: in
container mode, nothing. `ide_exec` already gives the agent a shell with
the workspace writable (verified by doing it). C surrenders less than it
appears to.

Safe mode settles itself either way: with no devcontainer there is nowhere
to relocate to, so the agent stays outside with the stand-in workspace, no
exec, and `write_allowed` genuinely confining. Two modes, two topologies,
each falling out of its own premise.

### Recommendation

A first: additive, preserves the live-buffer property, leaves C available.
If A proves frictional in practice, C is the principled retreat, not B.
Do not ship C without settling the trust question explicitly — it is a
design change, not a fix.

## Agent hardening (queued)

### 1. The agent should hold no credentials (auth proxy) — now Phase 1 of the multi-environment program

It holds exactly one today: its own Anthropic OAuth token at
`~/.claude/.credentials.json`, mode 600, on the `taste-agent-home` volume.
Everything else is already gone — no ssh keys, no git credential helpers,
no host home. The sharp edge is that the adapter NATIVE Bash runs in the
agent container, beside that file: brokered commands go to the
devcontainer through `ide_exec`, but the agent own shell does not, so
anything it decides to run locally executes next to the token.

Design: the IDE holds the credential and the agent never sees a usable
one. Point `ANTHROPIC_BASE_URL` at a loopback endpoint the IDE serves,
give `ANTHROPIC_AUTH_TOKEN` a placeholder, and inject the real
Authorization header on the way out. Viable against the pinned adapter
rather than hoped: `ANTHROPIC_BASE_URL` heads its `PROVIDER_ROUTING_ENV_VARS`
list in `dist/acp-agent.js`, alongside `ANTHROPIC_AUTH_TOKEN` and
`ANTHROPIC_CUSTOM_HEADERS`, and it honours per-session `_meta` env
overrides — so this is the mechanism the adapter already expects, not a
trick played on it.

Settle before writing code:

- **Sign-in moves to the IDE.** The OAuth flow currently runs in the agent
  (login TUI in a console tab, plus the URL bridge). Under a proxy the IDE
  owns it. Probably better — one place, and the confirmation dialog already
  exists — but it replaces a working flow, so it is a UX decision rather
  than a detail.
- **Streaming.** Responses are SSE; the proxy has to stream rather than
  buffer, or it costs latency and memory both.
- **How much the proxy polices.** Blind forwarding lets the agent spend the
  user token on anything it likes. The proxy is the natural place for
  limits and for a record of what was spent — but that is scope to decide,
  not to assume.
- **The other agents.** The registry carries Gemini CLI and Copilot, each
  with its own auth. Either a proxy per provider, or accept that only the
  first-class agent gets this and say so out loud.

The payoff is that "the agent has no credentials" becomes literally true
instead of nearly true, and a prompt injection or a compromised adapter
dependency has nothing left to take.

### 2. The agent container should be SELinux-confined like the devcontainer

It runs `--security-opt label=disable`; the devcontainer runs as
`container_t:s0:c91,c841` against a matching workspace label. The less
confined of the two is the one holding the credential.

Not a one-liner, which is why it is queued rather than done. The agent
binds `CLAUDE.md` straight out of the workspace, and the workspace carries
the devcontainer PRIVATE MCS categories — so dropping the flag makes that
bind unreadable, while `:z` on it relabels a workspace file shared, which
is exactly the label-stomping fixed in e0e1372.

The way out is to stop binding from the workspace at all. The stand-in
workspace is already assembled per session, so copy the agent-context
files into it at spawn instead of bind-mounting them. Then every bind the
agent container holds comes from the IDE own cache, `:z` on those is safe,
and `label=disable` can go. Cost: a snapshot rather than a live view,
which matters little — an agent reads its instructions once, at startup.

Needs a real podman to test, and the failure mode is an agent that will
not spawn.

### 3. Bound a runaway build step

`--cap-drop=all` and `--memory` are on the build (93c7354). `--pids-limit`
was too, and was wrong: it is a `podman run` flag, not a `podman build`
one, so it would have failed every build and stranded the IDE in safe mode
— where there is no exec target, and therefore no way for an agent to
repair it. Caught by reading `podman build --help` before reloading rather
than after.

So a `RUN` step that forks without limit is still unbounded. `--ulimit`
looks like the substitute; verify it appears in `podman build --help`
before adding it, and note that the whole class of build-time flags
deserves that check — the failure mode is not a warning, it is a
devcontainer that will not start.

## Near-term features

0. **Multi-chat tabs** (user-requested, designed, next up). The Chat tab
   strip becomes an AdwTabView: a + button opens a new chat (fresh ACP
   session, same agent), tabs close (ending their session; the wire and
   UI teardown all exist on ChatPane already). Each tab IS a ChatPane —
   session, transcript, composer, and per-session settings travel
   together, which the mode/model semantics already assume. Window-level
   routing (state persistence, sign-in completion, destroy-session
   toast, commit-message suggestions) addresses the SELECTED page's
   pane. Persist the session-id LIST in WorkspaceState (open_chats:
   Vec<...>, additive-compatible with the existing single field).
   Also queued from review: commit box appears when staged > 0 (any
   view), accent on the non-zero Staged count, auto-switch to Staged
   after a bulk Stage.

1. **Fork / rewind from a transcript point** (user-requested). The adapter
   advertises `sessionCapabilities: fork`. Plan: per-prompt-card menu —
   "Fork conversation from here" creates a forked session via ACP and
   switches the pane to it; "rewind code" maps to Claude Code checkpoints
   when the adapter exposes them over ACP (probe first; the schema side is
   unstable). Transcript cards already track their prompt index.
2. **Diagnostics via LSP, curated in-tree.** One bundled language server
   per first-class language (rust-analyzer ships in the devcontainer
   already), inline squiggles + a problems row in the console. No plugin
   surface: language support is a curation decision.
3. **Richer subagent visibility**: nested tool cards per Task invocation.
4. **Chat polish parity**: syntax highlighting inside diff cards, styled
   terminal-output sections on tool cards, thought-duration labels.
5. **Commit flow completion**: push button surfacing sync state more
   prominently after commit; changed-files funnel counts in the filter
   toggles ("Dirty (7)").

## 2026 bets (superlean, hyperfunctional)

- **The working set is the review unit.** Deepen the Dirty/Staged/Stashed
  filters into a first-class "review before commit" flow: keyboard-driven
  next-change/prev-change walking the changes faces of dirty files.
- **Agent-native affordances over IDE chrome.** Prefer MCP tools +
  transcript cards over new panes: e.g. "explain this diff", "test this
  hunk" as chat slash-commands operating on the working set, not buttons.
- **Session portability.** Sessions already live with the agent; surface
  "continue on another machine" by syncing only ids + the repo (state file
  is already industry-idiomatic JSON).
- **Zero-config service topology.** Lean into systemd + socket activation
  (Services tab) as *the* way projects run daemons; devcontainer templates
  that pair `foo.service`/`foo.socket` ghosts next to `.editorconfig`.
- **Explicitly out**: plugins, themes beyond light/dark, per-project IDE
  scripting, embedded browsers, telemetry.
