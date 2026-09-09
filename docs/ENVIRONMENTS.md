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
3. **Supervision: the coordinator chat plus a fleet view.** The
   coordinator is the primary environment's chat — the user's own — with
   nothing to designate. Human and AI supervision share one surface;
   sub-chats are ordinary chats the user can also drive by hand, at their
   own model settings — each in its own environment, reached by selecting
   it.
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
— `issue_start` makes the pair. So the chat tab strip is gone,
`ChatEntry::environment` is required, and the state is keyed by it
(`WorkspaceState::set_chat`; state v5, v4 discarded rather than merged).
Wanting a second conversation means wanting a second world, and a world
is an issue in progress: write the issue down and Start it.

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

**A clone shares no inode with anything, and that is a boundary
requirement.** A local clone's default is to hardlink the whole of
`.git/objects` — libgit2 does it exactly as `git clone --local` does, and
it is normally free. It is not free here. Every clone is bind-mounted into
a container with `:Z`, and `:Z` means *relabel this tree with a private
SELinux MCS category*; a label belongs to the **inode**, so relabelling
one end of a hardlink relabels the other. Starting one environment
therefore rewrote the security label on the object store of every other
clone — each of which holds a different category pair, so SELinux denied
them the read and every git command in them failed with `fatal: bad object
HEAD` — and rewrote it on the user's own checkout under `~` on the way
through, since that is where the objects were hardlinked from.

Two of this project's lines meet there. *Nothing an agent or a container
runs reaches the user's home*: a container's mount option was rewriting
metadata on files in `~`, through inodes nobody meant to share. And *the
boundary is the host, not the agent*: so the fix is to stop the sharing,
not to drop `:Z` to a shared label — the private label is what keeps one
environment's checkout out of another container's reach, and it was doing
its job. `taste_git::clone_local` passes `CloneLocal::NoLinks`, which
still bypasses the git-aware transport (no negotiation over a path on the
same disk) and copies the objects rather than linking them; one object
store per environment on disk is the honest price of an environment being
a separate world.

`taste_git::unshare_inodes` holds that postcondition whatever libgit2
did, and repairs the clones made before any of this was known: `reconcile`
runs it over every restored environment at startup, before anything mounts
one. It is idempotent and costs one `stat` per file in `.git` once there
is nothing left to break, so it needs no marker on disk saying it has
run.


**A row shows a stage and two conditions.** The pipeline is New →
Approved → Starting → Working → Review → Finished, with Declined the one
offramp (David, 2026-09-09). Each stage has its own glyph, and none of
them is a checkbox — the slot they sit in becomes a real one under the
pointer.

Rejecting a branch at review is deliberately **not** a transition:
"retracted at review just leaves an item in review … if I want to revise
it or request changes, I'll just ask for that". Asking is a conversation,
not a state. Finished **is** merged — this project has no "done but not
in" — and an issue is never created approved; approval is the user's, or
the coordinator's when the user asks for it.

**A stage is a position, never a condition**, which is the whole of the
split. `WorkState` mixed the two: `Waiting`, `Failed` and `Stopped` sat
in one enum beside `Review` as though they were places work had got to,
when they are things that can be true almost anywhere along it. They are
now the two badges on the glyph — attention at the upper right (the agent
is stopped on the user), health at the lower right — and `Standing` is
the derivation, with `Light::Off` and `Light::Unknown` producing **no**
health badge at all, because an environment deliberately stopped or never
started is not unhealthy. A settled row reports no health either.

The badges need the overlay to be bigger than the glyph (20px against a
13px icon, inside a 26px slot): aligned to the glyph's own box, two
ringed dots cover most of the icon they annotate.

**Interventions are a bar at the panel's foot, and they have a subject.**
Start, Stop, Rebuild and Delete came off the header (David, 2026-09-09).
A toolbar in a header cannot say what it is about, and these four all act
on something: the bar appears only when there IS a subject and names it,
and it is gone otherwise, which is the honest resting state for a panel
whose rows are what matter.

What it acts on is the **checked** rows, or the selected one when nothing
is checked, or nothing at all — and nothing is what hides it
(`backlog::targets_of`). Checking is done on a row's status glyph, which
becomes a checkbox under the pointer: a second way to say what an action
is for, because the selection aims the panes and opens the composer, so
it can only ever mean one row, and an intervention that wants three
environments needs somewhere else to say so. Checks win over the
selection whenever there are any — checking is deliberate, while the
selection moves whenever the user looks at a row.

The bar is one row of narrow square buttons and nothing else — no
subject line and no close box (David, 2026-09-09). The checks and the
selection are on screen directly above it, so a label restating them
spends a row of a panel that has none to spare, and unchecking is how the
bar goes away.

The glyph and the box share a `GtkStack` sized to the wider of them, so
the title beside them does not move when the pointer arrives. **Checking
one row shows the boxes on all of them**: the list is either in a
checking mood or it is not, and a half-checked list that showed a box
only where it was checked made the other rows read as unavailable. A button is
live when its action applies to at least one target and then acts on
every target it applies to; all-or-nothing would grey out Stop because
one of four checked environments happened to be down already. Checks
clear once an intervention has run.

Delete is the exception that stays one at a time: an issue with an
environment goes through the console's intervention, which names what
that clone holds before anything happens, so a batch asks once per
environment rather than once for the batch — a single "delete 4?" would
be a confirmation that hid the very thing it exists to show.

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
and log ring all become per-environment by construction rather
than by threading ids through a singleton.

