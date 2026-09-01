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
| MCP socket | `<container>-mcp.sock` | one per environment (the socket is the identity — see MCP); all bound, shipped 2b. A **relocated** agent reaches the same server through its environment's channel instead, at `/tmp/taste-ide-<env>/mcp.sock` inside its container — see Relocation |
| Build staging | `<container>` dir | per environment |
| Agent home volume | `taste-agent-home` (machine-global!) | `taste-env-<workspace-key>-<env>-home` |
| Config named volumes | verbatim from devcontainer.json | `taste-env-<workspace-key>-<env>-cfg-<declared>` — namespaced at run time, so no repo-declared cache is shared by accident |

The workspace key is 6 bytes of SHA-256 over the main checkout's path, hex
— the same width the old scheme used, which is what lets the sweep
recognise its leavings. All of it is derived in `taste_core::environment`
and nowhere else.

Two hashes fell out of this and both are needed: the **config hash**
covers the config *plus* the IDE's own mounts (this environment's checkout
and its home volume — no socket rides in any more, see Relocation) and
answers "is this container stale?"; the **build hash** covers the config
alone and keys the image.
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

## Relocation (shipped, phase 4)

A chat whose environment has a container running spawns its agent **inside
that container**, via `podman exec` (through `flatpak-spawn --host` when the
IDE is sandboxed). `taste_acp::AgentAim` stays the address and gains
nothing about topology; `taste_acp::relocate` is the topology.

**The conversation survives the move because nothing addressable changes.**
Each of ROADMAP's three pitfalls is defused by a value being identical on
both sides rather than by a code path remembering to translate:

- **Working directory**: the environment's checkout at its REAL host path.
  The supervisor's double bind already mounts it there, clones included, so
  the adapter's `~/.claude/projects/<flattened-cwd>/` key does not move.
- **`HOME`**: this environment's home volume, mounted at `/home/agent` in
  both topologies. It is a volume, so it outlives container rebuilds; it is
  per environment, so two agents never share a history. (The old
  machine-global `taste-agent-home` is gone — it put every workspace's
  agent in one directory, and an existing one is not adopted.)
- **Path translation**: none, which falls out of the first.

**The socket direction is inverted, and that is what makes relocation work
at all.** (Shipped as phase 4's sibling batch; the paragraph it replaces
described mounting the IDE's sockets in, which never worked on an
SELinux-enforcing host.)

The IDE used to bind its sockets — one MCP socket per environment, one auth
socket per workspace — and bind-mount them into the container at their host
paths. Mounting succeeded and dialling did not: a `container_t` process is
refused `connectto` on a socket whose listener is the unconfined desktop
app, so the file was readable and `connect(2)` returned `EACCES`. `:z`
relabels the socket `container_file_t` and changes nothing, because the
denial is about the listener's domain, not the file's label. Two things
*are* permitted, both verified live: a container may dial a socket it bound
itself, and the unconfined IDE may dial a socket a container bound.

So the endpoints moved inside. Per environment with a container up, the IDE
runs one **channel helper** — `podman exec -i <container> node -e …` — which
binds `/tmp/taste-ide-<env>/mcp.sock` and `.../auth.sock` in the container
and multiplexes every connection it accepts over its own stdio back to the
IDE. The agent's MCP stdio bridge and its auth forwarder dial those, which
is container-to-container and permitted. On the IDE side each demultiplexed
connection is handed to `McpServer::serve_stream` or
`AuthProxy::serve_stream` — the same servers, a different door.

Why `podman exec` stdio and not a socket the container binds in a shared
mount (which SELinux also permits): the exec pipe is one the IDE already
owns and already depends on — it is how the relocated agent speaks ACP —
and it needs no mount, no rendezvous protocol and no connection pool to
arrive at the same place. Measured byte-exact for 200 KB of random data.

Why it multiplexes rather than one exec per connection: `podman exec` costs
~190 ms. MCP would survive that (one agent, one long-lived connection); the
auth path would not, since hyper pools connections and an SSE turn holds one
open, so every request would pay it on the path the user watches token by
token. One exec per *environment* pays it once per container.

The framing is nine bytes — `u32` channel, `u8` kind, `u32` length — with
open/data/close, backpressure honoured in both directions, and a **closed
set of two service codes**. `Open` only ever travels container→IDE.

