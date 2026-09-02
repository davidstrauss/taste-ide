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

Done in the chat/composer pass (2026-08-31), against the "interface must
be beautiful" rule:
- **The composer's keyboard contract is one predicate**, `composer_key`,
  and it is unit-tested — because two of its three rules are invisible
  until they are wrong in front of somebody. Enter no longer fires
  mid-IME-preedit: it was committing the composition AND sending, which
  truncates the message mid-word on ordinary typing for every CJK user.
  Escape stops a running turn and does nothing otherwise (never clears
  the composer — that would throw away typing nobody asked it to). Up in
  an EMPTY composer recalls the last prompt; with text in it Up stays a
  cursor key. Focus follows the work: the caret returns to the composer
  after a send and on a tab switch.
- **Mid-turn sends queue, and the button says so.** The session layer
  already queued them, so disabling Send would have been the dishonest
  choice; it reads "Queue" while a turn runs, with Stop beside it. Typed
  text is never lost either way.
- **The focus ring is the platform's** (`outline` + `outline-offset`),
  not a hand-rolled 1px border — so it follows the theme's ring, a
  custom accent and high-contrast, which a hard-coded colour cannot see.
- Dropped files become attachments; chips are pills with the remove
  affordance AFTER the label (it led, and read as a bullet), focusable
  and Space-activatable, with an accessible label naming what they
  remove. Composer growth is stated in LINES (8) against the font in
  use, not a pixel count that meant five lines at one text size.
- The empty page names the agent actually selected — it said "Ask Claude
  Code" over a Gemini session — and carries one plain line about what
  the agent can reach.
