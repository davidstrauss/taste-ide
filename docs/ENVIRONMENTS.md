# Multi-environment design

> Approved 2026-08-31. This document is the design of record for the
> multi-environment program; ARCHITECTURE.md sections it supersedes carry
> pointers here and get rewritten as each phase lands. Until a phase
> ships, the code is the old design and ARCHITECTURE.md still describes
> what runs.

## What changes

A workspace stops meaning "one checkout, one devcontainer, one chat." An
open folder maintains an **arbitrary number of named environments**: one
backing each agent chat, plus human-created ones. Each non-primary
environment owns a **git clone of the main checkout** and a devcontainer
built from that clone's config. The main checkout becomes the local
integration point — a mini-GitHub in the sense of *sharing access*, not
in the sense of shared write access (see Git topology).

The clone chain is: GitHub (first) → the user's main checkout (second) →
per-environment clones (third). Agents do their work in the third,
publish branches into the second for the user's review, and never touch
the first.

Decisions locked up front, each elaborated below:

1. **Git topology: mediated publish.** No container ever holds write
   access to shared git. The IDE moves branches between repos host-side.
2. **Agent locus: inside its environment's devcontainer, auth proxy
   first.** This resolves ROADMAP's "where the agent runs" as option C,
   gated on the credential proxy so relocation never puts the Anthropic
   token beside repo-supplied build code.
3. **Supervision: an orchestrator chat plus a fleet view.** Human and AI
   supervision share one surface; sub-chats are ordinary tabs the user
   can also drive by hand, at their own model settings.
4. **Issues: a dedicated ref in the main repo**, written only through IDE
   MCP tools, riding along to GitHub on the *user's* push and never on an
   agent's.

And one correction adopted during review: **safe mode generalizes per
environment rather than disappearing.** The repair loop — an agent
helping define or fix a devcontainer config before it can launch — is
load-bearing and applies to every environment, not just the primary one.

## The environment model

An **environment** is: an identity, a git clone (or the main checkout,
for the primary), a devcontainer supervised from that clone, a mode
(container or safe, evaluated per environment), and zero or one bound
chat.

- **Primary environment.** The main checkout itself. Exists always;
  behaves exactly as the single-environment IDE does today. The editor,
  file tree, and git UI view the primary by default; other environments
  are reached through chats, terminals, the fleet view — and read-only
  watching (below).
- **Agent environments.** Created on demand when a chat wants an
  exec-capable world of its own; one chat ↔ at most one environment. The
  clone is created host-side from the main checkout; the container is
  built lazily on first need, not at chat creation.
- **Human environments.** Same machinery, no chat bound; console
  terminals can attach to any running environment (tabs already label
  their context).

**Where clones live**: `$XDG_STATE_HOME/taste-ide/environments/
<workspace-key>/<env-id>/repo`, bind-mounted into that environment's
container the same way the primary workspace is today. The directory is
IDE-owned state, not user data; the fleet view is its UI and `env_remove`
its lifecycle. Destroying an environment **must** enumerate unpublished
branches (commits not reachable from any `agents/*` ref in the main
checkout) and warn — the clone is the only copy of unreviewed work.

**Identity and naming.** Environments get a stable short id (slug).
Everything currently derived from the workspace-root hash gains the env
dimension:

| Resource | Old scheme | Multi-env (shipped, phase 2a) |
|---|---|---|
| Container name | `taste-<root-hash6>` | `taste-<workspace-key>-<env>` |
| Image tag | `<container>-image` | `taste-img-<build-hash12>` — keyed by **config content alone**, shared across envs with identical config; N environments must not mean N copies of a 2.4 GB image |
| MCP socket | `<container>-mcp.sock` | one per environment (the socket is the identity — see MCP); all bound, shipped 2b |
| Build staging | `<container>` dir | per environment |
| Agent home volume | `taste-agent-home` (machine-global!) | `taste-env-<workspace-key>-<env>-home` |
| Config named volumes | verbatim from devcontainer.json | `taste-env-<workspace-key>-<env>-cfg-<declared>` — namespaced at run time, so no repo-declared cache is shared by accident |