The **watcher is the exception, and deliberately so.** Config watching is
one inotify instance for the whole fleet (`configwatch::ConfigWatch`,
owned by the registry, which is the only thing that knows what the fleet
is), because the two limits involved are nothing like each other:
`fs.inotify.max_user_instances` is 128 and per *uid* — and under rootless
podman with `--userns=keep-id` the IDE, the user's desktop session, every
editor they have open and every agent in every environment all spend from
that one budget, of which 67 were already gone on the machine this was
measured on before an environment came up — while
`max_user_watches` was 273731 on the same machine and one instance may
hold descriptors on any number of unrelated paths. So the fleet's cost
scales with the number of *paths* it cares about and not with the number
of environments: `<root>` non-recursively (which catches
`.devcontainer.json`, and the creation or removal of `.devcontainer/`
itself) plus `.devcontainer/` recursively whenever it exists, per
environment, all on the one instance, dispatched to the owning supervisor
by longest matching root.

It replaced a `Supervisor::start_watching` that opened an instance each,
whose failure was a logged warning at all three call sites — so an
exhausted budget produced environments that silently stopped noticing
config drift, with no banner and no toast, and drift is what gates
`devcontainer_reload`.

**A read is not a change.** notify's inotify mask includes `IN_OPEN` and
`IN_CLOSE_*`, which arrive as `EventKind::Access` — and `recheck`'s whole
job is to OPEN `.devcontainer/devcontainer.json`, inside the directory it
is watching. So every recheck raised an event that asked for another
recheck: a loop clocked by file IO, measured on a live IDE at 8,000
rechecks and 96,000 inotify events a second with a core gone. The handler
drops accesses, which cannot lose a change — drift is a question about
content, and Create, Modify, Remove and the renames all still come
through. It is the only fix available at this layer, because notify
chooses its own mask.

The loop was invisible before the fleet-wide watcher because the
per-supervisor one deadlocked itself on its first event: what would have
spun was already dead.

**Arming is once, and the queue coalesces.** `Supervisor::recheck` asks
for the recursive `.devcontainer` watch every time it runs, and an event
is what makes it run — so an arm that did real work on each call turned
one event into a `readdir` plus an `inotify_add_watch` per directory plus
a blocking round-trip to notify's event loop, and that churn kept the
cycle fed. Measured on a live IDE: 8,000 rechecks a second, 96,000
inotify events a second (48,102 opens of `devcontainer.json` in six
seconds), one core gone, and 25 MiB a minute accumulating in the queue
behind it. The watch is now armed only when it is not already armed — and
disarmed when the directory goes, so its return re-arms — and at most one
recheck per environment sits in the queue, since rechecks are not
additive and events arriving while one is queued are worth exactly one
more run. That second half is what bounds the queue at all: the producer
is an inotify stream and the consumer reads files.

**The rule the module exists to keep**: nothing on notify's event-loop
thread may call `watch()`, and no lock that thread needs may be held by a
caller of `watch()`. One thread both delivers events to the handler and
services `watch`/`unwatch` — `watch_inner` posts `AddWatch` and blocks on
the reply (notify 8.2 `src/inotify.rs`) — so a handler that calls
`watch()` waits on the thread it is running on, and a `watch()` caller
holding a mutex the handler wants deadlocks the pair.

The per-supervisor version had the first of those shipped. Its handler
called `recheck`, `recheck` re-arms the `.devcontainer/` watch, and that
`watch()` ran on the handler's own thread — so the *first* filesystem
event in any environment that had a `.devcontainer/` directory froze that
environment's watcher permanently. It is invisible by construction: a
dead watcher thread and a config nobody is editing look exactly alike,
which is why it survived. Consolidating is what made it a single place to
get right. The handler now locks the owner map and nothing else and posts
the recheck to a `taste-config-recheck` thread over an unbounded channel
— unbounded because a handler that blocks on a send is the same deadlock
in a different hat — and the watcher sits behind its own mutex that is
never held together with that map. The test writes to a watched
`.devcontainer/` and then asserts a further `add` completes, which only
passes while the event loop is still answering; with the old shape put
back it hangs instead. A supervisor no longer takes a watcher down with
it when it drops, so `destroy` calls `ConfigWatch::forget`, and the
instance itself goes when the last environment leaves. The same idea from
the other end is `WatchSlot`, which keeps *one* workspace tree watcher
re-aimed at whichever checkout is on screen rather than one per
environment ever opened — so the whole fleet, at any size, is three
instances: config, the aimed workspace tree, and the sign-in URL bridge. Events gain an environment id
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

**Design commitment, locked 2026-09-01: the panel at the foot of the
file tree is the app's single top-level control, and every other pane
shows the selected environment's resources.** Since 2026-09-05 that panel
is the backlog — one list: your own checkout, then every issue, a started
one carrying its environment — and the Environments panel it stood beside
is gone (docs/spikes/issue-is-the-environment.md). The file tree, the git
views, the editor's tab set, the console and the chat all render one world
— the one the backlog says you are in — and selecting there IS the context
switch. It is the only one: no pane has a switcher of its own, because a
second one is something the first can disagree with.