**Identity is unchanged, and unchanged by construction.** "The socket is the
identity" generalizes to "the channel is": the IDE attaches the environment
at the demux because it knows which container it exec'd into, exactly as it
used to attach it at `accept`. Nothing a client sends names an environment,
and a container can ask for one of two services and nothing else.

**What the container sees of the host is now its checkout, and nothing
else.** Dropping the two socket mounts also drops them from the config
hash, which correctly makes every previously running container stale once.

**The auth forwarder is unchanged** except for which socket it dials. It
takes an **ephemeral** port and starts the agent from inside its `listen`
callback with `ANTHROPIC_BASE_URL` pointing at it: no fixed port to collide
with what the repo runs, and no race to lose. The proxy's placeholder model
is untouched — one workspace-wide auth service, because the auth wire
carries its own identity in the placeholder token, unlike MCP where the
channel *is* the identity.

**Conventions a devcontainer must meet to host an agent**, checked once per
container and reported rather than assumed:

- **It carries `node`.** Every ACP adapter here is a node program, and so
  are the MCP bridge and the auth forwarder. The IDE does not install it —
  the image belongs to the repo.
- **The agent home is writable.** Podman hands a brand-new named volume to
  container-root when the image has nothing at that path; the IDE chowns it
  once, as container-root, which under rootless podman is the user's own
  uid seen through the userns.
- **The IDE answers through its channel.** Not "is the socket there" — the
  helper just bound it. Each service is made to reply as itself: MCP gets a
  JSON-RPC `ping` and must return a result carrying the id, the auth proxy
  gets a credential-less request and must return its own 401. That proves
  the whole path — helper, framing, demux, the IDE's own server — and costs
  no token and no upstream call. Only services the IDE actually offers are
  probed, so `TASTE_AUTH_PROXY=0` does not fail an environment for a door
  nobody opened.

Any of these unmet, and **relocation is refused**: the chat keeps the
outside-confined topology, which works everywhere, and says why in the
transcript. Weakening the devcontainer's confinement was never on the table
— it is the container the repo's own build code runs in — and with the
direction inverted it is not needed: verified live on Fedora 44 with
`getenforce` reporting `Enforcing`, against an ordinary confined container
with no `label=disable`, no policy module and no relabelling. The agent
relocates, `ide_environment` answers as its own environment and names its
own clone, and a turn's API call reaches the upstream with the real
credential swapped in and that environment's spend counters moved.

**Transitions are debounced by settling, not by a timer.** Only settled
lifecycle states move an agent, so a rebuild's stop → build → start is one
respawn rather than three, and the reconnect backoff stands down while an
environment is in transition. A topology change arriving mid-turn waits for
the turn to end — moving the process would throw away work the user is
watching. An agent inside a container dies with it, and needs no special
case going down: the existing bounded reconnect brings it back
outside-confined, because that is what the environment now is.

## Watching an environment (shipped, phase 5a)

The user can open any environment and watch its agent work — **read,
never edit**. The fixed pane layout does not change; what the panes are
aimed at does, by explicit action only:

- **Where the panes are aimed is said once, by a permanent strip at the
  very bottom of the file-tree pane** — below the intervention panel and
  below anything else that pane opens, because a context indicator that
  can be displaced by a transient panel is not an indicator. It names the
  current context ("Yours", or the environment), carries its state dot and
  a lock while the view is read-only, and tints itself whenever that
  context is not home, so peripheral vision knows before anything is read.
  Clicking it — or Ctrl+Shift+E — opens the switcher: every environment,
  primary first as the way back, with busy and unpublished-work markers,
  and a filter once the list outgrows reading. It replaced the "Viewing
  `<env>` / Back to Yours" bar the tree header used to grow.
- An "open environment" action on a chat tab and on each fleet-view row
  points the file tree and git views at that environment's clone: its
  branch, its dirty/staged state, live. Those remain, as shortcuts into
  the same transition the strip calls. Switching chat tabs never
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
  than swapping the whole editing context. **The predicate is whose
  checkout the file is in, not what the tree is currently showing** — so
  such a tab stays read-only after the user returns home, and the same
  ownership is what bounds an agent's mediated *write* to a file in its
  own clone (that write is checked against its environment's checkout and
  mode; the window's workspace root was the wrong wall for a file the
  window does not own). The clone gets a workspace watcher while (and only
  while) it is watched, so the agent's edits reload clean buffers in
  place, restyle the tree, and refresh git state — the existing "an
  agent's work shows up like your own" machinery, aimed at the agent's own
  world.
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
  terminals attached to the env (interactive — they are the user's, so
  they carry no Kill button; closing the tab is how they end), agent
  terminals (read-only), `ide_exec` jobs (read-only mirrors), and the
  build/lifecycle stream, which is a roster row of its own mapping to the
  log view. A new terminal opens in the *selected* environment when that
  environment has a container, and in the workspace's own context
  otherwise: a clone with no container resolves to the host, and a shell
  there would claim an environment while showing the user's files. Honest
  limit, stated plainly: a process
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
environment id attached at accept time.