The workspace key is 6 bytes of SHA-256 over the main checkout's path, hex
— the same width the old scheme used, which is what lets the sweep
recognise its leavings. All of it is derived in `taste_core::environment`
and nowhere else.

Two hashes fell out of this and both are needed: the **config hash**
covers the config *plus* the IDE's own mounts (which name this
environment's home volume and socket) and answers "is this container
stale?"; the **build hash** covers the config alone and keys the image.
Keying images off the drift hash would have given every environment its
own copy of a byte-identical image.

Containers and images carry `taste.workspace` and `taste.env` labels, and
reconciliation enumerates by those rather than by a name lookup — a name is
what some build of the IDE computed, a label is the container's own claim.

Naming is uniform from day one — the primary is just the environment
with the reserved slug, not a legacy special case. Containers and
volumes from the old single-environment scheme are not adopted: they are
detected, removed, and reported once (see ARCHITECTURE → Compatibility
posture). Pick up the pieces, don't carry them.

**Supervision.** One `Supervisor` per environment behind an
`EnvironmentRegistry`; the lifecycle mutex, running-hash, pending flag,
log ring, and watcher all become per-environment by construction rather
than by threading ids through a singleton. Events gain an environment id
(`DevcontainerState`, `DevcontainerPendingChanges`, `DevcontainerLog`),
and every subscriber is rewritten to route on it in the same pass — no
untagged compatibility variants, no default-env fallbacks.

## Two modes, per environment

Each environment is in exactly one of the two modes, derived from whether
*its* container is running. Safe mode is unchanged in meaning — writes
confined to the safe-mode scope **of that environment's clone**, no exec
target, the agent runs confined outside a container against a stand-in
workspace — it just applies per environment now:

- A chat whose environment is down, broken, or not yet built runs its
  agent in today's outside-confined topology and can author or repair
  that environment's devcontainer config. This is the bootstrap path for
  every new agent environment: clone, agent up in safe mode, config
  authored/validated, user-consented start, relocate.
- The configuration-authority split is per environment and unchanged:
  the agent authors, the user applies; `devcontainer_reload` names what
  will run and denies when it cannot ask.
- The primary environment's safe mode is exactly today's safe mode.

**The confined-outside spawn path is therefore permanent infrastructure,
not legacy.** Every chat's agent must be spawnable in either topology —
outside-confined (env down) or inside the env's devcontainer (env up) —
and the transition between them is a respawn bridged by the persisted
session id and `session/load`, the same continuity mechanism reloads
already rely on. The chat never restarts; the process does.

## Watching an environment

The user can open any environment and watch its agent work — **read,
never edit**. The fixed pane layout does not change; what the panes are
aimed at does, by explicit action only:

- An "open environment" action on a chat tab and on each fleet-view row
  points the file tree and git views at that environment's clone: its
  branch, its dirty/staged state, live. Switching chat tabs never
  auto-follows — watching is deliberate, and the tree never jumps out
  from under the user.
- **Non-primary environments are read-only to the user.** Tree rows
  carry locks (the safe-mode affordance, reused for a second purpose),
  file operations and stage/discard/commit/push are disabled, and the
  editor refuses saves to foreign-env files. The user's intervention
  path is reviewing published branches or taking over the chat — never
  editing under a running agent, which would race it.
- Files opened from a watched environment become read-only editor tabs
  badged with the environment name, mixed alongside primary tabs rather
  than swapping the whole editing context. The clone gets a workspace
  watcher while (and only while) it is watched, so the agent's edits
  reload clean buffers in place, restyle the tree, and refresh git
  state — the existing "an agent's work shows up like your own"
  machinery, aimed at the agent's own world.
- **Live shells are first-class.** In container mode the IDE serves the
  ACP terminal extension — a change of position, deliberate: the "no
  third route to a process" refusal was written for the outside-confined
  topology and still holds there (safe mode keeps the extension
  unserved, since there is no exec target). Post-relocation the agent
  already runs beside the files, so client-served terminals add
  *visibility*, not authority. Agent-created terminals execute in that
  chat's environment container through its `ExecContext` (agent git
  policy attached) and surface as live **read-only** console tabs
  labeled `env · command`, each with a user-side Kill action — stopping
  a runaway process is supervision, not editing.