This supersedes two earlier descriptions in this document. Editor tabs
from a watched environment are no longer *mixed* alongside the user's;
each environment owns its tab set, stowed and restored whole. And the
console is no longer a list of every environment with a selection of its
own; it is the selected environment's detail, shown as a flat strip of
tabs and named nowhere — the backlog in the flank is the one place the
selected environment is named. What did
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
  very bottom of the file-tree pane** — below anything else that pane
  opens (its own interventions open inside it, under its list), because a context indicator that
  can be displaced by a transient panel is not an indicator. **It lists
  every environment, always, one row each**, primary first as the way back
  and named "Personal"; clicking a row aims the panes there. No menu, no
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
  The row's title is the issue's, because the environment IS that issue
  in progress: there is no name to put under it, and "what is it working
  on" is answered by the row itself. (An earlier round drew the issue's
  title as a dim second line under a generated name like `calm-1`; the
  generated name is gone, and with it the line.)
  Past seven rows the panel filters and scrolls inside itself instead of
  growing into the tree. Ctrl+Shift+E focuses it and walks the rows;
  Enter switches. Its header holds **+**, the composer, whose primary
  action is **Start**: the way to make a world is to write down what it is
  for, and it lives where the moving between worlds does. It replaced the
  "Viewing `<env>` / Back to Personal" bar the tree header used to grow, then
  the popover switcher that replaced that, then the Environments panel
  that replaced *that*.
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
  policy attached) and surface **in the chat, on the step that ran
  them** — the command, its output, and a Kill on the step while it is
  running: stopping a runaway process is supervision, not editing.

  The console had a tab for this and no longer does. First a tab per
  command, labeled `env · command`, on the grounds that the output is the
  record of what happened; the adapters serve their own shell tools over
  the terminal extension, so an agent's grep is an agent terminal, and an
  agent that grepped twenty times left twenty dead tabs to close by hand.
  Then one read-only `env · agent` tab per environment, pinned and
  unclosable, that every command accumulated into. That one earned
  nothing either: the transcript already carries each command AND its
  output, in order, on the step that asked for it, which is where anyone
  actually reads it (David, 2026-09-08: "it's honestly sufficient to just
  have it in the chat"). So the Kill moved to the step, and the tab is
  gone.

  ANSI is why the tab looked appealing — build output is colours and
  carriage-return progress bars, and a `TextView` shows the escape codes
  instead of obeying them — and ANSI still belongs in a *log*. What does
  not belong there is the agent's activity: an environment's build log is
  the record of the environment building itself, not of what an agent did
  inside it afterwards. `chatdoc::command_block` renders the escapes the
  transcript's own way.

  The **roster** (`taste_core::shells`) is untouched by any of this: every
  agent shell still registers there, because the fleet counts,
  `chat_status` and varlink read it — and because it is what the step's
  Kill finds its process through. An ACP tool call and a roster entry are
  two systems with no shared id, so they are joined on the command string
  (`chat::running_shell`).
- **The console's tabs are the user's own terminals; the agent's work is
  in the chat.** User terminals attached to the environment (interactive
  — they carry no Kill button; closing the tab is how they end) live
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
  an environment integrating N others' work — an integration issue the
  coordinator files and starts — merges N branches and publishes the
  result as *its own* branch of record.
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

It goes in **again** if it has to. This is the one send that builds its
card before spawning the agent — every other calls `activate` first and
builds the card afterwards — and spawning into a persisted session
replays that session's history, which `ensure_client` renders by
clearing the transcript first (three reconnects used to show "Hi,
Claude." three times). A prompt that has not been sent yet cannot be in
the history being replayed, so the clear took the user's own message off
screen and the agent then answered a question with no visible asker
(David, 2026-09-08: "it seemed to get the message, but it's not in the
chat"). `flush_revive_queue` checks whether the card is still the
transcript's and rebuilds it at the bottom if not, which is where the
thing about to be sent belongs.
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
- Orchestration tools (below) are served **only** on the coordinator's
  socket — the primary's — and other connections don't see them.
  (Shipped, phase 6; simplified 2026-09-06: the role is the primary's,
  always, and there is nothing on the server to write.)

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

## Supervision: fleet view + coordinator chat

**The fleet is enumerated once, and detailed once** (shipped, phase 5a;
scoped to one environment 2026-09-01; sections promoted to flat tabs, then
the console's own pane header deleted, 2026-09-02; the environment tab
itself dissolved 2026-09-06). The file tree's backlog is the list — your
own checkout and every issue, always, a started one with its environment's
traffic light and activity sparkline — and it is the app's **single namer**
of the selected environment. It is also, now, where an environment is
*acted on*: nothing below it repeats the name, the state, or the actions.

The console is what a row cannot hold: the **machine room** for the
environment the panes are aimed at, in **one flat strip of tabs and
nothing above them** — `[resources] [terminal…]`.

**Why the environment tab is gone.** It was the last surface drawing
things that had a better home. The state in words sat under a backlog row
that lights the same fact; the build log sat beside a Logs section that
opens the same stream as a document; a review banner offered Merge one
click from a row and none from a diff; a `⋮` menu of lifecycle actions sat
on a tab the user had to be looking at for it to say anything. Two
renderings of one fact are two things to keep in agreement, and the stale
one is always whichever the user is not looking at. So each fact went
where it is already read:

| What it was | Where it is |
|---|---|
| state in words, traffic light | the backlog row (its light, its second line) |
| what the mode means, the token spend, the publish ledger | the backlog row's tooltip |
| Start / Stop / Rebuild / Delete | the backlog header, on the selected row |
| Rename, Nuke, Open Review | the backlog row's `⋮` menu |
| Refresh everything | the backlog header, beside those actions |
| rename / destroy / reject panels | the backlog's intervention panel, under its list |
| the review banner's Merge and Reject, and the mergedness note | the review tab in the editor, under the comparison |
| the build and lifecycle log | a document, opened from the tree's Logs section |

Nothing is lost by the log's move: the supervisor's ring seeds the page
and `Event::DevcontainerLog` feeds it live, so a build is watchable while
it runs, and "View Log" opens exactly that page.

The fixture tab is **pinned, and therefore icon-only**: pinning is
how `AdwTabBar` draws a page as its icon alone, no title and no close
button, held at the strip's left edge — which is what it is. A
tab is a glance; the tooltip carries what a glance cannot.
(The pin travels: at the consolidated rung it is grafted into the
editor's one strip and is the same icon-only, unclosable page
there, as is the chat's grafted trio. It comes off only for the crossing
itself. See the responsive ladder.)

- **The resources tab** is the selected environment's podman objects
  (container, image, volumes with their own guarded removal), on its own
  rather than one page of a switcher. Its tooltip carries the **disk
  footprint** — it is the tab that enumerates the things that size is the
  sum of, and a footprint on a row the eye scans was answering a question
  nobody asks at a glance.
- **There is no Services tab any more** (2026-09-06). systemd units and
  their journals were a third fixture here; it was shelved for want of
  anything exercising it, and what it learned — and how a return should
  differ — is in `docs/spikes/systemd-services.md`.
- **Terminal tabs** keep short titles (`env · command`) — a terminal's
  identity IS its command, and four icon-only terminal tabs would be four
  indistinguishable tabs — but pick up the same badge convention for two
  facts of their own: an indicator marks a tab that is not the user's own
  (agent-owned, read-only), and a tab whose process has exited is marked
  exited and keeps its output on screen until the user closes it, rather
  than closing itself. The second overwrites the first, which is why it
  takes a different frame to photograph each.
- **New Terminal is at the tab strip's end**, right-anchored, because the
  strip it adds a tab to is the thing it acts on. It spent a round buried
  in a tab's content, and the reason is worth keeping: at the
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
- The strip carries a menu of its pages at the end, beside +, because an
  environment with a few terminals already scrolls a
  700px pane. A menu rather than `AdwTabOverview`: the overview
  is drawn to cover a window, and in a pane its header carried the
  window's close control and its way back sat in a different corner from
  its way in. GNOME Builder's frames do the same.

**What the console still owns is the model, not a drawing.** The
off-thread git and podman passes live here — every environment's branch
and unpublished work, the published branches, the merge-base question, the
issue queue, the footprint — and the fleet is assembled here from the six
places those facts live, as **pure data**, so the backlog, gadget mode and
the varlink read model render the same rows rather than re-deriving them.
The passes and the pane happen to share a file; only one of them is on
screen.

### The backlog's header, and what it costs

The header is a section header like Logs' and Ports': arrow, glyph,
title. After it come the count, the subscription gauge, and the actions —
Start, Stop, Rebuild, Delete on the selected row, then Refresh and New
issue, which are not about a row and are always sensitive.

Seven controls, a gauge and two labels in a 335px flank is a budget, and
it is spent down to the pixel:

- the actions are **one tight cluster**, not seven items on the header's
  own spacing. A toolbar group reads as a group when its own gaps are
  smaller than the gaps around it, which is how every GNOME header bar
  packs icon buttons — and six 6px gaps was a button's width taken from
  the only label here that can give any up.
- the gauge is **40px in both headers** (`gauge.rs`). Eight pixels of bar
  is nothing to a reader asking "how full"; eight pixels of caption is
  "4 · 3 active" against "4 · 3 …".
- Rebuild wears the platform's **build** glyph, not `view-refresh`, which
  is Refresh's four buttons along. Two identical glyphs on one line
  meaning "re-read the facts" and "rebuild the container" is worse than no
  glyph at all.

Measured rather than guessed: the panel's minimum is 286 against a 335
flank, so it does not decide how wide the column has to be, and the count
gets 58 for a natural 54.

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

      [file 1] … [chat] [usage] [agent] [resources]
      [terminal 1] [terminal 2]

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
  full width, the console's fixtures in its own strip — so a labelled
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

  The console's fixture crosses unpinned and is pinned again on arrival
  (`Console::begin_migration` / `Console::set_host`), so a transfer never
  has to have an opinion about which section a page is in. Nothing rides
  along beside the pages: the console has no header to bring, and nothing
  it draws is about anything but the page itself. Everything else survives
  the trip too — an unsent prompt, a live transcript, a terminal's
  scrollback.

  The pinned section is four icons wide and always on screen; the documents
  and terminals scroll beside it, and the pages menu at the strip's end is
  how a tab you cannot see is found.

  The flank stays put: it keeps its column, with the backlog in it. An
  earlier version of this rung also collapsed it;
  that made the window a stack of full-width bands, and took away the one
  pane that says which environment you are in.
- **Gadget mode** is not editing at all. The panes give way to the one
  panel that was already answering the supervision question: the backlog,
  moved into the window. The subscription gauge comes with it, being a
  child of the panel's own header. This used to be a bespoke card rendering the fleet snapshot — its
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
constants can only ever be raised by the arithmetic. The chat's term in
that sum is a constant (`chat_column.rs`: its width is never computed from
its content), so what moves the thresholds is the flank and the centre —
and the retune logs its arithmetic at info level (`responsive ladder
retuned`, in `ide_app_log`) so a rung change can be read back rather than
guessed at. The window's own
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
are two notifications and one chat asking twice is one. A finished turn is
the one exception and needs only the focus half: nothing waits on it, so a
user at the window is told by the tab rather than by the shell. A flag is
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

**Coordinator chat (shipped, phase 6; the designation dropped
2026-09-06).** The primary environment's chat — the user's own; same
ChatPane, same ACP agent, its own model settings — whose MCP connection
additionally serves the orchestration tools that *act* (`issue_start`,
`issue_reorder`, `chat_send`). The tools that *read* — `chat_status`,
`chat_transcript_tail`, `review_list`, and the issue tools — are served
on every socket since 2026-09-05: read-only, and coordination is simpler
when any agent can look. The full set:

- `issue_list` / `issue_status { issue }` — the issues, and through them
  the fleet: a started issue carries its environment as `runtime`,
  literally the row the console assembles and the varlink socket
  publishes, so the orchestrator and the user cannot disagree about what
  is running; `work` is the one derived state (`taste_core::work`), and
  `yours` is the user's own checkout. There is no separate environment
  listing, because an environment is an issue in progress.
- `issue_start { issue, agent?, model? }` — creates the issue's
  environment and its chat, hands it the issue as its first prompt,
  returns `{ chat }`, an id that IS the issue's. One chat per
  environment, one environment per issue: starting work *is* creating a
  world. It is created in the background; the user reaches it by
  selecting that row, and can take it over at any time.
- `issue_reorder { issue, position }` — move an issue in the backlog's
  queue; 0 is the top. The queue is the user's order, and the
  coordinator's brief is to keep it honest and say why when it moves
  something. Coordinator-only, like `issue_start`: a worker promoting its
  own issue is exactly what this must not serve.
- `chat_send { chat, text }` / `chat_status { chat }` /
  `chat_transcript_tail { chat, max? }` — drive and observe sub-chats.
  A chat may not `chat_send` itself; the prompt would only come back.
- `review_list { flagged_only? }` — where every environment stands for
  review: its branch of record, its mergedness against the user's
  branch, and its review state. Read from the hub.

**The coordinator is the primary's chat, and nothing designates it.** It
used to be a switch in a chat's settings, one per workspace, insensitive
on the primary — because sockets tell environments apart, not chats, and
every chat without an environment of its own shared the primary's, so
serving `issue_start` there would have handed it to all of them. That
premise is gone: since one chat per environment (state v5) there are no
unbound chats, and the primary's socket is exactly one conversation's —
the one that sits where the user does, in the user's checkout. So that is
the coordinator (David, 2026-09-06: "the chat/agent associated with my
personal/primary environment to be the coordinator with no configuration
otherwise"). `ChatEntry::role` went with the switch; an old state file's
`role` key is ignored. What the coordinator is *for* is in the
`instructions` its socket hands back at `initialize`, on top of the
backlog rule every socket carries: keep the backlog in the user's order
(`issue_reorder`), start environments for the most pressing items
(`issue_start`), add what the user asks for, and review what comes back
(below). Its authority is the fleet's and the backlog's, in full (David,
2026-09-06: "the orchestrator has authority over the fleet and the
backlog. It just should never be able to push to GitHub without my
involvement") — and that line is structural, not instructional: the
agent's sandbox has no push route and the credential proxy holds no git
credential, so what the coordinator merges waits in the user's checkout
for the user's push.

**Every agent is told to use the backlog.** Work the user asks for is
written down before it is done — an environment is an issue in progress,
and the backlog is the one list of what is wanted — so an agent asked to
do or change something files it with `issue_create` first, and before
filing shows the user the exact title and body and confirms them, because
the issue is the user's to read later. The one exception is written into
the same instruction: when the user has asked for a *set* of backlog
items, the agent files the set and shows the list rather than confirming
each (David, 2026-09-06). Follow-up work found while working an issue is
a new issue, not a detour.

**Chats are addressed by their environment, and environments by their
issue.** `issue_start` returns an id that *is* the environment id, which
*is* the issue id: it already exists, the backlog shows it under the
issue's title, a person can say it out loud, and it survives a restart,
where a tab ordinal does none of those. `"primary"` is a chat id like
any other — the coordinator's own — now that one chat per environment
leaves no unbound chat sharing the primary's socket; the one prompt it
refuses is the coordinator's to itself.

**`issue_start`'s order is the tool:** cap, the issue's pre-flight, create,
start, prompt. The refusals that cost nothing — the concurrency cap
(`taste_core::environment::MAX_ORCHESTRATED_ENVIRONMENTS`, six: soft in
the precise sense that it bounds the tool and not the user's own hand), an
issue that does not exist or is resolved, and an issue somebody already
started (named, with `started_by`) — happen before a clone exists. The
start is the store's compare-and-swap on `started_by`, so two machines
racing for one issue settle it in the ref, and the loser is told who won:
refused is the default, and the override can wait for someone to need it.
It happens *before* the task is sent, so a dispatch that loses leaves an
idle chat rather than one working somebody else's issue. Starting is the
link: the environment is named by the issue, and the close gate follows it
to `agents/<issue>`. `issue_link` survives for the case that cannot
express — work that landed from an environment other than
the one holding the issue, which is what integration produces.

There is deliberately **no user prompt per creation**. The gate that
matters is further in: the sub-agent's own permission prompts surface in
its own tab. A dialog whose only answer is yes is how consent gates stop
being read.

**The container starts first, and the agent starts inside it** (David,
2026-09-08: "outside the personal env, the container should be started
before the agent starts"). `issue_start` used to leave it stopped, so a
sub-agent began outside its container, in safe mode, and moved in later
if the user pressed Start — a topology nobody wanted, paid for with a
respawn and a `session/load`, and one the coordinator's own brief already
contradicted by telling it that `issue_start` "builds the environment's
container". Now the clone's container comes up, the agent is held back
until it does (`ChatPane::hold_for_container`), and the first prompt
queues in the meantime. If it cannot come up — no podman at this rung, a
build that fails — the agent starts outside it and says so in the chat,
which is what the rung below the containers has always done.

What this does NOT hand over is configuration authority. The consent this
paragraph used to rest on is `devcontainer_reload`'s, and that gate is
about a config that has **drifted** from the running container — the
agent-authored case, which is the whole of the risk (CLAUDE.md →
"configuration authority is execution authority"). A container started at
creation applies the config as cloned, which is the user's own, from
their own checkout, already running in their own environment. The agent
has written nothing yet; the ordering is what guarantees that. What the
user gives up is starting each sub-agent's container by hand, so a repo
whose committed lifecycle hooks are hostile runs them once per
environment rather than once — the same hooks, more times, still gated by
`taste_devcontainer::security`, and never a config an agent wrote.

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
(`taste_acp::authproxy::spawn_env`). Which model is the proxy's to know,
not the IDE's to remember: it asks the documented Models API what the
credential can run and offers the newest model above Opus in that list
(`taste_authproxy::models`), cached in the IDE's state so the first spawn
of a launch has last time's answer; an account with nothing above Opus
gets no row, because Claude Code's own picker is already complete for it.

Sub-chat permission prompts still surface in their own tabs to the user;
the orchestrator cannot approve on the user's behalf, and there is **no
tool that would let it** — `chat_status` reporting `awaiting-permission`
is how it learns to tell the user instead.

**The coordinator sits at the hub.** It is the primary's chat, and the
primary's checkout is the user's — the one every `publish` lands in — so
what a worker publishes is in front of the coordinator the moment it
lands, with no pull: `review_list` for where each environment stands, and
`git log` / `git diff` over `agents/<env>` in its own checkout for the
work itself. It has no clone of its own to integrate in (`publish` and
`update_from_main` are refused on the primary's socket, as they always
were), and that is the point: integration is a merge in the user's
checkout — the coordinator's own `git merge` of `agents/<env>` into the
user's branch, since that checkout is where it works — or, for work that
needs its own build and tests first, an *integration issue* the
coordinator files and starts like any other,
whose environment pulls the `agents/*` refs down through
`update_from_main` (which carries them — a Phase 3 requirement) and
publishes the combined result as its own branch of record. Phase 6 added
no git machinery for this and the simplification removed none.

**The IDE wakes the coordinator to review.** When an environment's agent
flags its work (`EnvironmentReviewChanged`, the review state `flagged`),
the window sends the primary's chat a prompt naming the environment and
its branch of record: call `review_list`, read the branch against the
user's in your checkout, merge it and complete the issue if it passes
(`issue_update state: completed`, which is verified against the merge),
or send the agent what to fix if not, and say which. Mid-turn it queues
like any other prompt; with no agent in the primary nobody is woken and
the user reviews alone, as they always could. The push is the user's.

**…and to triage what lands on the backlog.** The other wake-up is a new
item on the queue (`Event::IssueFiled`, David, 2026-09-08: "Wake up the
coodinator agent whenever a new item is added to the backlog. It can
decide what to do"): the coordinator is sent the id and title and told to
do whichever of its moves the item actually calls for — reorder it if it
outranks what sits above it, start it if it is ready and there is room
under the cap, link or decline it if the queue already covers it, or
leave it and say why — and to file nothing in reply.

Not its own filings. The event carries **who filed it**, published where
the issue is written rather than derived from a re-read of the ref,
because the ref cannot tell the coordinator's filing from the user's
(both carry `primary` as the reporter) and that is the whole of the
question. `by: None` is the user, in their own composer — the one filer
that is not an environment, and the one most worth waking for. A filing
the coordinator did itself is dropped: being told about the issue it just
wrote is a turn spent to learn nothing, and a loop if the reply files
another. An item the user files *and starts* in one gesture is dropped
too — they have already decided what happens to it.

**…and restarts it when it goes SILENT — if the user asked for that.**
The watchdog is a per-chat switch (chat Settings › **Restart when
silent**, `ChatEntry::restart_when_silent`) and it ships **off** (David,
2026-09-09: "disable it by default"). The asymmetry is the reason: waking
is additive and visible — a prompt lands in the chat and the user can
read it — while respawning kills a turn in the one chat the user talks to
themselves, so the IDE does not do it unbidden. With the switch off the
wake-up is still sent and nothing watches the clock afterwards; a wake-up
that could not be *sent* is noted in the transcript instead of retried.
The switch is only shown on the primary's chat, since that is the only
one the IDE ever wakes.

With it on: `coordinator.rs` reads the chat's own facts after ten
minutes (`ANSWER_DEADLINE`), and if nothing at all has happened in there
since — no chunk, no prompt, no turn ending, all of which
`ChatPane::touch` records — the IDE respawns the coordinator with its
conversation (`session/load`, the relocation mechanism), notes the
restart and the reason in the transcript, and asks again with a pointer
at the backlog and the review list, which are the high-level state a
fresh session picks things up from.

The deadline measures **silence, not elapsed time**, and it did not
always: `Streaming` was a restart reason on its own, and "did the errand
happen" was only asked of an idle chat. Between them those two restarted
the coordinator mid-conversation every ten minutes (David, 2026-09-08:
"the main chat gets restarted every 10 minutes because it's not properly
watching for turn-taking/chat activity") — and the coordinator's chat is
the one the *user* talks to, so what a respawn threw away was often the
user's own turn. A turn that ended after the wake-up closes the watch
whatever the chat is doing now; a chat that has done anything within the
deadline re-arms it, on the same restart count, because waiting for a
chat that is working is not a failed attempt at anything. The errand is
not lost by waiting: its prompt is already in the chat's queue and lands
when the conversation next comes up for air. What restarts the
coordinator is ten minutes of nothing, which is the wedge the deadline
was built for and the only thing a respawn actually fixes. No toast: the user may be asleep, and the chat is where the story
is told (David, 2026-09-06: "do the restart automatically … just note it
in the chat"). A wake-up that cannot be sent at all restarts at once. A
chat sitting on a permission prompt is not restarted — only the user can
answer it, and the card is already there; the note says so. After two
restarts the IDE stops and leaves a note that the errand needs the user.
The same note-in-the-chat is written when the user starts a new session
from the chat's settings, marking where the agent's memory now begins.

**An exhausted allowance stops what the IDE would start by itself.** The
credential proxy sees every refusal the account issues
(`QuotaSnapshot::exhausted`), and while one stands the IDE starts nothing
new on its own: either wake-up becomes a toast with the wake as a
button, and `issue_start` and `chat_send` are refused on the strip side
with a message telling the agent to stop and tell the user
(`Chats::allowance_exhausted`; David, 2026-09-06: "Require user
intervention to continue running if session allowances are exhausted").
The user's own prompts are not gated — typing one is the intervention.

**The coordinator's acts are cards the user can read at a glance.** Its
tool calls that *do* something — `issue_create`, `issue_start`,
`issue_update` (completed, which is merged; declined; reopened),
`issue_reorder`, `chat_send` — render in its chat as headline cards
(`chat.rs::act_kind`): an accent border, a glyph for the kind, and one
sentence written from the call's arguments and answer ("Filed i-0012 · The
composer loses…", "Started i-0012 · Claude Code · opus[1m]", "Completed
i-0007 · merged", "Declined i-0009", "Moved i-0012 to the top", "Prompted
i-0004 · …"), rewritten as the answer lands. Reads and shells keep the
plain card. `TASTE_PROBE_CHAT=acts` is that transcript, posed.

**The star is deliberate: no direct env→env channel, even mediated.**
Everything anyone integrates is first a ref in the user's checkout, so
the user's visibility is total and unpublished-work accounting on destroy
stays simple. The coordinator holds no special git authority — the extra
capability rides on its MCP socket, and its checkout is the user's, not a
privileged clone.

## Issues: a ref, not a service

**Shipped.** Issue tracking lives at `refs/taste/issues` in the main
checkout — no database, no server, nothing in the working tree. One
directory per issue: `issues/<id>/issue.md` (front-matter + markdown
body) with comments as sibling files under `comments/`. Three storage
choices are load-bearing:

- **The path is the id.** No `id:` in the front-matter, because two
  places that must agree eventually do not.
- **The start records its settings.** `started_by:` says who claimed the
  issue, and beside it `agent:` and `model:` say what the work began
  under — written by the start that wins (`issue_start_with`), from the
  chat the strip actually brought up, never from the request's wish. So
  the settings an issue was worked under travel with the issue and its
  branch in the ref, not with one machine's IDE state (David, 2026-09-06:
  "Do we at least persist those agent settings in git on a per-branch
  (read: per-issue) basis?" — now yes). What a chat is switched to later
  is the chat's; the issue keeps what it began with.
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

Five MCP tools — `issue_list`, `issue_status`, `issue_create`,
`issue_update`, `issue_link` — are served on **every** environment
socket, the primary's included, because the user's own agent files
issues too; `issue_start` and `issue_reorder` are the coordinator's
alone — one makes a world, the other rewrites the user's order. What the socket decides is not whether they exist but who the
caller is: a comment's author is the accept environment, never a
parameter, and who started an issue is the store's own identity
(`taste_git::starter_identity`, user@host), because the environment is
the issue's and no longer names anyone.

**Ordering, editing and deleting are the user's, and are deliberately not
MCP tools.** Agents create and start; the person with the queue in front
of them decides what matters next, retitles what was filed badly, and
unmakes mistakes. `issue_move`, `issue_reorder`, `issue_delete` and the
title/label half of `IssueChange` are IDE-side functions for the
environments tab, compare-and-swap like every other write on the ref.
(`issue_reorder` is the coordinator's since 2026-09-06 — its brief says
to keep the queue in the user's order and say why when it moves
something — and only the coordinator's, because a tool that lets a worker
promote its own issue above the user's is exactly the one not to serve on
a worker's socket.)

Durability rides the user's own push: the IDE's push includes
`refs/taste/issues:refs/taste/issues` when the ref exists, and is
byte-identical to the old push until it does. Sync fetches the remote's
ref into a tracking ref and fast-forwards the local one when that is
clean; when both sides moved it says so in one line and changes nothing.
That is the compare-and-swap problem across two machines, and a merge UI
is not the alpha's answer to it. Agents never push it anywhere.

**Four durable states, and only one of them is written down.** An issue
is **Queued** (filed, nobody has started it), **Started** (somebody did,
and its environment is its row), **Completed** (done, and its work is
merged) or **Declined** (it will not be done — and the record stays, which
is what separates declining from deleting). The row the user reads has
one state derived from those and the environment's runtime
(`taste_core::work`): queued, starting, working, waiting, failed, stopped,
review, completed, declined.

Only the *resolution* is on the `state:` line: `open`, `completed` or
`declined`. Started is derived from `started_by`, because that is already
where "who started it" lives, and a stored second copy is a mechanism that
can disagree with the first — the one that drifts is always the one nobody
is looking at. That also makes the format read forward:
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
coordinator write issues; worker agents — any ACP agent, any lab —
pick them up; the coordinator completes them once the work is merged, and
the user declines what is not going to happen):

- **Starting an issue is the env↔issue link, and there is nothing to
  draw twice.** The environment is named by the issue, so from the issue
  you have the environment and from the environment you have the issue
  without a lookup; `started_issues_for` is the id itself. The second
  starter's compare-and-swap fails, it re-reads, and it is told who holds
  it. Push dispatch (the coordinator's `issue_start`) and pull dispatch
  (the user's Start in the composer) are the same operation from two
  ends. One issue, one environment: follow-up work found while working an
  issue is a new issue, filed with `issue_create` and either started as its
  own world or left queued for the user — which is what "issues are how
  work outlives a conversation" already asks for.

  An earlier round had environments with generated names that *claimed*
  issues, and two panels each drawing one end of the claim. The claim was
  the model waiting to be noticed.
- **Destroying an environment hands its issue back**, with a comment
  saying why. An issue started in a world that no longer exists is not
  free for anyone else and looks, in the queue, exactly like work in
  progress — silence there is worse than either alternative. A released
  issue goes back to **Queued**, which is the same path a
  rejected review takes: rejecting is a judgment about the work, not about
  the need, so the issue is not declined for it. The need survives its
  first attempt; the comment trail says what was already tried.
- **Completing requires verified mergedness, not belief** — and the check
  is in the *tool*, not in an agent's good intentions. The branches
  checked are the issue's explicit links **and the branch of record of the
  issue's own environment** (`agents/<issue>`), so starting an issue and
  publishing unmerged work holds the close whether or not anyone called
  `issue_link`.
  It is the same `taste_git::Mergedness` the review lifecycle asks
  (`ahead == 0` against the user's current branch); otherwise the call is
  refused, naming the branch and its ahead count, and nothing is written.
  An issue with no branches behind it completes freely: not every issue
  produces code, and an environment that has never published is not
  evidence of anything. Links record the branch tip as
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
- **The user authors in the backlog**, in the file-tree flank. It was a
  section of the console's environment tab, which put a *workspace* fact
  inside the pane that is about the environment you are in, behind a tab
  you had to switch to; then a second panel under the Environments panel;
  and since 2026-09-05 it is the panel — the fleet is its started rows.

  It is **permanent**: it names where you are, and an indicator a panel
  can displace is not an indicator. Rows with an environment come first,
  then the queue in the `order` file's order, then the resolved. A row
  with an environment carries the environment's marks; a row without one
  carries **a state glyph and a title**, nothing else. Three of the glyphs
  are one checkbox at three points of its life (empty, dashed for
  "started, but not here", ticked); Declined leaves the family for a
  circle-and-slash, because it is not a checkbox outcome, and its title is
  struck through. Every row is two lines — the title, and what the work is
  doing — so a started row has room for its state line and marks, and a
  queued one says how long it has waited. Only a started row is at full
  strength: weight rather than hue, because this flank already
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

  The `+` opens an **inline composer** — title and body, in the backlog's
  own intervention panel under its list, no modal, the same convention the
  file tree's dirty-file flows follow under the file list, and the same
  slot the console's rename, destroy and reject ask in — and Edit opens a
  Save composer in that slot.
  The menu also carries what only makes sense pointed at one row's
  *environment*: Open Review, Rename and Nuke, in a section of their own
  that a row without an environment does not get at all. **Decline sits
  directly above Delete** in the menu
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

  **The composer is the chat's, and it lives in a panel** (2026-09-06;
  `composer.rs`). The header's **+** (New issue) opens it in the backlog's
  intervention slot — a panel under its list, the shape every one-shot
  flow in the files area uses under the file list — for a new issue only:
  the first line is the title, the rest
  the body, `+` attaches (a selection, the active file, a file, an image;
  drop or paste works too), the microphone dictates into the field, and
  the pill is **Create** — Ctrl+Enter creates, and the panel closes. There
  is no permanent field under the list any more (David: "Drop the
  chat-style compose panel entirely from the backlog … If I don't actually
  want the item, I'll just delete it"); Ctrl+Shift+I opens the panel and
  dictates. The composer is never repurposed for editing, because the
  issue being written may be half-typed when a row is clicked. Existing issues
  are acted on from the header's right: **Start** (a queued issue),
  **Stop** (a running environment), **Delete** (asked on the row; for a
  row with an environment, the console's destroy intervention). The row's
  menu keeps Edit, Decline and Delete, and Edit opens the same composer in
  a popover on the row with Save as its pill. Attachments are written into
  the issue's directory on the ref and read back by agents with
  `issue_attachment`. This replaced a card composer with two fields that
  opened on demand and could not attach anything.

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
- **The orchestration tools that act are execution authority** —
  `issue_start` spawns an agent that will run code in a container, and
  `chat_send` prompts one, and `issue_reorder` rewrites the user's order.
  Those are confined to the coordinator's socket — the primary's (absent
  from `tools/list` elsewhere, and refused by the arm besides); the reads are every socket's, and container creation stays
  subject to the same
  user-consent gates as today's `devcontainer_reload`: `issue_start`
  starts the clone's container from the config as cloned — the user's own,
  which their own environment already runs — and a config that has since
  drifted still needs the user, which is the case that gate is for.
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
just become healthy while the baseline runs reads as drift, so the
backlog row's light goes amber with "needs rebuild" beside the toolbar's
Rebuild, and `devcontainer_reload` asks the user to apply it. (The top
banner used to announce drift too; it no longer does — the row is the one
place, David, 2026-09-06.)

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
   gesture any more: a new conversation is a new environment, and a new
   environment is an issue started (v7: environment ids are issue ids).
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
   is what makes an integration environment possible. On the
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
   bus. The console rendered agent terminals and exec mirrors as read-only
   VTE tabs for a while; it does not any more (see above) — the transcript
   was already saying it, and the roster remains as the data half.
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
   so on the backlog pinned under the tree (which is also the one click
   back, and the switcher), keeps the active filter
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
6. ~~**Orchestrator**~~ — **shipped**, then simplified into the
   coordinator (2026-09-06). Orchestration tools on the primary's socket —
   the user's own chat's — and on no other (the `publish_branch`
   precedent, for a stronger reason: these spawn agents), with every arm
   re-checking the socket rather than trusting that the tool was listed.
   There is no designation: it was a switch in the chat's settings,
   insensitive on the primary because every unbound chat shared the
   primary's socket, and one chat per environment left no unbound chat.
   `issue_start` runs cap → issue pre-flight → create → start → prompt,
   so the cheap refusals cost no clone and a lost start leaves an idle
   chat rather than a misdirected one; per-level model config rides the
   session's own advertised
   options. The strip answers over `taste_core::orchestration`, shaped
   like the UI probe: plain data out, never a pane, and no request
   variant for answering a sub-chat's permission prompt. Proven live
   against a real Claude Code session (`taste-acp/tests/orchestrator.rs`):
   tools present on the coordinator's socket and absent from another
   environment's (the test now spawns the coordinator in the primary; the
   live run has not been repeated since the simplification), the model
   calling what is now `issue_start` off the descriptions alone,
   and the task landing in a second agent's real ACP session.
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
   container is stopped and its light is honestly grey — off, not
   failed; red is for a fault). The console's
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
    the backlog already names the selected environment and nothing below
    it should say it again. State, working-on and the review
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

11. **No environment tab at all** — **shipped, 2026-09-06.** The flat
    strip above was the right shape and one page too many. Every fact on
    the Environment tab had, by then, a surface that already showed it:
    the backlog row lights the container's state and says it in words, the
    Logs section opens the build stream as a document, the review's
    judgment belongs beside the diff it is a judgment on, and the row's own
    `⋮` menu is where an environment is acted on. So the tab is deleted and
    each fact went to the one place it is read — see the table under
    "Supervision" for where.

    Two of the moves are more than tidying. **Merge and Reject are on the
    review tab in the editor**, under the comparison line, which makes them
    unreachable until a file of the branch is open: judging before looking
    is what the review lifecycle exists to prevent, and a banner one click
    from a row was the opposite arrangement. And **the interventions moved
    under the backlog's list** — the panel of the subpanel whose rows they
    are about (later that day, `intervention.rs`: the files' flows under
    the file list, the backlog's under the backlog) — because there was
    never a case for a modal.

    The console keeps Resources, the terminals, and every off-thread pass:
    the git walks, the podman queries, the issue read, the fleet assembly.
    That is a model job. Only its drawing shrank.

Each phase lands green (`cargo test --workspace` in the devcontainer),
updates ARCHITECTURE.md for what it changed, and is independently
useful.
