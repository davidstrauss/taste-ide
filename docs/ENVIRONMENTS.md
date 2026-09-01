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

| Resource | Today | Multi-env |
|---|---|---|
| Container name | `taste-<root-hash6>` | `taste-<root-hash6>-<env>` |
| Image tag | `<container>-image` | keyed by **config hash**, shared across envs with identical config — N environments must not mean N copies of a 2.4 GB image |
| MCP socket | `<container>-mcp.sock` | one per environment (the socket is the identity — see MCP) |
| Build staging | `<container>` dir | per environment |
| Agent home volume | `taste-agent-home` (machine-global!) | `taste-env-<root-hash6>-<env>-home` |
| Config named volumes | verbatim from devcontainer.json | prefixed per environment, or shared deliberately and documented |

The primary environment keeps today's names, so existing containers and
volumes adopt cleanly.

**Supervision.** One `Supervisor` per environment behind an
`EnvironmentRegistry`; the lifecycle mutex, running-hash, pending flag,
log ring, and watcher all become per-environment by construction rather
than by threading ids through a singleton. Events gain an environment id
(`DevcontainerState`, `DevcontainerPendingChanges`, `DevcontainerLog`);
the primary's id is stable so existing subscribers keep working during
the transition.

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
- Bootstrap pragmatics: the credential continues to be created by the
  existing agent login flow; the IDE reads it from the volume host-side.
  IDE-owned OAuth replaces that later — a UX decision deliberately
  deferred (ROADMAP, Agent hardening #1 notes).
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

0. **Multi-chat tabs** (already designed) — N ChatPanes in an
   AdwTabView, `open_chats` list in WorkspaceState. Pure UI + state
   schema; no environments yet.
1. **Auth proxy** — new crate, per-spawn env injection, placeholder
   tokens. Ships value alone (hardening #1) even before relocation.
2. **Environment core** — EnvironmentRegistry, N Supervisors, per-env
   naming/volumes/sockets/ExecContexts, tagged events, clone lifecycle,
   WorkspaceState v2 (chat ↔ env binding).
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