- The console enumerates a per-environment **shell roster**: user
  terminals attached to the env (interactive — they are the user's),
  agent terminals (read-only), `ide_exec` jobs (read-only mirrors), and
  the build/lifecycle stream. Honest limit, stated plainly: a process
  the agent spawns without a terminal is not observable — visibility is
  by convention (the adapter prefers client terminals when offered),
  not by ptrace. After relocation that convention covers nearly
  everything the agent runs.
- The git filters earn their keep here: the Dirty view over an agent's
  clone is a live review-in-progress of work not yet published.

## Git topology: mediated publish

**No container ever holds write access to git it does not own.** The
sharp edge in any "local GitHub" design is shared writable git: a
container that can write another repo's `.git` can plant hooks the
user's host-side git later executes — a host-boundary crossing — or
corrupt refs other environments depend on. So all inter-repo git flows
run **host-side, in the IDE, via libgit2** (which executes no hooks),
between two repos only the IDE can see as a pair:

- **Publish** (agent → user): the agent commits in its clone, then calls
  the `publish_branch` MCP tool. The IDE fetches that branch from the
  env clone into the main checkout as `agents/<env>/<topic>`. Explicit
  handoff, no shared mounts, nothing polled.
- **Refresh** (user → agent): an `update_from_main` tool (and fleet-view
  action) fetches the main checkout's branches into the env clone's
  remote-tracking refs; the agent rebases inside its own world.
- Inside a container, the clone's `origin` points at a host path that is
  not mounted; fetch/push from inside simply fail. The existing
  `agent_git_config` push-blocks stay as defense-in-depth.

**Review reuses the git-in-the-tree UI.** Published `agents/*` branches
surface as an inbox — a filter alongside Dirty/Staged with a live count —
whose rows open as diffs against the merge base and whose bulk ops are
merge/delete-branch. Merging into branches the user created happens with
the existing flows. Nothing about review grows a new pane.

**Push to GitHub stays user-only and host-side**, exactly as today. The
issues ref (below) rides along on that push; agent branches do not,
unless the user merges them first — publishing to the world remains a
deliberate human act.

`taste-git` grows the plumbing this needs (all parameterized, no
singleton state): remote management, fetch-from-local-path with explicit
refspecs, arbitrary-ref read/write (`refs/taste/*`), commit-to-ref
without touching HEAD, branch enumeration by prefix, and a push that can
carry an extra refspec.

## The auth proxy (prerequisite for relocation)

Relocating the agent into its environment's devcontainer puts it beside
repo-supplied build code. The one thing it holds that repo code must
never read is its Anthropic token — so before any relocation, the token
moves to the IDE:

- The IDE runs a loopback HTTP proxy (rustls; no openssl inside
  Flatpak). Agent environments get `ANTHROPIC_BASE_URL` pointing at it
  and a **per-environment placeholder token**; the proxy swaps in the
  real Authorization header on the way out and streams SSE responses
  without buffering.
- The placeholder doubles as identity: the proxy knows which environment
  is spending, giving attribution and per-environment revocation for
  free.
- **Both halves use documented mechanisms, deliberately.**
  `ANTHROPIC_BASE_URL` is Anthropic's own way to "route requests through
  a custom API endpoint", and `ANTHROPIC_AUTH_TOKEN` is documented for
  "routing through an LLM gateway or proxy that authenticates with
  bearer tokens". The IDE is that gateway. Nothing here depends on an
  adapter internal, so nothing here breaks when one changes.
- **The credential is one the user provisioned to the IDE**, and the IDE
  reads no other program's credential storage. Two intended surfaces:
  a Console API key (`ANTHROPIC_API_KEY`, no expiry), or the one-year
  OAuth token from `claude setup-token`, which prints to the terminal
  and is saved nowhere — so pasting it into the IDE *is* the sign-in.
  Either is held in IDE state at
  `$XDG_STATE_HOME/taste-ide/anthropic.json`:

  ```json
  {"kind": "oauth_token", "token": "…", "expires_at_ms": 1788250887800}
  ```

  `kind` is `oauth_token` or `api_key`; `expires_at_ms` is optional
  because `setup-token` prints no expiry metadata.
- **There is no OAuth refresh, by construction.** A year-long token and
  a non-expiring key both outlive any session, so the problem dissolves
  instead of being solved: no token endpoint, no client id, no refresh
  grant. A known expiry is refused with an error naming the fix, and an
  upstream 401 drops the cache so a re-provision lands without an IDE
  restart.
- Deferred: **IDE-owned sign-in UX**. Today provisioning is a file the
  user writes; the IDE should eventually walk them through it. That is
  a UX gap, not a design gap — the credential already belongs to the
  IDE either way.
- Gemini/Copilot: the proxy is per-provider machinery. Until theirs
  exists, those agents do not relocate; they keep the outside-confined
  topology. Say so in the UI rather than pretending.

## MCP: the socket is the identity

The MCP server today cannot tell which caller is which, and the wire has
no room for identity without changing every client. So: **one socket per
environment**, all served by the one workspace `McpServer`, with the
environment id attached at accept time. Tools route on it:

- `ide_exec` → that environment's `ExecContext` (and job registry;
  handles stop being a shared namespace). rust-analyzer instances are
  per-environment, spawned in that env's container.