A relocated agent's connections arrive over its environment's channel
rather than on that socket, and the rule generalizes without weakening:
the id is attached at the demux, because the IDE knows which container it
exec'd the far end into. Decided before a byte is read, either way. What
must stay true is the negative — **there is no environment id on the
wire** — and there still is not.

Tools route on it:

- `ide_exec` → that environment's `ExecContext` (and job registry;
  handles stop being a shared namespace). rust-analyzer instances are
  per-environment, spawned in that env's container.
- `devcontainer_*` → that environment's `Supervisor`.
- `publish_branch`, `update_from_main` → that environment's clone.
- `fs/read_*`/`fs/write_*` (ACP side) and `write_allowed` evaluate
  against that environment's clone root and mode.
- Orchestration tools (below) are served **only** on the orchestrator
  chat's socket; other connections don't see them. (Shipped, phase 6.
  The role is one `Option<EnvironmentId>` on the server, written by the
  chat strip; the primary is refused as a holder, on both sides.)

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

**Fleet view (shipped, phase 5a).** The pinned console tab *is* the
environments view: one row per environment — name, mode, container state,
bound chat with a busy indicator, current branch, published-branch count,
an unpublished-work marker, disk footprint and token spend — with per-row
Start/Stop/Rebuild/Nuke (the existing actions, per-supervisor now), Open,
Rename, Destroy, and the selected row's build log, shell roster and podman
resources beneath it. Rows are assembled as **pure data** from the six
places an environment's facts live, so every other surface below renders
rows rather than re-deriving them. Issue queue renders here too once
issues exist.

**Gadget mode: the window is the monitor.** Supervising a busy fleet
should not require keeping a full IDE focused. Below a breakpoint size
(libadwaita `AdwBreakpoint`, the mini-player pattern) the window swaps
its panes for one compact fleet card: per-chat busy indicators,
environment states, the subscription-quota gauge fed by the proxy's
spend counters, and the inbox count. Shrink the window into a corner and
it is a monitor; stretch it back and it is the IDE — one window, layout
commitment intact. A floating always-on-top gadget is deliberately not
attempted: Wayland does not grant apps keep-above, and panes never
float. The companion is **GNotifications for moments needing the user**
— a waiting permission prompt, a turn ended, a failed env build, a
branch arriving in the inbox. Glancing is ambient; action gets a
notification. (Phase 5b — landed. The breakpoint is 520sp, chosen to sit
below every width GNOME's own tiling produces, so gadget mode is entered by
dragging a corner and never by snapping the IDE beside a browser. The card
is a render of the same fleet snapshot the varlink service publishes. The
notification rule, in one line: never notify about the surface the user is
already looking at — window focused AND that surface on screen — with ids
scoped per chat and per environment so two chats needing the user are two
notifications and one chat asking twice is one.)

**Shell integration rides a varlink interface — varlink, not D-Bus, by
decision.** Phase 5 exports the fleet as a varlink service on a unix
socket (named by `taste_core::environment`, IDL checked in-tree):
environment states, busy chats, the quota gauge, the inbox count — the
same data gadget mode renders. It costs little, is testable like every
other socket in this codebase, and is the substrate for a **thin
optional in-tree GNOME Shell extension** (top-bar indicator + fleet
popover, GJS consuming the socket via `Gio.SocketClient`) — a separate
install by nature (extensions cannot ship in a Flatpak) and kept to a
dumb renderer so GNOME version churn touches nothing that matters. The
"no extension mechanism, ever" rule is about extending taste-ide;
taste-ide extending the desktop through the desktop's own intended
mechanism is a different act, done in-tree and curated like everything
else. The rule, stated precisely: **varlink for interfaces we design;
the established contract — D-Bus included — when implementing someone
else's.** So the GNOME search provider (`org.gnome.Shell.SearchProvider2`,
a D-Bus contract) is a legitimate optional surface: overview search
returning live fleet rows, backed by the same data. Ruled out for real:
MPRIS impersonation and AppIndicator routes — misuse of interfaces, not
transports.