- Tool titles are explicitly normal weight: the card header is a
  GtkButton and Adwaita bolds button labels, so a shell command was
  outweighing the agent's prose. A contentless card is `can-target:
  false` rather than insensitive — insensitive dims the label, and a
  failed call that produced no output is precisely the card that must
  not read as faded.

Standing conventions to keep honoring:
- Disable, never hide; no modal dialogs for dirty-file workflows (bottom
  intervention panel); toasts for outcomes; StatusPage for empty states;
  destructive styling + confirmation only for destructive actions.
- ~~Known debt: the file pane header is crowded (new-file, three git
  filters, ignored toggle)~~ — paid in phase 3b: the Inbox filter was the
  one more control, and the ignored-files eye moved up beside the
  search-ghosting toggle (both are listing choices), leaving the filter
  group its own row. The chat "Auto-approve" switch overlaps
  conceptually with the agent's Auto mode — consolidate when the ACP
  permission story settles. One stray `GtkGizmo snapshot without
  allocation` warning remains unexplained (cosmetic; watch it) — it did
  NOT appear in any probe run of the chat/composer pass, including runs
  that build tool cards, diff views, terminal views and the permission
  banner, so whatever raises it is not in the transcript's widget tree.

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

0. ~~Multi-chat tabs~~ — **shipped, then superseded**: there is no tab
   strip any more, one chat per environment. See Near-term feature 0
   below and ENVIRONMENTS.md → Phase 0.
1. ~~Auth proxy~~ (Agent hardening 1 below) — **shipped, on by default.**
2. ~~Environment core: registry, N supervisors, per-env
   naming/volumes/sockets/ExecContexts, tagged events, clone lifecycle,
   WorkspaceState v2.~~ — **shipped.** See ENVIRONMENTS.md → Phases 2a/2b.
3. ~~Mediated publish + review inbox (taste-git plumbing, publish/update
   MCP tools, agents/* filter).~~ — **shipped, and the inbox half since
   replaced by the review lifecycle.** See ENVIRONMENTS.md → Phases
   3a/3b and 9.
4. ~~Agent relocation into the env container, outside-confined safe-mode
   fallback, session/load bridge; the ACP terminal extension served in
   container mode, with the per-environment shell roster behind it.~~ —
   **shipped.** See ENVIRONMENTS.md → Phases 4 and 4c.
5. ~~Fleet view (Containers tab → environments view), gadget mode, the
   varlink service and notifications.~~ — **shipped.**
6. ~~Orchestrator chat + orchestration MCP tools, per-level models.~~ —
   **shipped**, and with it the program's original vision. See
   ENVIRONMENTS.md → "Supervision: fleet view + orchestrator chat".
7. ~~Issues ref + tools + user-push ride-along.~~ — **shipped.**

## Where the agent runs — RESOLVED

> **Resolved 2026-08-31** as option C (relocation), gated on the auth
> proxy, by the multi-environment program above. The analysis below is
> kept because its reasoning — especially the trust question and the
> history-keying pitfalls — constrains the implementation.
>
> **SHIPPED (phase 4).** All three "What C requires" items are handled by
> making each a property of a *value* rather than of a code path, so no
> spawn can get one right and another wrong:
>
> 1. **cwd** — the workdir is the environment's checkout at its REAL host
>    path, which the supervisor's double bind already provides for clones
>    too. The same string in both topologies, so the adapter's
>    `listSessions({dir})` key does not move.
> 2. **HOME** — both topologies mount the same per-environment volume at
>    the same path (`env_home_volume` → `AGENT_HOME_IN_DEVCONTAINER`). It
>    survives rebuilds by being a volume. The machine-global
>    `taste-agent-home` this section worried about is gone entirely.
> 3. **Path translation** — none needed, falling out of (1). The class of
>    bugs stays avoided by construction rather than mapped around.
>
> The trust question is settled the way this section demanded: the
> credential is not in the container at all. The auth proxy holds it and
> the agent gets a per-environment placeholder, so "keep the credential
> store out of reach of anything the repo supplies" is satisfied without
> needing the second-uid scheme sketched below. That idea is now
> unnecessary rather than deferred.
>
> One thing the analysis did not anticipate, found by running it: on an
> SELinux-enforcing host a confined container is refused `connectto` on a
> socket served by the unconfined IDE, so a relocated agent could not
> reach the MCP socket or the proxy however they were mounted. Relocation
> was refused there for one batch. **Since fixed by inverting the
> direction**: the container's own helper binds both endpoints and
> multiplexes them to the IDE over `podman exec` stdio, so the only
> connections are container-to-container and no IDE socket is mounted in
> at all. Proven live on an enforcing host against an ordinary confined
> container. See ENVIRONMENTS.md → Relocation.

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

Safe mode settled itself either way, at the time this was written: with no
devcontainer there was nowhere to relocate to, so the agent stayed outside
with the stand-in workspace, no exec, and `write_allowed` genuinely
confining. That premise is gone — the baseline environment (phase 8) gave
safe mode a devcontainer of its own, so the agent relocates there too now;
see ARCHITECTURE.md → "The two modes" for the current design.

### Recommendation

A first: additive, preserves the live-buffer property, leaves C available.
If A proves frictional in practice, C is the principled retreat, not B.
Do not ship C without settling the trust question explicitly — it is a
design change, not a fix.

## Agent hardening (queued)

### 1. The agent should hold no credentials (auth proxy) — now Phase 1 of the multi-environment program

> **Status: ON BY DEFAULT — verified live 2026-08-31.** The user
> provisioned a `claude setup-token` credential and the live round-trip
> (`taste-acp/tests/live_proxy.rs`) passed: EndTurn with 713 input / 18
> output tokens counted *by the proxy*, zero unrecognized credentials,
> one benign unauthenticated probe (`HEAD /api/hello`) refused at no
> cost. Two live findings are now regression-tested: the proxy declines
> `Accept-Encoding` (compressed bytes blind the usage scanner), and
> refusals are split into unauthenticated (expected) vs unrecognized
> (always a bug). `TASTE_AUTH_PROXY=0` opts out for proxy debugging.
> Remaining honesty: the adapter's own login file may still exist on the
> agent-home volume from before; the placeholder outranks it (documented
> precedence), and it stops existing at relocation (fresh per-env homes).
> See ENVIRONMENTS.md → "The auth proxy" for the design of record; the
> four questions below were settled there.
>
> Settled: the proxy streams; it records per-environment spend (requests,
> bytes, and the Messages API's own `usage` counters) but enforces no
> limits; Claude Code only — Gemini and Copilot keep their own auth and
> do not relocate.
>
> **Subscription usage is visible (shipped).** Being the last hop, the
> proxy reads the account's rate-limit headers off responses it was
> already carrying, and a 429 as the authoritative "closed until". One
> workspace-global snapshot, stamped with when it was read, surfaced as a
> gauge in the environments panel header and a Subscription section in the
> chat's Utilization tab; per-environment spend is the breakdown under it.
> Nothing is ever requested to refresh it — see ENVIRONMENTS.md → "The
> auth proxy" → Subscription usage. What that costs is honest and
> permanent: no traffic, no reading.
>
> **Verified live 2026-09-01** on the provisioned `setup-token`
> credential: a real turn came back with `anthropic-ratelimit-unified-*`
> carrying a five-hour utilization, a `-7d-` weekly utilization, resets
> as epoch seconds, a `status`, and a `representative-claim` naming which
> window the unnamed family speaks for — and with **none** of the
> documented per-minute headers, which are API-key traffic. That family
> is undocumented, so the parse is shape-matched, pinned by a unit test
> to those exact headers, and anything unrecognised is kept verbatim
> rather than guessed at. If the shape changes, the gauge goes quiet
> instead of lying.
>
> **The credential is one the user provisions to the IDE** — an
> `ANTHROPIC_API_KEY`, or the one-year token from `claude setup-token` —
> held in IDE state at `$XDG_STATE_HOME/taste-ide/anthropic.json`. An
> earlier revision instead parsed Claude Code's own
> `~/.claude/.credentials.json`. That was a mistake and is gone: it is
> another program's private storage, not an interface, and reading it
> both coupled us to an undocumented shape and made the IDE a second
> consumer of a grant issued to a different client. A guard test keeps
> it from coming back.
>
> **OAuth refresh will not be implemented.** It briefly looked necessary
> — an `/login` access token lasts ~8h — and the route to it ran through
> an undocumented token endpoint and client id lifted out of a minified
> bundle. Both the mechanism and the need evaporate with a provisioned
> credential: an API key does not expire and a `setup-token` token lasts
> a year, so there is no token endpoint here, no client id, and no
> refresh grant. Known expiry is refused with a message naming the fix;
> an upstream 401 drops the cache so a re-provision lands without a
> restart.
>
> **Two things gate flipping the default**, and neither is code:
>
> 1. **A provisioned credential.** Turning the proxy on without one
>    turns a working chat into a broken one, since the agent's own
>    credential is no longer the fallback. IDE-owned sign-in UX (a
>    prompt, not a hand-written file) should land first.
> 2. **A live turn through it.** `cargo test -p taste-acp --test
>    live_proxy -- --ignored` is that check; it is `#[ignore]`d because
>    it spends real tokens. Its assertion is that the proxy's spend
>    counters *moved*, because a turn that merely succeeds proves
>    nothing — an adapter that bypassed the proxy succeeds identically.
>
> Partial evidence already in hand: with the proxy active, the pinned
> adapter does send `POST /v1/messages` to `ANTHROPIC_BASE_URL`, so the
> routing half is real. The end-to-end assertion has not been run
> against a provisioned credential yet, which is why the default stays
> off.

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
Authorization header on the way out. This is the documented mechanism
rather than a trick played on the adapter, which is what makes it safe to
depend on: Anthropic documents `ANTHROPIC_BASE_URL` as the way to "route
requests through a custom API endpoint", and `ANTHROPIC_AUTH_TOKEN` as
being for "routing through an LLM gateway or proxy that authenticates
with bearer tokens". The IDE is that gateway. Depending on documented
behaviour rather than on a bundle's internals is also why this survives
an adapter release.

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
one, so it would have failed every build — the baseline's included — and
stranded the IDE at the rung below safe mode, with no exec target at all
and therefore no way for an agent to repair anything. Caught by reading
`podman build --help` before reloading rather than after.