- `devcontainer_*` → that environment's `Supervisor`.
- `publish_branch`, `update_from_main` → that environment's clone.
- `fs/read_*`/`fs/write_*` (ACP side) and `write_allowed` evaluate
  against that environment's clone root and mode.
- Orchestration tools (below) are served **only** on the orchestrator
  chat's socket; other connections don't see them.

The primary environment's socket is the existing path, so current agents
keep working untouched.

**Shipped (2b), with two clarifications the implementation forced.** First,
not every tool routes. `ide_open_files`, `ide_selection`, `ide_open_file`,
`ide_screenshot`, `ide_widget_geometry`, `ide_app_log`,
`ide_permission_log` and `flatpak_*` describe the IDE the user is looking
at, of which there is one; routing them would invent per-environment
editors. The line is: a tool routes when it names a checkout, a container
or a mode. Second, the routing lookup can **fail** — an environment
destroyed under a live connection leaves that connection pointing at
nothing — and it says so rather than answering for the primary. There is no
fallback environment anywhere in this design.

## Supervision: fleet view + orchestrator chat

**Fleet view.** The pinned Containers console tab generalizes into the
environments view: one row per environment — name, mode, container
state, bound chat, current branch, published-branch count, disk
footprint — with per-row Start/Stop/Rebuild/Nuke (the existing actions,
per-supervisor now) and the build log of the selected row. Issue queue
renders here too once issues exist.

**Orchestrator chat.** A distinguished chat session — same ChatPane,
same ACP agent, its own model settings — whose MCP connection
additionally serves orchestration tools:

- `env_list` / `env_status` — the fleet, as data.
- `chat_create { task, agent?, model? }` — creates an environment and a
  chat bound to it, seeds the first prompt, returns the chat id. The new
  chat appears as an ordinary tab; the user can take it over at any
  time.
- `chat_send { chat, text }` / `chat_status { chat }` /
  `chat_transcript_tail { chat }` — drive and observe sub-chats.
- `branches_published` — what's in the review inbox, per environment.

Model choice per level is ACP session config (the adapter already
advertises model options; the IDE already renders them) — the
orchestrator picks its own, and passes a model option when creating
sub-chats. Sub-chat permission prompts still surface in their own tabs
to the user; the orchestrator cannot approve on the user's behalf.

**The orchestrator's environment is the integration workspace.** The
orchestrator is a chat, and chats get environments — its own clone and
container are where sub-agents' work is merged, conflicts resolved, and
the combined result tested, so the user reviews one integrated branch
instead of N raw ones. The flow is the star, always through the hub:

1. Sub-agents publish as usual — `publish_branch` lands
   `agents/<env>/<topic>` refs in the main checkout.
2. The orchestrator's environment pulls those refs down via the same
   `update_from_main` mediation, which therefore carries `agents/*`
   refs and not just the user's branches (a Phase 3 requirement, not an
   orchestrator afterthought).
3. Integration is ordinary agent work inside its own clone: merge,
   resolve with native tools, run the tests in its own devcontainer —
   observable through the same watching and live-shell machinery as any
   environment.
4. The result publishes the only way anything publishes:
   `agents/<orchestrator-env>/integration-<topic>` into the main
   checkout, with the raw per-agent branches still inspectable beneath
   it.

**The star is deliberate: no direct env→env channel, even mediated.**
Everything the orchestrator integrates is first a ref in the user's
checkout, so the user's visibility is total and unpublished-work
accounting on destroy stays simple. The orchestrator's environment holds
no special git authority — the extra capability rides on its MCP socket,
never on its clone.

## Issues: a ref, not a service

Issue tracking lives at `refs/taste/issues` in the main checkout: one
file per issue (markdown + front-matter: state, assignee-environment,
links to published branches), committed to that ref host-side without
touching HEAD or the index. The MCP tools (`issue_list`, `issue_create`,
`issue_update`, `issue_comment`) are the only write path for agents; the
fleet view renders the queue for the human.

Durability rides the user's own push: the IDE's push includes
`refs/taste/issues:refs/taste/issues` alongside the branch. Fetch/sync
picks up the remote ref the same way. Agents never push it anywhere.

**The lifecycle the tools must carry** (the loop is: the user and the
orchestrator write issues; worker agents — any ACP agent, any lab —
pick them up; the orchestrator closes them once the work is merged):

- **Claiming is compare-and-swap.** An agent claims by `issue_update`
  setting the assignee-environment; the ref's CAS makes a double-claim
  impossible by construction — the second writer fails, re-reads, and
  sees it is taken. Push dispatch (`chat_create` seeded from an issue)
  and pull dispatch (a worker browsing `issue_list` and claiming) are
  the same tools used in different directions.
- **Closing requires verified mergedness, not belief.** The orchestrator
  may mark an issue done only after checking that the published branch
  is reachable from the user's target branch — the merge-base/
  reachability primitives exist in taste-git and are exposed over MCP
  precisely so "the work is merged" is a query, never an assumption.
- **The user authors in the fleet view.** The issue queue is not just
  rendered there; it carries a composer (intervention-panel convention,
  no modals), because the user writing issues is half the point.
- Worker agents from other providers participate fully — issues, publish,
  per-env exec are all IDE-served MCP, agent-agnostic. Their one
  asymmetry is auth: no proxy for their providers yet, so they keep
  their own credentials and the outside-confined topology.

## Trust model deltas

Restated against ARCHITECTURE.md's trust model, which otherwise stands:

- **The host boundary is unchanged and still the line.** Environment
  clones are IDE-owned state directories; a container sees exactly one
  host path — its own clone — plus its own sockets and volumes. Nothing
  gains reach into `$HOME` beyond what the workspace bind already meant.
- **"Read-only remote git" refines to: real remotes are read-only,
  inter-repo flows are IDE-mediated.** Agents gain no credentials and no
  push targets; "push" is a tool call the IDE fulfills by fetching.
- **"The agent holds no credentials" becomes literally true** (proxy) —
  and is a *prerequisite*, not a follow-up, because relocation without it
  is a regression against today's accidental-but-real separation of repo
  code from the token.
- **One principal per environment, not one principal globally.** Agent
  and repo code remain one principal *within* an environment; separate
  environments are separate worlds that meet only through the review
  inbox and the issues ref, both IDE-mediated. An agent environment gone
  hostile can burn its own clone and its own container, and nothing
  else.
- **The ACP terminal extension becomes served in container mode**
  (unserved in safe mode, as today). ARCHITECTURE.md's "no third route
  to a process" holds where it was argued — the outside-confined
  topology. Inside an environment the agent already executes beside the
  files; the extension trades nothing and buys the user live visibility
  of every command the agent runs.