The service landed in phase 5b as `taste-fleetlink`. What a GJS client
needs: the socket is `taste-<workspace-key>-fleet.sock` in
`$XDG_RUNTIME_DIR` (glob `taste-*-fleet.sock`; one per open window), the
protocol is stock varlink — NUL-terminated JSON, `more` for streaming —
and the two methods are `List()` and `Watch()`, which return the same
shape. `org.varlink.service.GetInfo` and `GetInterfaceDescription` are
served on the same socket, so a client can discover the whole interface
from the connection rather than shipping a copy of it.

**Orchestrator chat (shipped, phase 6).** A chat the user designates —
same ChatPane, same ACP agent, its own model settings — whose MCP
connection additionally serves orchestration tools:

- `env_list` / `env_status { env }` — the fleet, as data. Literally the
  rows the console assembles and the varlink socket publishes, so the
  orchestrator and the user cannot disagree about what is running.
- `chat_create { task, agent?, model?, issue? }` — creates an
  environment and a chat bound to it, seeds the first prompt, returns
  `{ chat, env }`. The new chat is an ordinary background tab; the user
  can read it and take it over at any time.
- `chat_send { chat, text }` / `chat_status { chat }` /
  `chat_transcript_tail { chat, max? }` — drive and observe sub-chats.
- `branches_published { env? }` — the review inbox, read from the hub.

**The designation is a chat's, but the socket is an environment's, and
that is why an orchestrator must be bound.** Per-environment sockets tell
environments apart, not chats; every chat without an environment of its
own shares the primary's. Serving these tools there would hand
`chat_create` to every unbound chat in the workspace, including ones the
user opened for something else. So the affordance — an "Orchestrator"
switch in the chat's own settings list, one per workspace, reassignable,
persisted in `ChatEntry::role` (state v4) — clones an environment in the
same gesture when the chat has none. Moving the role takes it off the
previous holder first and respawns both chats, because ACP sends the tool
list once per session.

**Chats are addressed by their environment.** `chat_create` returns an id
that *is* the environment id: it already exists, the fleet view shows it,
a person can say it out loud, and it survives a restart, where a tab
ordinal does none of those. `"primary"` is refused as a chat id rather
than resolved, because every unbound chat is "in" the primary and the
name picks out no conversation.