So a `RUN` step that forks without limit is still unbounded. `--ulimit`
looks like the substitute; verify it appears in `podman build --help`
before adding it, and note that the whole class of build-time flags
deserves that check — the failure mode is not a warning, it is a
devcontainer that will not start.

## Near-term features

0. ~~**Multi-chat tabs**~~ — **DONE, then SUPERSEDED** (2026-09-01; see
   ENVIRONMENTS phase 0 and "Watching an environment"). The chat pane was
   an AdwTabView of ChatPanes. It is now one chat per environment with no
   tab strip at all: the pane shows the environment the panel has
   selected, and a new conversation is a new environment. Session,
   transcript, composer, model, permission mode and auto-approve still
   travel together — with the environment now, which is the chat's
   identity. `WorkspaceState` is v5: chats keyed by a required
   `ChatEntry::environment`, no `active_chat` (which chat is on screen
   follows the selection, and that is never persisted). Alpha rules
   throughout — a stale file is discarded and the reset is toasted, never
   migrated. Restore is still lazy: a remembered chat connects the first
   time its environment is selected. Landed alongside it: the permission mode is now re-applied
   to *every* session a chat connects (default `auto`), restored ones
   included, through the mode config option when no modes state is
   advertised — the two halves of "auto never stuck".
   Still queued from that review: commit box appears when staged > 0 (any
   view), accent on the non-zero Staged count, auto-switch to Staged
   after a bulk Stage. Still pending from the UX ledger: consolidating
   the two overlapping permission controls (the client-side Auto-approve
   switch and the agent's own mode) into one.

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
4. ~~**Chat polish parity**~~ — **DONE**. Diff cards render through the
   GtkSourceView already in the binary, language guessed from the path and
   the Adwaita scheme following the dark preference; the path moved out of
   the buffer into a caption, where it is no longer syntax-highlighted as
   if it were code. `Execute` tool calls render their output as terminal
   output — monospace, a dim `currentColor` wash, ANSI SGR read into text
   tags against GNOME Console's palette, and every unrecognised escape
   DROPPED rather than printed (a cargo log was showing its `[2K[1G`
   bytes literally in a wrapped prose label). A finished thought reports
   how long it took instead of saying "Thinking…" for the rest of the
   session.

   Found while doing it, and fixed with it: tool cards *appended* each
   content update rather than replacing it, though ACP sends content as a
   snapshot and agents restate the whole of a shell call's output on every
   update — so a card grew by a copy of itself per update and the
   transcript jumped under the reader for the length of a turn. Cards now
   rebuild only on a changed snapshot, guarded by a signature whose
   restated-case comparison measures 19–153µs from 15–255 KiB of output
   (`cargo test -p taste-app perf_ -- --ignored --nocapture`), against the
   full parse-and-rebuild it replaces. Status classes were added and never
   removed, so a call that failed after reporting progress kept `success`;
   an in-flight call showed the same static glyph as a finished one and
   now spins.

   Not done, and deliberately: **ANSI beyond SGR colour/bold/dim**.
   Cursor movement, erase and scroll regions need a screen model — that is
   a terminal, and the console already has one in VTE. Tool output is a
   transcript of what happened, not a live screen, so the escapes are
   dropped instead of half-honoured.
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