- **Orchestration tools are execution authority** — `chat_create` spawns
  an agent that will run code in a container. They are confined to the
  orchestrator's socket, and environment/container creation stays
  subject to the same user-consent gates as today's `devcontainer_reload`
  (the config being applied is named; denial when the UI cannot ask).

## Resource policy

- Lazy everything: clone on environment creation, container build on
  first need, agent spawn on first prompt.
- Image dedup by config hash (the common case: every env of a workspace
  shares one image).
- Idle-stop: environments with no chat activity and no running exec for
  a configurable-by-convention interval get their container stopped
  (state survives; restart is cheap). A soft cap on concurrently
  *running* environments, surfaced in the fleet view rather than
  silently enforced.
- Disk honesty: the fleet view shows per-environment footprint (clone +
  target + volumes); `env_remove` reports what it frees.

## Phases

Detailed sequencing lives in ROADMAP.md. In outline:

0. ~~**Multi-chat tabs**~~ — **shipped.** N ChatPanes in an AdwTabView,
   `open_chats` list in WorkspaceState (v2; the single-chat fields are
   gone). Tabs restore lazily — a remembered chat connects on first
   selection, never at startup — which is the same laziness phase 2
   needs for environments. Every chat shared the one workspace MCP
   socket; 2b split it.
1. **Auth proxy** — new crate, per-spawn env injection, placeholder
   tokens. Ships value alone (hardening #1) even before relocation.
2a. ~~**Environment core**~~ — **shipped.** `EnvironmentRegistry` owning N
   `Supervisor`s, identity injected rather than derived; all derived names
   in one `taste_core::environment` module; per-env volumes, ExecContexts,
   staging and sockets; images keyed by build hash and shared;
   `taste.workspace`/`taste.env` labels with reconciliation by label; the
   clone lifecycle (`create` clones with libgit2, `destroy` enumerates
   unpublished work first); old-scheme containers and images swept and
   reported once; tagged devcontainer events with every subscriber
   rewritten; WorkspaceState v3 (`ChatEntry::environment`, environment
   metadata), discarded not migrated. The MCP server and the Containers tab
   still act on the primary only, and the server still binds only the
   primary's socket.
2b. ~~**Environment surfaces**~~ — **shipped.** One MCP socket per
   environment, the environment attached at accept time, and every
   environment-facing tool routing on it (IDE-facing ones deliberately do
   not — see MCP above); `taste_acp::AgentAim`, which turns a chat's
   binding into the checkout, socket and mode a spawn needs, so an agent
   follows its environment without the spawn path knowing what an
   environment is; the per-chat "Give This Chat Its Own Environment"
   affordance that clones off the main thread, records the binding in
   `ChatEntry::environment`, respawns the chat's agent against the new aim
   (`session/load` carrying the conversation), and names the environment in
   the tab. Binding is one-way and closing a tab does not destroy its
   environment — the clone is the only copy of that agent's work, and
   environment lifecycle belongs to phase 5. `WorkspaceState::environments`
   stays deliberately **unwritten**: its documented job is what the disk
   cannot say — a human name — and there is no naming UI yet; filling it
   with slugs the clone directory already carries would make it a second
   inventory that can disagree with the first.
3. **Mediated publish + review inbox** — taste-git plumbing, publish/
   update tools, the agents/* filter in the file tree.
4. **Relocation** — spawn inside the env container when Running,
   outside-confined fallback (per-env safe mode), session/load bridge;
   serve the ACP terminal extension in container mode (live read-only
   agent-terminal tabs).
5. **Fleet view + watching** — the Containers tab becomes the
   environments view; read-only environment watching (tree/editor/git
   retargeting, per-env watcher, exec mirrors, the per-env shell
   roster).
6. **Orchestrator** — orchestration tools on a distinguished chat,
   per-level model config.
7. **Issues** — the ref, the tools, the push ride-along, fleet queue.

Each phase lands green (`cargo test --workspace` in the devcontainer),
updates ARCHITECTURE.md for what it changed, and is independently
useful.