**`chat_create`'s order is the tool:** cap, issue pre-flight, create,
claim, prompt. The two refusals that cost nothing — the concurrency cap
(`taste_core::environment::MAX_ORCHESTRATED_ENVIRONMENTS`, six: soft in
the precise sense that it bounds the tool and not the user's own hand)
and an issue somebody else already holds — happen before a clone exists.
The claim is the real compare-and-swap and can only be made once the
environment it names exists, so it happens *before* the task is sent: a
dispatch that loses the race leaves an idle chat rather than one working
somebody else's issue. Creation-time linking is a *claim*, not an
`issue_link`: links name a branch, and the branch does not exist yet, so
the seeded prompt tells the worker to link its own branch when it
publishes.

There is deliberately **no user prompt per creation**. The gates that
matter are already further in: the environment's container is not
started (a fresh environment is in safe mode, which is where lifecycle
commands get their consent), and the sub-agent's own permission prompts
surface in its own tab. A dialog whose only answer is yes is how consent
gates stop being read.

Model choice per level is ACP session config — the orchestrator picks its
own from the pane's existing controls, and passes a `model` when creating
a sub-chat. The value is applied at the sub-session's `Ready` and
validated against what that session actually advertises; an unknown id is
refused by naming the advertised ones, and the chat is left created and
*unprompted* rather than quietly running on a different model. What the
pinned Claude Code adapter advertises today, read off a live session by
`taste-acp/tests/orchestrator.rs`: option `model` with values `default`,
`opus[1m]`, `sonnet`, `sonnet[1m]`, `haiku` (alongside `mode`, `effort`
and `fast`, which the IDE renders but does not yet let an orchestrator
set per sub-chat).

Sub-chat permission prompts still surface in their own tabs to the user;
the orchestrator cannot approve on the user's behalf, and there is **no
tool that would let it** — `chat_status` reporting `awaiting-permission`
is how it learns to tell the user instead.

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

Phase 6 added no git machinery for this, which was the Phase 3
requirement paying off: `update_from_main` already carries `agents/*`,
and `publish_branch` already works from any environment's clone. The
orchestrator's environment is an environment like any other; what makes
it the integration workspace is the work the user gives it, not a
capability its clone holds.

**The star is deliberate: no direct env→env channel, even mediated.**
Everything the orchestrator integrates is first a ref in the user's
checkout, so the user's visibility is total and unpublished-work
accounting on destroy stays simple. The orchestrator's environment holds
no special git authority — the extra capability rides on its MCP socket,
never on its clone.

## Issues: a ref, not a service

**Shipped.** Issue tracking lives at `refs/taste/issues` in the main
checkout — no database, no server, nothing in the working tree. One
directory per issue: `issues/<id>/issue.md` (front-matter + markdown
body) with comments as sibling files under `comments/`. Three storage
choices are load-bearing:

- **The path is the id.** No `id:` in the front-matter, because two
  places that must agree eventually do not.
- **Comments are files, not appended sections.** Concurrent commenters
  touch disjoint paths, so a compare-and-swap loser re-reads, re-numbers
  and re-applies rather than rewriting someone else's prose — and a
  comment shows up in review as an added file, not a hunk in the middle
  of a paragraph.
- **Ids are short, monotonic and zero-padded** (`i-0001`), allocated as
  one past the highest, inside the retry loop. A UUID would dodge the
  race by being unreadable; humans type these into chat messages.

Five MCP tools — `issue_list`, `issue_create`, `issue_claim`,
`issue_update`, `issue_link` — are served on **every** environment
socket, the primary's included, because the user's own agent files
issues too. What the socket decides is not whether they exist but who
the caller is: a claim's assignee and a comment's author are the accept
environment, never a parameter.

Durability rides the user's own push: the IDE's push includes
`refs/taste/issues:refs/taste/issues` when the ref exists, and is
byte-identical to the old push until it does. Sync fetches the remote's
ref into a tracking ref and fast-forwards the local one when that is
clean; when both sides moved it says so in one line and changes nothing.
That is the compare-and-swap problem across two machines, and a merge UI
is not the alpha's answer to it. Agents never push it anywhere.

**The lifecycle the tools carry** (the loop is: the user and the
orchestrator write issues; worker agents — any ACP agent, any lab —
pick them up; the orchestrator closes them once the work is merged):

- **Claiming is compare-and-swap.** `issue_claim` sets the
  assignee-environment from the socket; the second writer's swap fails,
  it re-reads, and it is told who holds it. Push dispatch (`chat_create`
  seeded from an issue) and pull dispatch (a worker browsing
  `issue_list` and claiming) are the same tools in different directions.
- **Closing requires verified mergedness, not belief** — and the check
  is in the *tool*, not in an agent's good intentions. An issue with
  linked branches closes only when every one of them is reachable from
  the user's current branch (`ahead == 0`, the same primitive the review
  inbox renders); otherwise the call is refused, naming the branch and
  its ahead count, and nothing is written. An issue with no links closes
  freely: not every issue produces code. Links record the branch tip as
  well as its name, because the honest workflow merges from the inbox
  and then presses Delete Branch — without the tip, that issue would be
  unclosable forever.
- **The user authors in the fleet view.** The environments tab carries
  the queue as a fourth panel and a composer in the intervention panel
  (no modals). It is workspace-scoped where its neighbours are
  environment-scoped, and the heading says so.
- The queue joins `fleet::snapshot`, so the gadget card, the varlink
  socket and the console cannot disagree about how much is open. It is
  the one number there that is not a sum over the rows — an unclaimed
  issue belongs to no environment — which is why the read model went to
  **version 2** with `openIssues` rather than deriving it.
- Worker agents from other providers participate fully — issues, publish,
  per-env exec are all IDE-served MCP, agent-agnostic. Their one
  asymmetry is auth: no proxy for their providers yet, so they keep
  their own credentials and the outside-confined topology.

