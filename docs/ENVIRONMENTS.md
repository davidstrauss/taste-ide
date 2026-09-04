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
   supervision share one surface; sub-chats are ordinary chats the user
   can also drive by hand, at their own model settings — each in its own
   environment, reached by selecting it.
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
(container or safe, evaluated per environment), and **at most one chat**.

That last one is an invariant, not a tendency (locked 2026-09-01): a chat
*is* an environment's conversation. Two chats in one environment has no
answer to "which one does the pane show", and the design never wanted one
— `chat_create` has made the pair from the start. So the chat tab strip
is gone, `ChatEntry::environment` is required, and the state is keyed by
it (`WorkspaceState::set_chat`; state v5, v4 discarded rather than
merged). Wanting a second conversation means wanting a second world,
which is what New Environment is for.

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

Each environment is in exactly one of the two modes, and since the baseline
shipped (below) the mode is derived from **whose config** its running
container was built from rather than from whether one is running at all.
Writes stay confined to the safe-mode scope **of that environment's clone**
in safe mode; what changed is that safe mode now has somewhere to run:

- A chat whose environment is down, broken, or not yet built can author or
  repair that environment's devcontainer config, with the IDE's baseline
  container up so it can actually *run* things while doing so. This is the
  bootstrap path for every new agent environment: clone, baseline up,
  config authored/validated, user-consented start, relocate.
- The configuration-authority split is per environment and unchanged:
  the agent authors, the user applies; `devcontainer_reload` names what
  will run and denies when it cannot ask. The baseline does not soften it —
  the baseline declares no lifecycle hooks at all, so there is nothing to
  consent to in the fallback itself.
- The primary environment's safe mode is exactly every other
  environment's.

**The confined-outside spawn path is therefore permanent infrastructure,
not legacy.** Every chat's agent must be spawnable in either topology —
outside-confined (nothing running) or inside the env's container (up) —
and the transition between them is a respawn bridged by the persisted
session id and `session/load`, the same continuity mechanism reloads
already rely on. The chat never restarts; the process does. What the
baseline changes is how *often* that rung is reached: it is now the answer
to "podman is gone", not to "this repo has no devcontainer".

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

**Design commitment, locked 2026-09-01: the environment panel is the
app's single top-level control, and every other pane shows the selected
environment's resources.** The file tree, the git views, the editor's tab
set, the console and the chat all render one world — the one the panel
says you are in — and selecting there IS the context switch. It is the
only one: no pane has a switcher of its own, because a second one is
something the first can disagree with.

This supersedes two earlier descriptions in this document. Editor tabs
from a watched environment are no longer *mixed* alongside the user's;
each environment owns its tab set, stowed and restored whole. And the
console is no longer a list of every environment with a selection of its
own; it is the selected environment's detail, shown as a flat strip of
tabs and named nowhere — the environment panel in the flank is the one
place the selected environment is named. What did
not change is the predicate: whose checkout a file is in still decides
whether it is read-only and which set it belongs to
(`policy::in_environment_checkout`), never what is on screen.

Three consequences worth stating because they are what make it usable:

- **One selection, stored once.** `window.rs`'s `aim_panes` owns it. Every
  surface that can ask — a panel row, a console action, a notification
  click, a gadget row, the editor being told to open a foreign file —
  asks it, and it tells each pane. An environment it cannot resolve is
  refused rather than replaced by the primary: there is no fallback
  environment anywhere in this design.
- **Switching loses nothing and costs nothing.** Chat panes are stack
  pages that are never destroyed, so a hidden conversation goes on
  streaming; editor pages transfer between tab views, so buffers, undo
  and unsaved edits survive by never being taken apart (only the scroll
  offset is written down and put back). No filesystem or git work runs on
  the main thread during a switch.
- **A chat you cannot see can still ask for you.** A waiting permission
  request marks that environment's row in the panel, which is on screen
  whether or not its chat is. With one chat per environment and only the
  selected one rendered, that row is the only place in the window the
  question can appear. Desktop notifications are the same fact, outside
  the window.

The user can open any environment and watch its agent work — **read,
never edit**. The fixed pane layout does not change; what the panes are
aimed at does, by explicit action only:

- **Where the panes are aimed is said once, by a permanent panel at the
  very bottom of the file-tree pane** — below the intervention panel and
  below anything else that pane opens, because a context indicator that
  can be displaced by a transient panel is not an indicator. **It lists
  every environment, always, one row each**, primary first as the way back
  and named "Yours"; clicking a row aims the panes there. No menu, no
  reveal: the switcher was a popover, which meant the fleet existed only
  while it was open, and between openings the panel could not say that
  another environment was building, or waiting on you, or had gone down.
  The panel tints itself whenever the context is not home, and the aimed
  row is bold and carries the read-only lock.
  Every row carries a **traffic light** — green (up), amber (building,
  starting, drifted config, safe mode on the baseline, or a chat stopped on
  a question only the user can answer), red (nothing runs here) — and an
  **activity sparkline**, the last five minutes of that environment's
  event, output and turn traffic (`taste_core::activity`). A state cannot
  tell an environment that is up and hammering from one that is up and
  idle; that is what the sparkline is for. Silence draws nothing rather
  than a flat line, which would claim a measurement where there is only an
  absence. A chat waiting on an answer gets a **mark of its own** beside
  them, because amber is a steady state a fleet can sit in — baseline mode
  alone would keep half the lights amber — and a question nobody has
  answered must not drown in it.
  And under the name, when the environment holds a claim, **what it is
  working on**: the claimed issue's title, dim, one line. Two signals per
  row and no more — this is a sentence, not a signal, and is read rather
  than glanced at. It is the panel's half of the env↔issue link and the
  half worth the pixels: "what is `calm-1` doing" is the question you look
  at the fleet to answer. It is a second LINE rather than a suffix after
  the name, which was tried and photographed: in a 180px flank the suffix
  ellipsizes an issue title to three words and a box, so the caption was
  there and said nothing. Rows that carry one are taller, and the six-row
  ceiling counts *rows* rather than pixels so a fleet that is busy does not
  quietly cost two rows of visible fleet.
  Past six environments the panel filters and scrolls inside itself instead
  of growing into the tree. Ctrl+Shift+E focuses it and walks the rows;
  Enter switches. Its header holds **New Environment**: the way to make a
  world lives where the moving between them does. It replaced the
  "Viewing `<env>` / Back to Yours" bar the tree header used to grow, and
  then the popover switcher that replaced that.
- The panel is the only switcher. A notification click and a gadget row
  still arrive somewhere, and both do it by asking for the same
  transition rather than moving a pane of their own. Nothing auto-follows:
  watching is deliberate, and the tree never jumps out from under the user.
- **Non-primary environments are read-only to the user.** Tree rows
  carry locks (the safe-mode affordance, reused for a second purpose),
  file operations and stage/discard/commit/push are disabled, and the
  editor refuses saves to foreign-env files. The user's intervention
  path is reviewing published branches or taking over the chat — never
  editing under a running agent, which would race it.
- **Each environment owns its editor tab set.** Files opened from a
  watched environment are read-only tabs badged with the environment
  name, and they live in *that* environment's set: switching away stows
  them, switching back restores them in order, with their selection and
  scroll. (They used to be mixed in beside the user's own tabs. That was
  the last place two environments shared a pane.) Opening a file that
  belongs to another environment moves the one selection rather than
  stranding a tab nobody can see — a tab the user cannot see is not an
  open file. **The predicate is whose
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
- **Live shells are first-class.** Wherever the agent relocates — the
  project's own devcontainer or the baseline alike — the IDE serves the
  ACP terminal extension — a change of position, deliberate: the "no
  third route to a process" refusal was written for the outside-confined
  topology and still holds there (the rung below both modes has no exec
  target at all, so relocation itself is refused and the extension goes
  unserved). Post-relocation the agent
  already runs beside the files, so client-served terminals add
  *visibility*, not authority. Agent-created terminals execute in that
  chat's environment container through its `ExecContext` (agent git
  policy attached) and surface as live **read-only** console tabs
  labeled `env · command`, each with a user-side Kill action — stopping
  a runaway process is supervision, not editing.