**What the ref substrate had to learn.** A compare-and-swap has to be
against the tip the *decision* was made on. `commit_to_ref` re-read the
ref for itself, which is right for a write whose content is fixed and
wrong for every write here: an id allocated as "one past the highest on
this tree", committed onto whatever tree arrived meanwhile, produces a
well-formed chain in which the second writer's `issues/i-0001/issue.md`
lands on top of the first writer's, with no conflict reported anywhere.
`commit_to_ref_at` takes the expected tip. Two smaller ones came with
it: `Repository::reference(force: false)` tests availability *before* it
locks, so ref writes go through a transaction now; and libgit2 caches
references per handle, so the check under the lock reads through a
handle opened for it — a stale read under a lock is a lock that does
nothing.

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
  orchestrator's socket (absent from `tools/list` elsewhere, and refused
  by every arm besides), and container creation stays subject to the same
  user-consent gates as today's `devcontainer_reload`: `chat_create`
  starts no container, so the sub-agent begins in safe mode and the
  lifecycle commands the user consents to are still the user's to start.
  What bounds the tool itself is a resource cap, not a dialog —
  `MAX_ORCHESTRATED_ENVIRONMENTS`, refused by naming the cap — because a
  prompt per creation is a prompt whose only answer is yes.

## VM substrate (direction, spike pending)

Decided direction, 2026-08-31: agent activity should sit behind KVM, not
only rootless podman — the trust model's "kernel escapes are out of
scope" line gets retired once N autonomous agents run semi-unattended.
Requirements set by the user:

- **Container builds run in the VM too, not just containers.** The build
  executes repo-supplied `RUN` steps — the earliest and least-confined
  untrusted-code path in the system — so any substrate that covers runs
  but not builds misses the sharpest edge.
- Devcontainer compatibility is non-negotiable (same devcontainer.json,
  same images); rootless is non-negotiable.

Candidates, to be decided by an empirical spike on a real host (measure:
cold `cargo build` timings, keep-id/systemd/runArgs survival, the
relocation live test, and whether `podman build` RUN isolation can ride
the runtime): **`podman machine`** (one VM; builds and runs both land
inside via the connection — the only candidate that covers builds for
free), **libkrun/`krun`** (rootless microVM per container; strongest
granularity; build coverage unproven), or a hybrid (machine for builds,
krun for runs). The architecture already absorbs this: the podman
wrapper is one seam (a connection/runtime dimension), the environment
model gains a substrate field, `AgentHosting` probes whatever the
substrate actually is, clones stay host-resident (virtiofs-shared) so
mediated publish is untouched, and per-env volumes already keep build
artifacts off the slow shared filesystem. The stdio-over-podman-exec
bridge from the socket-inversion work crosses a VM boundary
transparently — one transport for SELinux hosts and VM substrates alike.

**Safe mode joins the same substrate (decided with it).** The IDE ships
a **baseline environment definition** in-tree — git, node for agents,
inspection tools, no project toolchain — always usable because the image
travels with the IDE (OCI archive loaded on first run, never fetched).
An environment whose own config is broken, unbuilt, or absent runs the
baseline instead: same topology as container mode, different config
authority. What this changes and what it does not:

- "No exec in safe mode" was derived from absence — the only target
  would have been the host. A baseline VM is not the host; the real
  principle (no agent process on the host, ever) is untouched, and the
  repair loop gains real tools.
- The write wall stays real: the baseline mounts the env's clone
  **read-only**; writes remain IDE-mediated through `write_allowed`'s
  safe-mode scope, still the single source of truth. Reads go native —
  the one mode where the read-only bind was always the right answer.
- No nested container runtime, unchanged: builds stay IDE-supervised.
  The agent-authors / user-applies split is unchanged.
- `NoConfig` stops being a dead state: a repo with no devcontainer gets
  the baseline immediately — one environment is always usable.
- The outside-confined topology (bwrap, stand-in workspace, sibling
  agent container) is kept only as the rung of last resort for a broken
  substrate, and becomes deletable the day that rung is judged
  unnecessary. One topology, two config authorities — that is the end
  state.

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
3a. ~~**Mediated git plumbing**~~ — **shipped.** `publish_from` /
   `update_refs_from` between two local paths, libgit2 only (a `git fetch`
   would run the other repository's hooks — the host-boundary crossing this
   design refuses); `refs/taste/*` read/write without HEAD, index or
   working tree; branch enumeration by prefix with ahead/behind.
3b. ~~**Mediated publish + review inbox**~~ — **shipped.**
   `publish_branch` and `update_from_main` on agent-environment sockets
   only — the primary is the hub, and neither tool is even listed there.
   Publish is fast-forward by default: divergence comes back as a refusal
   naming the commits a force would cost and the rebase that avoids it, and
   `force: true` does not force — it asks the *user*, in a prompt naming the
   branch and the loss, and an unanswerable question is a no (the
   `devcontainer_reload` gate, applied to the second thing an agent can
   destroy). Update carries `agents/*` as well as the user's branches, which
   is what makes the orchestrator's integration workspace possible. On the
   user's side, an Inbox filter beside Dirty/Staged: published branches with
   summary, age and ahead/behind against the current branch; opening one
   lists its changed files against the merge base; bulk Merge and Delete
   Branch in the existing bottom panel. A merge that would conflict is
   computed in the object database and refused whole — nothing half-applied,
   no second conflict UI. Freshness rides the existing status refresh, so
   the `.git` watcher, fetch/sync and the publish tool's event all move the
   count.
4. **Relocation** — **shipped, and now working everywhere.** The agent
   spawns inside the env container when Running and outside-confined
   otherwise, bridged by session/load; hosting is probed per container and
   refused with a reason. The socket-direction inversion landed as this
   phase's second batch: the container's own helper binds the MCP and auth
   endpoints and multiplexes them over `podman exec` stdio, no IDE socket is
   mounted into a repo-built container any more, and the SELinux gate that
   refused relocation on every enforcing host is lifted — proven live on one.
   See "Relocation" above.
4c. **Live shells** — **shipped.** The IDE serves the ACP terminal
   extension in container mode. What the protocol models was checked rather
   than remembered: the crate's v2 draft terminals are *agent*-owned and sit
   behind a feature this workspace does not enable, while v1 — what the IDE
   negotiates — is client-served, five requests (`terminal/create`,
   `output`, `wait_for_exit`, `kill`, `release`) and one
   `ClientCapabilities::terminal` flag sent once at `initialize`. That makes
   advertisement per connection, which is per session, and that is the
   honest mechanism here rather than a limitation: a topology change is
   already a respawn, so a relocating session comes back advertising
   terminals and one dropping to safe mode comes back without them, with
   per-request refusal covering the window in between. The gate is
   *relocation's* gate, derived from it rather than re-decided from
   `AgentHosting`, because two predicates that must agree eventually do not.
   Commands compose through `ExecContext::resolve_for_agent_in` — the same
   `podman exec` route relocation and `ide_exec` take — so the agent git
   policy rides along (applied after the agent's own variables, so a request
   cannot shadow it) and one environment stays of record. No permission
   prompt per terminal: creating one is exec authority the agent already
   holds there, and a dialog whose only answer is yes is how consent gates
   stop being read — supervision is the Kill button instead. The channel was
   deliberately **not** extended: a terminal the agent asks for is the IDE
   running `podman exec` in its own right and wants nothing from that pipe,
   so `Open` stays container→IDE and `Service` stays a closed set of two.
   The shell roster (`taste_core::shells`) landed as the data half — user
   terminals, agent terminals, `ide_exec` mirrors and lifecycle streams, per
   environment, with per-shell watchers so output never rides the broadcast
   bus — and the console renders agent terminals and exec mirrors as
   read-only VTE tabs labelled `env · command`, killable, kept after exit
   until the user closes them.
   **One assumption did not survive contact.** The pinned Claude Code
   adapter (`@agentclientprotocol/claude-agent-acp` 0.69.0) never sends
   `terminal/create` — the string is not in the package. It runs Bash in its
   own process and *reports* what it ran, as
   `ToolCallContent::Terminal { terminal_id }` plus `_meta.terminal_info` /
   `terminal_output` / `terminal_exit`, gated on the client advertising
   `_meta["terminal_output"]`. That is the v2 draft's agent-owned model
   carried over `_meta` as a v1 extension; the only capability the adapter
   reads called "terminal" is `auth.terminal`, the sign-in TUI. So the IDE
   serves both directions on the one gate: correct client-served v1 for
   agents that ask, and this reporting path for the default agent, both
   landing in the same roster so the console renders them identically.
   Honest asymmetry: agent-owned rows are **not killable** (the process is
   inside the adapter, there is no child to signal and no request to ask
   with) and their output arrives once with the tool result, so the row
   appears while the command runs and fills in when it ends. The console
   says so in the disabled button's tooltip rather than offering a control
   that would do nothing. Proven live on an enforcing host against an
   ordinary confined container: terminals offered, commands run in the
   environment's own container, a long one watched and killed from the
   roster, safe mode advertising nothing and refusing with a reason.