- **Every shell IS a console tab; there is no separate roster listing.**
  User terminals attached to the environment (interactive — they carry
  no Kill button; closing the tab is how they end), agent terminals and
  `ide_exec` jobs (read-only, Kill in the tab's own header) all live
  side by side with the environment's other tabs, following the
  selection the same way they always did: closing one loses nothing —
  the shell keeps running (or its output keeps sitting there) and
  reopening the environment brings its tab back. Ownership reads off the
  tab itself — an indicator badge marks a tab that is not the user's own
  (agent-owned, read-only) — and a tab whose process has exited is
  marked exited and keeps its output on screen until the user closes it
  by hand; nothing auto-closes it any more. `taste_core::ShellRoster`
  is still the model every one of these tabs watches (and what fleet
  counts and the varlink read model draw on) — only the console's own
  *listing* of it went away, folded into the tabs it used to enumerate. A
  new terminal opens in the *selected* environment when that environment
  has a container, and in the workspace's own context otherwise: a clone
  with no container resolves to the host, and a shell there would claim
  an environment while showing the user's files. Honest limit, stated
  plainly: a process the agent spawns without a terminal is not
  observable — visibility is by convention (the adapter prefers client
  terminals when offered), not by ptrace. After relocation that
  convention covers nearly everything the agent runs.
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
  the `publish` MCP tool. The IDE fetches that branch from the env clone
  into the main checkout at the environment's **branch of record**.
  Explicit handoff, no shared mounts, nothing polled.
- **Refresh** (user → agent): an `update_from_main` tool (and fleet-view
  action) fetches the main checkout's branches into the env clone's
  remote-tracking refs; the agent rebases inside its own world.
- Inside a container, the clone's `origin` points at a host path that is
  not mounted; fetch/push from inside simply fail. The existing
  `agent_git_config` push-blocks stay as defense-in-depth.

### Strictly one branch per environment

**An environment has exactly one branch, `agents/<env>`, and nothing
chooses its name.** It is derived from the environment id
(`taste_git::env_branch`), created by the first publish, and moved by
every publish after that. `publish` takes no topic, because there is no
topic to take.

The reason is that **the environment is the unit of review.** An
environment is already one clone, one container, one agent session and one
merge target; letting it publish N topic branches means the thing the user
reviews is not the thing they stop, and the thing they merge is not the
thing they destroy. With one branch those are the same object, which is
what makes the whole review lifecycle below expressible at all: "this
environment is done" is a statement about a branch, a container and a
conversation at once.

Consequences worth stating:

- Publishing twice moves one ref. There is no accumulation to garbage
  collect, and no per-environment list for a view to render.
- `update_from_main` still carries `agents/*` down into every clone, so
  the orchestrator integrating N environments' work is unchanged — it
  merges N branches and publishes the result as *its own* branch of
  record.
- The mediation itself is untouched: host-side libgit2, no hooks, no
  working tree moved on either side, fast-forward by default with force
  gated on the user.

**`agents/<env>/<topic>` is a dead generation.** Alpha rules: nothing
migrates it. A publish blocked by a leftover topic branch — git cannot
hold both a ref and a directory of the same name — says exactly that and
names the branches to delete, and `review_list` reports whatever is left
under `dead_generation_branches`, attributed to nobody.

**Push to GitHub stays user-only and host-side**, exactly as today. The
issues ref (below) rides along on that push; agent branches do not,
unless the user merges them first — publishing to the world remains a
deliberate human act.

`taste-git` grows the plumbing this needs (all parameterized, no
singleton state): remote management, fetch-from-local-path with explicit
refspecs, arbitrary-ref read/write (`refs/taste/*`), commit-to-ref
without touching HEAD, branch enumeration by prefix, and a push that can
carry an extra refspec.

## The review lifecycle: environments, not an inbox

**This replaces the review inbox.** The inbox was a list of published
branches; review is now a state each environment is in, and the list is
the fleet you already have. The arc:

```text
Working ──ready──▶ FlaggedForReview ──▶ Merged ──┐
   ▲                     │                       ├──▶ destroyable
   └───── back to work ──┘             Rejected ──┘
```

**Flagging is a sentence the agent says, not a side effect of
publishing.** `publish` is a checkpoint: it moves the branch and changes
nothing else. `publish { ready: true }` is the submission — it flags the
environment and **stops its container**. The two are separate because an
agent checkpoints far more often than it finishes, and a publish that
always flagged would stop environments mid-thought.

**Flagging stops the container, to save the machine.** A flagged
environment is waiting on a person and running nothing; so is a merged or
rejected one. The stop is the ordinary `Supervisor::stop`, not a second
kind of stopped-ness, and revival is the ordinary start — the row's Start
action, `devcontainer_reload`, or **sending a message to its chat**.
Nothing restarts an environment on the IDE's own initiative: a review
state is never a reason to spend the user's machine.

That third way in is a gesture, not a mechanism. A flagged environment's
conversation is still there to read, and typing into it used to go
nowhere useful — the agent spawned into the outside-confined fallback
against an environment with no exec target, and the container stayed
down. Now the composer carries a line saying what a send will do
("calm-1 is stopped — sending will start it"), and the send calls the
same `Supervisor::reload` the other two do. The message is not dropped
and not raced: it goes into the transcript at once, wearing the
composer's existing queued badge, and is handed over when the
environment has an exec target — so it reaches an agent living beside
the files rather than the topology the container is about to replace.
`chat::revive_wanted` is the gate, and `ChatPane::send` is its only
caller passing `user_initiated: true`, so "who started this container"
stays answerable. (The stop is deferred by a beat, because the agent that
asked for it lives in the container being stopped and its answer has to
get out first.)

**Merged and Rejected mean destroyable with nothing to warn about.** The
destroy warning exists for work nobody else has a copy of; once the user
has looked at an environment's branch and ruled on it, its leftovers are
what they already decided against. Suppressing the warning there is what
keeps the warning meaningful everywhere else. This is how a fleet drains
instead of accumulating.

**Merged is a record, never a latch.** Whether the work is actually *in*
the target is `taste_git::Mergedness`, asked fresh: the environment's
branch tip reachable from the merge target, `ahead == 0`. A force-moved
target un-merges the work and the fact says so. That is one function with
two callers — the review state and the issue close gate — because two
implementations of `ahead == 0` means one of them is eventually wrong.

**The flag is persisted with the environment** (`EnvironmentEntry.review`,
state v6 — old files are discarded with a notice, per alpha rules) and
read through `taste_core::ReviewBoard`, a handle on the workspace. An IDE
that forgot which environments were waiting would restart every container
it had stopped to save the user resources.

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
- Gemini/Copilot: the proxy is per-provider machinery, and until theirs
  exists those agents carry their own credentials — in the agent home
  volume (`~/.gemini`, `~/.copilot`), which is on the agent's side of the
  boundary in both topologies. They **relocate like Claude Code does**:
  the gate asks whether the environment has somewhere to be, not which
  agent is asking, and every image that can host an agent carries node,
  so each is launched as its pinned npm package (`npx -y <pkg>@<version>`)
  rather than as a bare command the container was never going to have.
  What they lack is the proxy's half — spend accounting and a placeholder
  in place of a credential — and that is the difference to say out loud.

### Subscription usage

The credential the IDE holds is billed to a subscription, and a
subscription is **one pool**: every environment in the fleet and the
user's own interactive Claude use draw on the same rolling windows. Being
the last hop of every Anthropic request the fleet makes, the proxy is the
one place that can see the state of that pool — so it does, **passively**.

- **Harvested, never asked for.** Each response the proxy is already
  carrying is read for its rate-limit headers on the way past: the
  documented `anthropic-ratelimit-*` family
  ([response headers](https://platform.claude.com/docs/en/api/rate-limits#response-headers)),
  and — recognised by shape rather than by documentation, because none
  describes them — any family naming itself a unified or plan window.
  Claude Code's own `/usage` asks an endpoint; that endpoint is
  undocumented, so it is not ours to call, and no request is ever made to
  refresh a gauge — spending the user's quota to describe their quota
  would be an absurd way to report it.
- **What a subscription actually sends** (observed live through this
  proxy on a `claude setup-token` credential, 2026-09-01):

  ```text
  anthropic-ratelimit-unified-status:                allowed
  anthropic-ratelimit-unified-utilization:           0.03
  anthropic-ratelimit-unified-reset:                 <epoch seconds>
  anthropic-ratelimit-unified-7d-utilization:        0.03
  anthropic-ratelimit-unified-7d-reset:              <epoch seconds>
  anthropic-ratelimit-unified-representative-claim:  five_hour
  anthropic-ratelimit-unified-fallback-percentage:   0.5
  ```

  Two things follow. **None of the documented per-minute headers came
  back at all** — that family is API-key traffic, so on a subscription
  the plan windows are the whole of what there is, and the code renders
  the per-minute rows only if they ever appear. And the unnamed `unified`
  family is *one* window rather than the union of them: which one is what
  `representative-claim` says, so it is read rather than assumed —
  otherwise a five-hour number would silently wear the weekly label.
  `fallback-percentage` is kept verbatim and shown nowhere, because a
  name is not a meaning. Every unrecognised `anthropic-ratelimit-*`
  header is kept the same way, so the next person can see what the
  account is sending now rather than what it sent when this was written.
- **A 429 is the authoritative signal.** Utilization headers describe
  headroom; a refusal is the account declining to serve, and it carries
  `retry-after` and a message naming the window. It is recorded whatever
  the headers said, and lifted by the next response that is *served* —
  proof the window reopened, again without asking.
- **As of last turn, by nature.** There is no reading without traffic, so
  every snapshot carries the moment it was taken and every surface says
  so: the environments panel's gauge fades once a reading is an hour old,
  and the chat's Utilization tab puts "as of 4 min ago" in the section
  heading rather than in a footnote. Before any turn has run, the tab
  says nothing has been observed — which is not the same as nothing
  having been spent, and the difference is the point.
- **Per-environment spend is the breakdown, not the total.** The proxy's
  own counters say who drew on the pool *through this IDE*; the account's
  windows include whatever the user did in Claude elsewhere, which
  nothing here can see. Both appear in the Utilization tab, labelled as
  what they are.
- The snapshot is workspace-global and rides the fleet's existing 1 Hz
  assembly (`PoolFacts`, beside the fleet rows): the console reads the
  proxy, and the panel and the chats render what it hands them.

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
- `publish`, `update_from_main` → that environment's clone.
- `fs/read_*`/`fs/write_*` (ACP side) and `write_allowed` evaluate
  against that environment's clone root and mode.
- Orchestration tools (below) are served **only** on the orchestrator
  chat's socket; other connections don't see them. (Shipped, phase 6.
  The role is one `Option<EnvironmentId>` on the server, written by the
  chat pane; the primary is refused as a holder, on both sides.)

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

**The fleet is enumerated once, and detailed once** (shipped, phase 5a;
scoped to one environment 2026-09-01; sections promoted to flat tabs, then
the console's own pane header deleted, 2026-09-02). The file tree's
environment panel is the list — every environment, always, with a traffic
light and an activity sparkline each — and it is the app's **single namer**
of the selected environment: nothing below it repeats the name. The console
is the *detail* for the one the panes are aimed at, and it is **one flat
strip of tabs and nothing above them**.

There are no nested tab sets anywhere, and no pane header either:
`[environment] [resources] [services] [terminal…]`. The build log, the
shell roster and podman's resources were once an `AdwViewStack` behind an
inline switcher inside a single "Environment" tab, which put a row of
tab-shaped controls under a row of tabs and made "which strip am I in" a
question the eye had to answer twice. Promoting them to real tabs left the
facts that describe the environment in a header above the strip; that
header is gone too, because the panel already names the environment and a
header above a strip is a thing that has to be carried by hand at the
consolidated rung. Its facts are the environment tab's own content now.

The three fixture tabs are **pinned, and therefore icon-only**: pinning is
how `AdwTabBar` draws a page as its icon alone, no title and no close
button, held at the strip's left edge — which is what these three are. A
tab is a glance; the badges and the tooltip carry what a glance cannot.
(The pin travels: at the consolidated rung they are grafted into the
editor's one strip and are the same three icon-only, unclosable pages
there, as is the chat's grafted trio. It comes off only for the crossing
itself. See the responsive ladder.)

- **The environment tab** (what the flat-tab round called "Log") opens
  with **two lines, grouped by kind**. The round before this put the
  state, two git counts, a disk size, two token counts, an agent's name
  and three buttons on one baseline, which read as a wall of unrelated
  facts sharing a line; what it is now is:
  - **The machine.** A traffic-light dot — the environment panel's own,
    same diameter, same vocabulary — and the state in words: mode named
    only when it departs from the normal case, because every environment
    that is up is a container and "container mode" distinguished nothing.
    The baseline says "safe mode" (something IS running there), the rung
    below both says "no environment", and the project's own config in
    force says nothing at all and lets the state lead. This line is the
    only thing in the header at full contrast, so there is somewhere for
    the eye to land. At the right edge: refresh and the environment's
    `⋮` menu (Start/Stop/Rebuild/Nuke, Rename, Destroy) — and, while the
    config has drifted from the container running it, an inline
    **Rebuild** button. Drift is a persistent condition and a persistent
    condition earns a persistent affordance, which is exactly the review
    banner's shape: the words carry the condition, the button offers the
    fix. It runs the same reload the `⋮` menu's Rebuild does, and it is
    the user applying a configuration — the half of the authority split
    that is theirs, and the reason the agent's own path
    (`devcontainer_reload`) has to ask first and this does not.
  - **The work**, dim, indented past the dot so both lines open on the
    same left edge: the bound chat with its busy spinner and role glyph,
    what the environment is working on, and the publish ledger
    (unpublished and published counts) holding the right edge as a
    column under the actions. The agent leads this line rather than
    trailing it — trailing, it was the only thing on the line for any
    environment with nothing claimed and nothing published, which
    includes the user's own checkout, and a lone dim chip against a
    right edge reads as something left over.

  Then the build/lifecycle log with its Tail switch in a toolbar directly
  above it, and the intervention panel (rename, destroy) at the bottom —
  never a modal.

  Two numbers left that line for the surfaces they are about: the **disk
  footprint** is on the Resources tab's tooltip, which is the tab that
  enumerates the containers, volumes and images it is the sum of, and the
  **token spend** is on the state line's tooltip beside the mode it
  explains, since the chat pane's Utilization face is the surface about
  what things cost. Neither earned a permanent slot on a row the eye has
  to scan.
  The header does NOT carry the branch or the dirty count: those are
  working-tree facts, and the file tree is where working-tree facts live.
  They are in the tab's tooltip, with the environment's name, because a
  tooltip is asked for.
  When the environment is flagged for review, an `AdwBanner` leads the
  tab's content — a persistent condition wants a persistent widget, not a
  card that could be scrolled past — with Open Review as the banner's own
  button and Merge/Reject/Destroy just beneath it, since a banner holds
  only one action.
  Its badges: the icon is the container's state, the indicator is a
  configuration that drifted under a running container, and
  `needs-attention` is "you have to answer something" — failed, flagged
  for review, or a conversation stopped on a question.
- **The resources tab** is the selected environment's podman objects
  (container, image, volumes), on its own rather than one page of a
  switcher.
- **The Services tab** is unchanged: systemd units and their journals.
- **Terminal tabs** keep short titles (`env · command`) — a terminal's
  identity IS its command, and four icon-only terminal tabs would be four
  indistinguishable tabs — but pick up the same badge convention for two
  facts of their own: an indicator marks a tab that is not the user's own
  (agent-owned, read-only), and a tab whose process has exited is marked
  exited and keeps its output on screen until the user closes it, rather
  than closing itself. The second overwrites the first, which is why it
  takes a different frame to photograph each.
- **Refresh and the environment's action menu** are in the environment
  tab's own content. They are actions on the selected environment, which
  is what that tab is, and a page's content crosses with the page.
- **New Terminal is at the tab strip's end**, right-anchored, because the
  strip it adds a tab to is the thing it acts on. It spent a round buried
  in the tab's content, and the reason is worth keeping: at the
  consolidated rung this pane's *pages* move into the editor's strip and
  this tab bar stays behind with the pane, so a button parented to the
  bar and then forgotten leaves the window at 960sp. **Bar furniture does
  not graft.** The fix is not to hide the button in a page — it is for
  the rung change to install it on whichever bar is hosting the family,
  which is what `set_rung` does in both directions, composing it beside
  the editor's display-mode menu rather than replacing it. The button is
  at the far right end of the bar at both rungs, and since the editor's
  bar says nothing about which environment is selected, its tooltip names
  the one a terminal would open in.
- The strip carries an `AdwTabOverview` button with the tab count on it,
  because an environment with two sections, Services and two terminals
  already scrolls a 700px pane.

The issue queue renders in the flank's backlog panel — it is the
workspace's, not an environment's, and its heading says so.

The tab listed every environment until the panel became permanent, and
then the listing was deleted rather than left in parallel: two renderings
of the same rows competing for the same glance is one to keep in agreement
with the other, and the stale one is always whichever the user is not
looking at. The tab chooses nothing now — the panel aims the panes, and
the tab follows. Rows are assembled as **pure data** from the six places
an environment's facts live, so the panel, gadget mode and the varlink
read model render rows rather than re-deriving them.

### The responsive ladder

**One window, three widths, and nothing is ever rearranged.** The layout
commitment is that the panes keep their places; what changes with width is
how many of them are *columns*. Two `AdwBreakpoint`s, and at each rung the
thing that gives way is **moved, never rebuilt** — the same widget,
reparented, exactly as the editor stows a tab set when the selection moves.

| Width | Flank | Chat | Console | Editor |
|---|---|---|---|---|
| full | column | column | pane under the editor | column |
| ≤ 960sp *or the full layout's minimum* — *consolidated* | column | tabs in the one strip | tabs in the one strip | **is** the strip |
| ≤ 520sp *or the consolidated layout's minimum* — *gadget* | **is** the window | — | — | — |

- **Consolidated** is a window tiled beside a browser: four panes are still
  wanted and no longer fit as four *columns*. **Consolidation is of tab
  sets, and it goes all the way**: the chat column and the console pane
  stop being panes, and their views become tabs at the end of the editor's
  strip, so the window has exactly ONE tab strip in it —

      [file 1] … [chat] [usage] [agent] [environment] [resources]
      [services] [terminal 1] [terminal 2]

  — which is the same principle the console follows at every width: **no
  nested tab sets**, every leaf view a first-class tab in its region's one
  strip, and down here there is one region. The chat's own three-toggle
  strip hides and its three views become three tabs; the console's tabs are
  *transferred pages*, so a terminal's pty crosses the breakpoint without
  noticing. Whichever tab the user is reading takes the whole width.

  Everything is reparented, never rebuilt: the chat is the same widget the
  column was, so switching environments keeps working, and the utilization
  and settings shades are lifted out of its overlay rather than built a
  second time — a second set would be a second answer to which agent this
  conversation uses. A conversation stopped on the user lights its tab the
  way a tab strip says it: `needs-attention`. The utilization tab keeps
  its badge, which is the same badge its toggle wears at full width: the
  glyph is never tinted, and how full the conversation is rides as a
  **traffic dot in the glyph's corner** — amber filling up, red nearly
  full, nothing at all while there is room — at the size, corner and
  hairline the container and services glyphs already badge with. One icon
  name carries it, so the toggle and the tab cannot disagree and neither
  needs CSS a tab page does not have; the colour comes from the palette,
  because GTK recolours a symbolic icon's `warning`/`error` classes. It
  cannot be the page's *indicator* — the obvious slot — because
  `AdwTabBar` gives a pinned tab one 16px slot and draws the indicator
  *instead of* the icon in it.

  Grafted tabs are **guests**: they arrive as a family, stay together, and
  refuse to close — they are panes, and a pane you can accidentally close
  is a pane the user has to know how to get back.

  **A pane's tab is its icon and nothing else**, and it is `pinned` to be
  so. That is the one rendering `AdwTabBar` has for "icon alone, no title,
  no close button", and both halves are wanted: a pane is known by its
  glyph everywhere else in this window — the chat's own three toggles at
  full width, the console's three fixtures in its own strip — so a labelled
  `[💬 Chat ×]` was the one place these views wore a label, and the × was a
  button that could only ever be refused. A shorter title is not an
  alternative: `AdwTabBox` allocates every *unpinned* tab the same width
  (measured: nine tabs, 126px each, from "Chat" to "primary · cargo test
  --workspace"), so an empty title buys a tab with nothing in the middle.

  The price is the position — `AdwTabView` keeps pinned pages in a section
  of their own at the leading edge — and it is worth paying. The rule it
  replaces ("guests trail the user's files") was written to stop a strip
  that interleaved documents with panes, and the pinned section
  interleaves with nothing: it is a separate, non-scrolling box, so the
  files stay together in theirs, in their own order. What trailing actually
  produced at 900px was six labelled guests scrolled off the end of the
  strip, which made the chat — the pane this rung exists to keep — the
  hardest thing in the window to reach. **Terminals are not pinned**: a
  terminal is closable, closing its tab is how the user ends the shell, and
  its identity is the command it is running, so it keeps a short title.
  What is left for `tabfamily` to arrange is the run the user can drag:
  documents first, terminals after.

  The console's fixtures cross unpinned and are pinned again on arrival
  (`Console::begin_migration` / `Console::set_host`), so a transfer never
  has to have an opinion about which section a page is in. Nothing rides
  along beside the pages: the console has no header to bring, and every
  fact that would have been in one is inside the environment tab, which
  crosses as its page's content. The section the user was reading survives
  the trip, and so does everything else — an unsent prompt, a live
  transcript, a terminal's scrollback.

  The pinned section is six icons wide and always on screen; the documents
  and terminals scroll beside it, and `AdwTabOverview` — thumbnails with a
  search box, opened from a button carrying the tab count — is how a tab
  you cannot see is found.

  The flank stays put: it keeps its column, with the Environments panel and
  the Backlog in it. An earlier version of this rung also collapsed it;
  that made the window a stack of full-width bands, and took away the one
  pane that says which environment you are in.
- **Gadget mode** is not editing at all. The panes give way to the two
  panels that were already answering the supervision question: the
  Environments panel and the Backlog under it, moved into the window. The
  subscription gauge comes with them, being a child of the panel's own
  header. This used to be a bespoke card rendering the fleet snapshot — its
  own list, its own glyphs, its own spend bars — which was a second widget
  tree drawing the same facts as the panel, and the one that went stale was
  always whichever nobody was looking at.

The two breakpoints are **ordered widest-first**, and that is load-bearing:
libadwaita applies the *last* breakpoint whose condition matches, and at
400sp both of these match. Added the other way round the middle rung
shadows gadget mode entirely, and a window dragged into a corner keeps its
panes and merely squeezes them.

520sp is chosen to sit below every width GNOME's own tiling produces, so
gadget mode is entered by dragging a corner and never by snapping the IDE
beside a browser; 960sp is deliberately *above* them, for the opposite
reason — being tiled beside a browser is exactly when consolidating helps.

**Both numbers are floors, not thresholds, and the difference was a bug.**
A rung that is still in force at a width it does not fit in does not
degrade: its panes are allocated below their minimums and the last one in
the row is cut off the edge of the window. And whether a rung fits is not a
constant — it is the sum of the panes' own minimums, and the flank's
minimum carries a branch name and a git status line. Measured: the full
layout needs 973px against a real checkout and 863px against the
screenshots' fixture, while the breakpoint handed over at 960sp — so
between 961 and 973 the chat column ran off the right edge, which is
exactly what it was reported doing.

So each rung hands over at the **larger** of its constant and the measured
minimum of the rung above it, recomputed as the window resizes; the
constants can only ever be raised by the arithmetic. The window's own
minimum cannot be asked to do this job: a window with breakpoints reports
the minimum of its *narrowest* configuration (360px here, the gadget
card's), because otherwise it could never be dragged small enough to reach
the rung that needs less room. `TASTE_PROBE_WALK=1500-380` walks the
ladder and fails on any width where a pane leaves the frame.

The consequence to know about: against a real checkout the consolidated
rung's own minimum is 660px (flank 335 + handle + strip 320), so a window
tiled to half of a 1280 display lands in gadget mode rather than in a
middle rung that does not fit. Keeping 520sp real means keeping the
**flank's** floor down — `TASTE_MEASURE_MIN=1` attributes it — not
restating the constant.
A floating always-on-top gadget is deliberately not attempted: Wayland does
not grant apps keep-above, and panes never float.

The companion is **GNotifications for moments needing the user** — a
waiting permission prompt, a turn ended, a failed env build, an environment
flagging itself for review. Glancing is ambient; action gets a
notification. The rule, in one line: never notify about the surface the
user is already looking at — window focused AND that surface on screen —
with ids scoped per chat and per environment, so two chats needing the user
are two notifications and one chat asking twice is one. A flag is
persisted, so the digest baselines on its first read: a restarted IDE does
not announce a fleet that was already waiting.

**Shell integration rides a varlink interface — varlink, not D-Bus, by
decision.** Phase 5 exports the fleet as a varlink service on a unix
socket (named by `taste_core::environment`, IDL checked in-tree):
environment states, busy chats, the quota gauge, how many environments are
flagged for review, and what each has claimed off the backlog — the same
rows every surface in the IDE renders. It costs little, is testable like every
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
  environment and its chat, seeds the first prompt, returns
  `{ chat, env }`. The pair is the point: one chat per environment, so
  creating a conversation *is* creating a world. It is created in the
  background; the user reaches it by selecting that environment, and can
  take it over at any time.
- `chat_send { chat, text }` / `chat_status { chat }` /
  `chat_transcript_tail { chat, max? }` — drive and observe sub-chats.
- `review_list { flagged_only? }` — where every environment stands for
  review: its branch of record, its mergedness against the user's
  branch, and its review state. Read from the hub.

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
somebody else's issue. Creation-time linking is a *claim*, and that is now enough on its own:
the claim names the environment, and the close gate follows it to that
environment's branch of record. `issue_link` survives for the case a
claim cannot express — work that landed from an environment other than
the one holding the issue, which is what integration produces.

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
set per sub-chat) — plus the Fable row the proxy puts back. Claude Code
lists a subscription's Fable model only when it holds the account's login
itself, and behind the proxy it holds a placeholder, so the IDE adds that
one entry through Claude Code's documented custom-picker variables
(`taste_acp::authproxy::spawn_env`); whether the account can use it is the
API's to say at the first turn.

Sub-chat permission prompts still surface in their own tabs to the user;
the orchestrator cannot approve on the user's behalf, and there is **no
tool that would let it** — `chat_status` reporting `awaiting-permission`
is how it learns to tell the user instead.

**The orchestrator's environment is the integration workspace.** The
orchestrator is a chat, and chats get environments — its own clone and
container are where sub-agents' work is merged, conflicts resolved, and
the combined result tested, so the user reviews one integrated branch
instead of N raw ones. The flow is the star, always through the hub:

1. Sub-agents publish as usual — `publish` moves each one's
   `agents/<env>` branch of record in the main checkout.
2. The orchestrator's environment pulls those refs down via the same
   `update_from_main` mediation, which therefore carries `agents/*`
   refs and not just the user's branches (a Phase 3 requirement, not an
   orchestrator afterthought).
3. Integration is ordinary agent work inside its own clone: merge,
   resolve with native tools, run the tests in its own devcontainer —
   observable through the same watching and live-shell machinery as any
   environment.
4. The result publishes the only way anything publishes: onto the
   orchestrator environment's own `agents/<orchestrator-env>`, with the
   raw per-agent branches still inspectable beside it.

Phase 6 added no git machinery for this, which was the Phase 3
requirement paying off: `update_from_main` already carries `agents/*`,
and `publish` already works from any environment's clone. The
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

One more file sits beside them on the same ref: **`order`**, one issue id
per line, top of the queue first. The queue is a **backlog**, and its
order is the user's to author.

One file rather than a `position:` per issue, and that is what makes it
work: ordering is a statement about the *list* — moving one issue up moves
another down — so a per-issue field would need N writes to say one thing,
and two landing out of order would leave two issues claiming one place.
One file is one compare-and-swap, and the loser of a race re-reads the
winner's list and re-applies its move to it. The file is advisory in one
direction only: ids in it that no longer exist are skipped, and issues it
does not mention append in id order — so an untouched queue reads exactly
as it did before there was an order file, and an issue created during a
reorder cannot be lost.

Five MCP tools — `issue_list`, `issue_create`, `issue_claim`,
`issue_update`, `issue_link` — are served on **every** environment
socket, the primary's included, because the user's own agent files
issues too. What the socket decides is not whether they exist but who
the caller is: a claim's assignee and a comment's author are the accept
environment, never a parameter.

**Ordering, editing and deleting are the user's, and are deliberately not
MCP tools.** Agents create and claim; the person with the queue in front
of them decides what matters next, retitles what was filed badly, and
unmakes mistakes. `issue_move`, `issue_reorder`, `issue_delete` and the
title/label half of `IssueChange` are IDE-side functions for the
environments tab, compare-and-swap like every other write on the ref.
(An orchestrator-authored reorder is a plausible later addition; it is
not needed for the loop below, and a tool that lets an agent promote its
own work above the user's is worth thinking about before it exists.)

Durability rides the user's own push: the IDE's push includes
`refs/taste/issues:refs/taste/issues` when the ref exists, and is
byte-identical to the old push until it does. Sync fetches the remote's
ref into a tracking ref and fast-forwards the local one when that is
clean; when both sides moved it says so in one line and changes nothing.
That is the compare-and-swap problem across two machines, and a merge UI
is not the alpha's answer to it. Agents never push it anywhere.

**Four states, and only one of them is written down.** An issue is
**Queued** (filed, nobody holds it), **Active** (an environment claimed
it), **Completed** (done, and its work is merged) or **Declined** (it will
not be done — and the record stays, which is what separates declining from
deleting).

Only the *resolution* is on the `state:` line: `open`, `completed` or
`declined`. Active is derived from the assignee, because the assignee is
already where "who is working on it" lives, and a stored second copy is a
mechanism that can disagree with the first — the one that drifts is always
the one nobody is looking at. That also makes the format read forward:
`state: closed`, everything written before there was a second way to end,
parses as Completed because that is what it meant. Nothing migrates and
nothing resets a ref full of the user's own prose.

Declining exists because a queue that could only be closed as "done" had
one honest way to say "we are not doing this", and it was `issue_delete` —
which takes the id away and with it any way to find out the idea was ever
had, let alone why it was refused. A decision is worth keeping. So the
fourth state is the decision, written where the next person to have the
same idea will find it.

**The lifecycle the tools carry** (the loop is: the user and the
orchestrator write issues; worker agents — any ACP agent, any lab —
pick them up; the orchestrator completes them once the work is merged, and
the user declines what is not going to happen):

- **A claim is a structured env↔issue link, readable from both ends —
  and drawn from ONE.** `issue_claim` sets the assignee-environment from
  the socket; the second writer's compare-and-swap fails, it re-reads, and
  it is told who holds it. From the issue you get the environment; from the
  environment you get what it is working on (`claims_for`). Push dispatch
  (`chat_create` seeded from an issue) and pull dispatch (a worker browsing
  `issue_list` and claiming) are the same tools in different directions.

  Which end the *interface* draws is a separate question, and the answer is
  one of them: **environments narrate, issues have states.** An
  environment panel row says what its world is working on — that is the
  question you look at the fleet to answer. A backlog row says which of the
  four states its issue is in, and nothing about any world. Both panels
  used to draw the link, eight pixels apart in opposite orders, and the
  queue's copy was the one that was not the queue's question. It survives
  as the state glyph's tooltip, which is the right size of answer for
  "which world has i-0007".
- **Destroying an environment releases its claims**, with a comment on
  each saying why. An issue assigned to a world that no longer exists is
  unclaimable by anyone else and looks, in the queue, exactly like work in
  progress — silence there is worse than either alternative. A released
  claim puts the issue back to **Queued**, which is the same path a
  rejected review takes: rejecting is a judgment about the work, not about
  the need, so the issue is not declined for it. The need survives its
  first attempt; the comment trail says what was already tried.
- **Completing requires verified mergedness, not belief** — and the check
  is in the *tool*, not in an agent's good intentions. The branches
  checked are the issue's explicit links **and the branch of record of the
  environment that claimed it**, so claiming an issue and publishing
  unmerged work holds the close whether or not anyone called `issue_link`.
  It is the same `taste_git::Mergedness` the review lifecycle asks
  (`ahead == 0` against the user's current branch); otherwise the call is
  refused, naming the branch and its ahead count, and nothing is written.
  An issue with no branches behind it completes freely: not every issue
  produces code, and an environment that claimed something but has never
  published is not evidence of anything. Links record the branch tip as
  well as its name, because the honest workflow merges and then deletes
  the branch — without the tip, that issue would be unclosable forever.
- **Declining requires nothing, and that is not a hole in the gate.** The
  gate asks whether the work is in the target branch; a decline says there
  will be no work, so there is nothing to verify and demanding evidence
  would only make the honest answer unwritable. It is not a way around the
  gate either: it changes what the issue *claims* — from "this was done" to
  "this was decided against" — and an agent that declined its way out of
  unmerged work would be writing that decision down under its own name.
  Same transaction as every other end, and the comment is not optional:
  `issue_decline` writes `Declined: <reason>`, which is what the backlog's
  state tooltip reads back.
- **The user authors in the Backlog panel**, in the file-tree flank,
  directly under the Environments panel. It was a section of the console's
  environment tab, which put a *workspace* fact inside the pane that is
  about the environment you are in, behind a tab you had to switch to.
  Beside the fleet is where it belongs, and the adjacency is the argument:
  the two panels are one thought, and each says one half of it — the panel
  above names an environment and what it is working on, the backlog below
  names an issue and what state it is in.

  It is **collapsible**, where the Environments panel is permanent: the
  panel names where you are, and an indicator a panel can displace is not
  an indicator; the backlog is something you consult. Rows are in the
  `order` file's order, top first, and carry **a state glyph and a title**.
  Nothing else — no environment, no second traffic light. Three of the four
  glyphs are one checkbox at three points of its life (empty, dashed,
  ticked); Declined leaves the family for a circle-and-slash, because it is
  not a checkbox outcome, and its title is struck through. Only Active is
  at full strength: weight rather than hue, because this flank already
  spends colour on traffic lights. The claiming environment is on the
  glyph's tooltip, in the name the panel above uses for it — one fleet
  assembly, so the two surfaces cannot disagree about what a world is
  called.

  A row is **not activatable**. It used to be: clicking a claimed issue
  aimed every pane in the window at the environment holding it, which is a
  jump with no affordance, off a row that looked exactly like the unclaimed
  rows around it. Selecting an issue selects the issue.

  A row is reordered by **dragging it** where you want it, or from the
  row's **own context menu** — move to top, up, down, to bottom, then edit,
  decline and delete in a section of their own. Right-click, long-press, or
  the Menu key on the focused row, because an action reachable only by
  pointer is not reachable. An action that is meaningless on a row (the top
  row cannot move up; an issue that already ended cannot be declined) is
  shown disabled rather than hidden: an item that vanishes teaches a
  different menu each time, where an insensitive one teaches the reader
  where in the list they are and what has already been decided.

  The rows carried six hover buttons once, and the reason they are gone is
  worth keeping. Every defect they had came from one place: a control that
  appears under the pointer, on a list that **rebuilds itself whenever
  anything writes**. The click destroyed the very button handling it, so
  the reveal (`:hover`, `:focus-within`) died with it, the keyboard lost
  its focus outright, and — because the rows had just re-sorted — the row
  that swapped into that spot got the second click. Worse, the delete
  confirmation appeared in the exact slot the delete button had occupied,
  wearing the same trash glyph, so a double-click destroyed an issue
  having asked nothing. A drag has none of this: the gesture ends before
  anything rebuilds, and it says where the row is going by putting it
  there. The menu is built per summoning and dismissed before the write it
  starts, so nothing it holds can be disposed under it. In both, **the row
  identity is the issue id, never a list index** — an index means
  something different the instant the list moves, which is exactly when
  these actions are used.

  The `+` opens an **inline composer** — title and body, in the panel, no
  modal, the same convention the file tree's dirty-file flows follow — and
  editing reuses it. **Decline sits directly above Delete** in the menu
  because they are the same gesture with opposite consequences, and the
  choice should be one item apart: declining keeps the record and is
  undoable, deleting takes the id away for good. So deleting confirms
  **inline on the row** — arriving from a menu that is already dismissed,
  which is why the confirmation can no longer appear under a pointer that
  has not moved — and declining does not: there is no honest undo for a
  delete on the issues
  ref, because the id cannot come back, and a toast offering one would be a
  lie. Every write is off the
  main thread and optimistic: the row moves now, the compare-and-swap
  follows, and the refresh is the correction. A write that loses its race
  is re-read by `taste-git`'s retry, so what lands is the winner's list. A
  write that *failed* is put back by the panel itself — the refresh cannot
  do it, because git still says what it said before and every reader of
  the queue is equality-guarded, so nothing would announce and the row
  would stay where the gesture optimistically put it.
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
  lifecycle and the issues ref, both IDE-mediated. An agent environment gone
  hostile can burn its own clone and its own container, and nothing
  else.
- **The ACP terminal extension becomes served wherever the agent
  relocates** — container mode and, since the baseline shipped, safe mode
  too (unserved only at the rung below both, which has no exec target to
  relocate into). ARCHITECTURE.md's "no third route to a process" holds
  where it was argued — the outside-confined topology. Inside an
  environment the agent already executes beside the files; the extension
  trades nothing and buys the user live visibility of every command the
  agent runs.
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

## The substrate: where containers run

Decided 2026-08-31, spiked (`docs/spikes/vm-substrate.md`), shipped
2026-09-01. Agent activity should sit behind KVM, not only rootless
podman — the trust model's "kernel escapes are out of scope" line gets
retired once N autonomous agents run semi-unattended. The requirements
that shaped it:

- **Container builds run in the VM too, not just containers.** The build
  executes repo-supplied `RUN` steps — the earliest and least-confined
  untrusted-code path in the system — so any substrate that covers runs
  but not builds misses the sharpest edge.
- Devcontainer compatibility is non-negotiable (same devcontainer.json,
  same images); rootless is non-negotiable.

The spike settled the candidate question — **`podman machine`, for
everything, and no `krun` variant**. krun was disqualified on capability
rather than speed: it cannot `podman exec`, which is the transport the
environment channel, relocation, `ide_exec` and live shells all ride; it
cannot run systemd as PID 1; and it ignores `containerUser`/keep-id,
which is a devcontainer-compatibility break. Read the spike for the
numbers.

**What shipped is not a machine feature. It is a connection
abstraction.** The output of the whole subsystem is a
`taste_core::PodmanTarget` — a name podman knows — and every podman
invocation in the IDE composes against one. That choice is what makes
the tiers below peers rather than special cases:

| Provider | Containers run | Reached by | Status |
| --- | --- | --- | --- |
| `Local` | the user's host | the local rootless service | the default, unchanged |
| `Machine` | a local VM, behind KVM | the connection `podman machine` registered | shipped |
| `Remote` | any host with podman | a connection over ssh | transport shipped, gated (below) |
| cloud | a VM the IDE provisions | *a provisioner that returns a connection* | future |

A cloud VM is not a fourth kind of thing. A provisioner authenticates to
GCP/AWS/Azure, creates a host, registers a connection, and hands back
`Remote`. Provisioning reduces to **produce a connection**, and nothing
downstream learns a new word — which is the point of not adding a `--vm`
flag. It is also what makes the end state David named reachable: the
*coordinator* environment running persistently on a cloud VM is a
coordinator whose substrate is a connection that outlives the IDE
process.

### How the provider is chosen — convention, not configuration

1. the connection named by `TASTE_PODMAN_CONNECTION`, if set (the alpha
   seam for a host you registered yourself with `podman system connection
   add`, and how the remote tier is verified until a provisioner exists);
2. otherwise the machine named `taste-ide`, **if one exists** — creating
   it is a deliberate act, so its existence *is* the choice;
3. otherwise local podman.

There is no substrate setting, no sizing knob and no per-project
substrate. Machine sizing is IDE-decided and derived from the host:
memory is a quarter of host RAM clamped to 4–12 GiB, vCPUs are half the
host's capped at 8, disk ceiling 64 GiB.

**Creating the machine is the one affordance this batch does not ship.**
`Machine::create` exists, sizes the machine and arranges the helper
binaries; nothing in the UI calls it yet, because a button that commits
several GiB of the user's RAM is a design decision, not a wiring task.
Until it has one, a machine is created by the live test
(`TASTE_MACHINE_TESTS=1 … --test machine`) or by hand with the IDE's own
helper arrangement in force:

```sh
H=~/.local/share/taste-ide/helpers      # written by Helpers::arrange
CONTAINERS_CONF_OVERRIDE=$H/containers.conf PATH="$H:$PATH" \
  podman machine init --cpus 8 --memory 7936 --disk-size 64 taste-ide
CONTAINERS_CONF_OVERRIDE=$H/containers.conf PATH="$H:$PATH" \
  podman machine start taste-ide
```

From then on the IDE finds it by itself and says so in the app log. To go
back to the host: `podman machine rm -f taste-ide` — the environments
inside it go too, and the next reload rebuilds them locally.

**Never degrade silently.** A machine that exists but will not start —
no KVM, no helper binaries — falls back to local *with a reason*, which
lands in the app log and a toast. An IDE that quietly ran on the host
after the user asked for a VM would be telling them their agents are
behind KVM when they are not.

### The machine, concretely

- **One machine hosts every environment**, not one per environment. It
  costs ~1.35 GB idle and ~20 s to boot, and it hosts ordinary podman, so
  N environments inside it are N containers exactly as before. One VM per
  environment would multiply a fixed cost by the number the fleet exists
  to grow.
- **Helper binaries, arranged in user space.** `podman machine start`
  needs `gvproxy` (absent from an immutable Fedora host) and `virtiofsd`
  **on `$PATH`** (installed, but at `/usr/libexec`). The IDE fetches
  gvproxy version-pinned and sha256-verified into its own data directory,
  symlinks the system virtiofsd beside it, and points `[engine]
  helper_binaries_dir` at that directory through `CONTAINERS_CONF_OVERRIDE`
  — **scoped to the machine lifecycle commands only**, never exported,
  never written into the user's own `containers.conf`. Nothing is
  installed on the host and no `rpm-ostree` operation is ever run. The
  hash is re-checked on every arrange, so a corrupted or substituted
  helper is self-healing rather than sticky.
- **Sizing is a commitment, not a ceiling.** qemu runs with a memfd
  backend and no balloon, so guest page cache ratchets host RSS to the
  configured memory and never returns it (measured: 1.3 GB idle → 8.4 GB
  after one image build and one cargo build). The machine therefore
  appears as its own row in the environment Resources view — *"taste-ide
  — running, 8 vCPU, 7.8 GiB committed, 4.3 GiB on disk of 64 GiB"* —
  because no per-environment number can explain memory the VM took and
  disk a sparse qcow2 will not give back.
- **Machines are cattle.** The answer to a machine that is wrong is
  remove and recreate, not repair: it holds nothing the IDE cannot
  rebuild, since images rebuild from configs and clones live on the host.
  What that costs is every container inside it, so
  `Supervisor::reconcile_container_presence` asks whether the container
  an environment believes in still exists and reports the environment
  *down* rather than phantom-running. Without it `ide_exec` would fail
  with podman's "no such container" instead of the IDE's "this
  environment is down", and chats would keep trying to relocate into
  nothing.
- **Idle-stop stops containers, never the machine.** A stopped machine
  takes every environment down at once and costs ~20 s to come back.

### What did not change, and why that is the result

Per-environment volumes were already the design, and the spike showed
they are load-bearing rather than an optimization: moving `target/` off
the shared filesystem into a VM-local named volume is worth 30% of a cold
build (70.5 s vs 100.0 s), which is what keeps the machine within 7% of
a CPU-matched host. The stdio-over-`podman exec` environment channel
crosses the VM boundary transparently — one transport for SELinux hosts
and VM substrates alike — and `AgentHosting` probes whatever the
substrate actually is, unchanged. The relocated agent follows its
container onto the substrate because the connection rides on the
`Relocation` value: a container name alone is not an address.

**One rung deliberately stays local: the outside-confined agent**
(`taste_acp::sandbox`). It is the fallback for an environment with no
container to relocate into, and it is built out of host sockets — the
IDE's MCP socket, the URL bridge, `--network=host` for the OAuth
callback. A unix socket bind-mounted through virtiofs is not connectable
from inside a VM and the host's loopback is not the VM's, so moving that
rung onto a machine would produce an agent with no tools and no way to
log in. Its confinement is unchanged; what runs on the substrate is the
topology the design actually wants, the agent beside the files in its
environment's own container, which is where the isolation is for.

### The one compatibility rule the substrate imposes

**Every host path the IDE binds into a container must be under the
machine's shared set.** The default share is `$HOME:$HOME` and it can
only be set at `init` — `podman machine set` has no `--volume`. `/tmp`
cannot be shared at all; podman refuses that destination by name. Binding
a path the VM does not have fails loudly (`statfs …: no such file or
directory`) rather than mounting an empty directory, which is the good
failure mode, but it is still a failure. Today's topology survives
because checkouts, clones, the build-context staging directory and the
baseline definition all live under `$HOME`/`$XDG_STATE_HOME`. Anything
future staged in `/tmp` breaks this, and the live suites are the tripwire.

### Remote substrate: what is proven, and the gate

The remote provider is **proven end to end** against a real `ssh://`
podman connection: environment lifecycle (image built and container
started over there), `ide_exec` through `ExecContext`, and the
environment channel — including the production `AgentHosting` reach probe
— all round-trip through it. A running podman machine *is* an
ssh-reachable podman host (`podman system connection list` shows its
`ssh://core@127.0.0.1:PORT` endpoint), so pointing the remote provider at
one exercises the whole path with nothing faked.

**What a genuinely foreign host differs in is not the transport. It is
the files.** A machine shares `$HOME` over virtiofs, so an environment's
checkout exists at the same path on both sides and nothing moves. A
foreign host has no such share, so the clone would have to live *there*,
and mediated publish would have to cross the wire. That is **clone
locality**, it is the gate the real remote and cloud tiers wait behind,
and it is deliberately out of the substrate batch. Until it lands,
`TASTE_PODMAN_CONNECTION` pointing at a host that does not share the
user's `$HOME` will fail at the bind, loudly.

**Safe mode joins the same substrate — shipped, ahead of the VM work.**
The IDE ships a **baseline environment definition** in-tree
(`data/baseline-environment/`, compiled into the binary and written out at
first need) — git, node for agents, inspection tools, no project toolchain,
on a digest-pinned `fedora-minimal` base. An environment whose own config
is broken, unbuilt, or absent runs the baseline instead: same topology as
container mode, different config authority. What this changes and what it
does not:

- "No exec in safe mode" was derived from absence — the only target
  would have been the host. A baseline container is not the host; the real
  principle (no agent process on the host, ever) is untouched, and the
  repair loop gains real tools. The gates ask
  `ExecContext::has_exec_target()` and still refuse when it is false.
- The write wall stays real: the baseline mounts the env's clone
  **read-only** — on both binds, since the host-path bind would otherwise
  be the way around the first — while writes remain IDE-mediated through
  `write_allowed`'s safe-mode scope, still the single source of truth. The
  mount is strictly the more restrictive of the two, never a second opinion
  about what is writable. Reads go native — the one mode where the
  read-only bind was always the right answer.
- No nested container runtime, unchanged: builds stay IDE-supervised.
  The agent-authors / user-applies split is unchanged, and the baseline
  declares **no lifecycle hooks**, so the fallback itself asks nothing of
  the consent gate.
- `NoConfig` stops being a dead state: a repo with no devcontainer gets
  the baseline immediately — one environment is always usable.
- The outside-confined topology (bwrap, stand-in workspace, sibling
  agent container) is kept only as the rung of last resort for a broken
  substrate, and becomes deletable the day that rung is judged
  unnecessary. One topology, two config authorities — that is the end
  state.

**Three things the implementation settled.** First, the mode predicate had
to split. `ExecContext::is_container()` was answering two questions that
agreed only because safe mode had no container — "is the project's config
in force" (writes unlocked) and "is there anywhere to run" — and the
baseline answers them differently. `is_container()` keeps the first, so
every write check, tree lock, mode label and agent aim stays correct
untouched; `has_exec_target()` is the second, and is what the exec gates
ask. Second, the authority rides on a `taste.authority` container label as
well as on the exec target, because adoption at startup cannot recover it
from the config on disk: a baseline container running beside a config the
agent has since repaired is exactly the case that matters, and reading the
config would adopt it as the project's. Third, drift collapses to one
question — does the running container match what the ladder resolves today?
— which is also how the repair loop *finishes*: a project config that has
just become healthy while the baseline runs reads as drift, so the banner
lights and `devcontainer_reload` asks the user to apply it.

**Naming and images.** The baseline is an ordinary `DevcontainerConfig`
staged at one fixed, machine-wide path, so it flows through the existing
machinery with no parallel copy of it: `taste-img-<build-hash>` by content,
`taste.workspace`/`taste.env` labels, reconciliation by label. The fixed
path is load-bearing — `config_hash` covers the config file's own path, so
a per-workspace staging directory would give every workspace its own copy
of a byte-identical 300 MB image.

**The agent process relocates too — wired.** `ChatPane::relocation` asks
`has_exec_target()` rather than `is_container()`, so in safe mode the agent
runs *inside the baseline container*, beside the files, exactly as it does
in container mode. Terminal advertisement came with it for free: it is
derived from the relocation this same spawn computed rather than re-decided
from the mode, which is why there was one predicate to change and not two.

The mode predicate stays where it was. `AgentAim::safe_mode` still reads
`is_container()`, and must: the agent is in a container, but it is the
IDE's container, the checkout is bound read-only, and the write scope is
still safe mode's. Relocation answers "is there somewhere to be"; the aim
answers "whose config is in force". The rung below both is unchanged — no
podman, nothing to relocate into, and the outside-confined topology with no
exec target at all is what remains.

**Packaging, noted not solved.** For alpha the baseline image is built
locally by podman on first need. Bundling it as an OCI archive in the
Flatpak — so the rung that must always work never depends on a registry —
is a packaging task, not a design one.

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

0. ~~**Multi-chat tabs**~~ — **shipped, then superseded (2026-09-01).**
   N ChatPanes in an AdwTabView, with a chat list in WorkspaceState. The
   laziness it introduced survives and was the point — a remembered chat
   connects on first selection, never at startup, which is the same
   laziness environments needed — and so does one chat per environment,
   which the strip enforced by hand. **The tab strip itself is gone.**
   Once every chat had an environment, a strip of chats was a second
   environment switcher beside the panel that is the real one, able to
   disagree with it about where the user is. The chat pane now shows the
   selected environment's conversation, one per environment, keyed in the
   state so nothing can recreate the situation (v5). "New chat" is not a
   gesture any more: a new conversation is a new environment.
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
3b. ~~**Mediated publish + review inbox**~~ — **shipped, and the inbox
   half has since been REPLACED by the review lifecycle (phase 9); what
   follows is what 3b landed.**
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
   adapter (`@agentclientprotocol/claude-agent-acp` 0.73.0) never sends
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
5a. ~~**Fleet view + watching**~~ — **shipped.** The console is the
   environments view (a list then, one environment's detail now): name
   (human when given, slug otherwise), mode and container state live off
   the tagged events, bound chat with a busy indicator, branch,
   published-branch count, an unpublished marker, disk footprint and
   per-environment token spend,
   with Start/Stop/Rebuild/Nuke, Open, Rename and Destroy per row and the
   selected row's build log, shells and podman resources beneath (all
   three later flattened into the strip's own tabs; see phase 10). The
   row model is pure data (`taste-app/src/fleet.rs`) assembled from the six
   places those facts live and tested as such — gadget mode and the varlink
   read model consume rows, not six sources. Two costs are kept off the
   render and off the main thread: the per-environment git pass and the
   footprint walk, both cached and refreshed on demand. Destroy enumerates
   what the clone holds *before* the button becomes sensitive.
   Watching landed whole: "Open Environment" — from a fleet row or a chat's
   own environment row — aims the tree and git views at that clone, says
   so on the environment panel pinned under the tree (which is also the
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
   composer in the environments tab (the queue became an ordered backlog
   in phase 9); `openIssues` through `fleet::snapshot`
   to the card and the socket (read model v2); and the ride-along on the
   user's push and sync. The ref substrate gained `commit_to_ref_at` on the
   way — see "Issues: a ref, not a service" for why a swap against the
   ref's *current* tip is not a swap at all.
8. ~~**Baseline environment**~~ — **shipped** (the safe-mode half of the VM
   substrate, taken ahead of the VM itself because it needed none of it).
   Safe mode stops meaning "no container": an in-tree, digest-pinned
   baseline definition carrying node, git and an inspection set runs
   whenever the project's config is absent, unbuilt, malformed or refused
   by the security validator, with the clone bound read-only and the
   security validator still running *before* the rung is chosen. The mode
   predicate split in two (`is_container` keeps meaning container mode, so
   every write check and lock stayed correct untouched; `has_exec_target`
   is what the exec gates ask), the authority rides a `taste.authority`
   label so adoption cannot mistake a baseline for the project's, and drift
   became one question, which is what makes a repaired config raise the
   banner. `NoConfig` is no longer a dead state. Proven on real podman with
   SELinux enforcing (`taste-devcontainer/tests/baseline.rs`): the
   container starts for a repo with no config, node and git run in it, a
   write to the checkout from inside fails and leaves the host copy
   untouched, an IDE-mediated write to `.devcontainer/` lands and is
   visible through the read-only bind, and the repaired config then reads
   as drift. Two bugs fell out: the config watcher was armed only after a
   successful parse — so a malformed `devcontainer.json`, the one file the
   repair loop exists to edit, raised no events — and the baseline's shared
   staging directory needed atomic writes, because two environments coming
   up together is the normal case and `fs::write` truncates before it
   fills. Still open: relocating the agent *process* into the baseline (one
   predicate in `ChatPane::relocation`), and bundling the image as an OCI
   archive rather than building it locally on first need.

9. **One branch per environment + the review lifecycle** — **shipped,
   model and surfaces.** The three moves are one idea: the environment is
   the unit of review. `agents/<env>` is
   derived from the id and moved by every publish, so `publish_branch`
   collapsed to `publish` with no topic to name; the inbox became a state
   each environment is in (Working → FlaggedForReview → Merged/Rejected →
   destroyable), persisted with the environment (state v6) and stopping the
   container when it leaves Working; and `branches_published` became
   `review_list`. Mergedness stopped being two copies of `ahead == 0` and
   became one function the close gate and the review state both ask. A
   claim is now a first-class env↔issue link readable from both ends,
   released with a comment trail when its environment is destroyed, and
   enough on its own to arm the close gate. The queue gained a
   user-authored `order` file — IDE-side operations, deliberately not agent
   tools. `agents/<env>/<topic>` is a dead generation: reported, never
   migrated.

   The surfaces followed. The Inbox filter is **deleted**, not deprecated:
   review is a state, the fleet is the list, and a flagged environment is
   marked on the row you already look at (an accent rail and an eye —
   deliberately not a fourth traffic light, since a flagged environment's
   container is stopped and its light is honestly red). The console's
   environment detail leads with a review band carrying the branch, the
   target, the ahead count and the mergedness, plus Open Review, Merge,
   Reject and — once settled — a Destroy with nothing left to warn about.
   Open Review is where the inbox's own machinery survived: one branch's
   changed files against the merge base, which is why the filter could be
   deleted rather than replaced. The console's Issues section became the
   Backlog panel in the flank. Gadget mode stopped being a bespoke card
   and became those two panels, moved. `BranchesArrived` became
   `ReadyForReview`, one per environment. The varlink read model went to
   **v4** — the first bump that removes a field: `inbox` (a sum of
   published branches, which counted checkpoints) is gone, replaced by
   `flaggedForReview`, and rows gained `review` and `workingOn`.

10. **One flat strip, and nothing above it** — **shipped, 2026-09-02.**
    Two moves that arrived together and are one idea: the console stops
    nesting, and it stops repeating the panel.

    Log, Shells and Resources were an `AdwInlineViewSwitcher` over an
    `AdwViewStack` inside one pinned "Environment" tab — a row of
    tab-shaped controls under a row of tabs. They became real pages in the
    console's own `AdwTabView`, siblings of Services and of every
    terminal, and the responsive ladder went with them: below
    `CONSOLIDATED_MAX_WIDTH_SP` those pages are *transferred* into the
    editor's strip (`tabfamily`, `Editor::graft_pages`), so the window has
    one tab strip and a terminal's pty crosses the breakpoint untouched.
    `TASTE_PROBE_ROUNDTRIP=1` makes the trip and shoots what came back,
    which is how "nothing rearranged" is checked rather than asserted.

    What described the environment landed, for one round, in a pane header
    above the strip. That header is now deleted outright, and every fact in
    it found a new home rather than being dropped. Name: nowhere, because
    the environment panel already names the selected environment and
    nothing below it should say it again. State, working-on and the review
    band moved into the environment tab's own content (what that round
    called "Log"), which now leads with an `AdwBanner` when the environment
    is flagged — a persistent condition wants a persistent widget. Tail
    moved into a toolbar directly above the log it controls. Refresh and
    the environment `⋮` menu became that tab's first row. New Terminal
    went there too, and has since come back out to the tab bar's end
    where it belongs (2026-09-02) — the objection that put it in a page
    was real but the conclusion was wrong: this pane's pages move to the
    editor's strip at the consolidated rung while its tab bar stays
    behind, so an end widget *left there* is a control that leaves the
    window at 960sp. Bar furniture does not graft, so the rung change
    installs it on whichever bar is hosting the family instead. Container
    state, drift and "you have to answer something" became the
    environment tab's icon, indicator and `needs-attention`.

    The three fixture tabs are pinned, which is how `AdwTabBar` renders
    them **icon-only with badges** — and the pin travels with them: in the
    editor's strip at the consolidated rung they are the same three
    icon-only, unclosable pages, as is the chat's grafted trio. It comes
    off only for the crossing itself, so a transfer never has to have an
    opinion about which section a page is in.

    The Shells tab is deleted: terminals already have their own tabs, so a
    roster listing the same shells a second time was two lists of one
    truth. Ownership (agent-owned vs. the user's own) and exit status read
    off the terminal tab itself — an indicator badge for the one that is
    not the user's own, another for one whose process has exited — and an
    exited tab keeps its output on screen until the user closes it by hand
    rather than closing itself on a countdown. `taste_core::ShellRoster` is
    unchanged and still backs fleet counts and the varlink read model; only
    the console's UI listing of it is gone.

Each phase lands green (`cargo test --workspace` in the devcontainer),
updates ARCHITECTURE.md for what it changed, and is independently
useful.