5a. ~~**Fleet view + watching**~~ — **shipped.** The pinned console tab is
   the environments view: one row per environment carrying name (human when
   given, slug otherwise), mode and container state live off the tagged
   events, bound chat with a busy indicator, branch, published-branch count,
   an unpublished marker, disk footprint and per-environment token spend,
   with Start/Stop/Rebuild/Nuke, Open, Rename and Destroy per row and the
   selected row's build log, shell roster and podman resources beneath. The
   row model is pure data (`taste-app/src/fleet.rs`) assembled from the six
   places those facts live and tested as such — gadget mode and the varlink
   read model consume rows, not six sources. Two costs are kept off the
   render and off the main thread: the per-environment git pass and the
   footprint walk, both cached and refreshed on demand. Destroy enumerates
   what the clone holds *before* the button becomes sensitive.
   Watching landed whole: "Open Environment" — from a fleet row or a chat's
   own environment row — aims the tree and git views at that clone, says
   so on the environment strip pinned under the tree (which is also the
   one click back, and the switcher), keeps the active filter
   (the Dirty view over an agent's clone *is* the live review), locks every
   row, disables every write at the control and refuses it again at the
   entry point, and gives the clone a watcher for exactly as long as it is
   watched. Files opened from it are read-only editor tabs badged with the
   environment, and they stay that way afterwards, because the predicate is
   whose checkout the file is in rather than what the tree is showing.
   That predicate also fixed a real bug it uncovered: the editor bounded
   every write by the *window's* workspace root, so an agent's mediated
   write to a file in its own clone was refused for being outside the
   workspace. Writes are now bounded by the checkout that owns the file.
   The roster is complete — the user's own terminals register themselves
   (interactive; closing the tab is how they end) and the build/lifecycle
   stream is a roster row of its own.
5b. **Gadget mode + varlink + notifications** — *done.* The compact fleet
   card below an `AdwBreakpoint` at 520sp (`gadget.rs`), the
   `net.davidstrauss.taste.Fleet` varlink service on a per-workspace socket
   (`taste-fleetlink`, IDL checked in and served over
   `GetInterfaceDescription`), and GNotifications for the moments needing
   the user (`notify.rs`, one pure decision function). All three consume
   `fleet::FleetRow` through one projection, `fleet::snapshot` — the card
   renders the same `Snapshot` struct the socket publishes, so no surface
   grew an inventory of its own. The service is read-only: a control
   interface, if ever wanted, gets its own name and its own argument about
   authority.
6. ~~**Orchestrator**~~ — **shipped.** Orchestration tools on the
   designated chat's environment socket and on no other (the
   `publish_branch` precedent, for a stronger reason: these spawn
   agents), with every arm re-checking the role rather than trusting that
   the tool was listed. The designation is a switch in the chat's own
   settings that clones an environment in the same gesture when the chat
   has none — an unbound orchestrator would share the primary's socket
   with every other unbound chat. `chat_create` runs cap → issue
   pre-flight → create → claim → prompt, so the cheap refusals cost no
   clone and a lost claim leaves an idle chat rather than a misdirected
   one; per-level model config rides the session's own advertised
   options. The strip answers over `taste_core::orchestration`, shaped
   like the UI probe: plain data out, never a pane, and no request
   variant for answering a sub-chat's permission prompt. Proven live
   against a real Claude Code session (`taste-acp/tests/orchestrator.rs`):
   tools present on the hub's socket and absent from the primary's, the
   model calling `chat_create` off the descriptions alone, and the task
   landing in a second agent's real ACP session.
7. ~~**Issues**~~ — **shipped.** `refs/taste/issues` with one directory per
   issue, comments as sibling files, and ids allocated inside the
   compare-and-swap; five tools on every socket with the caller's identity
   taken from it; the close gate enforced in `issue_update` against the
   same mergedness primitive the review inbox renders; the queue and its
   composer in the environments tab; `openIssues` through `fleet::snapshot`
   to the card and the socket (read model v2); and the ride-along on the
   user's push and sync. The ref substrate gained `commit_to_ref_at` on the
   way — see "Issues: a ref, not a service" for why a swap against the
   ref's *current* tip is not a swap at all.

Each phase lands green (`cargo test --workspace` in the devcontainer),
updates ARCHITECTURE.md for what it changed, and is independently
useful.
