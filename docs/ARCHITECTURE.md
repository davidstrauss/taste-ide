# taste-ide Architecture

> In an era of AI software authoring, all that's left is taste.

taste-ide is an opinionated, AI-supported coding IDE: Rust, libadwaita-native,
Flatpak-first, devcontainer-native via rootless Podman.

## Compatibility posture: alpha

taste-ide is alpha. **No backwards compatibility until beta** — persisted
state (workspace state files, volumes, naming schemes, on-disk layouts)
carries a schema version, and a version mismatch means the data is
discarded and rebuilt, never migrated. The one courtesy owed is a notice:
when stale data is reset, the IDE says so once (a toast, or a warning in
the app log) rather than silently starting over. Do not write migration
shims, dual-format readers, or deprecated aliases; delete the old shape
and move on.

## Non-negotiable opinions

1. **One window arrangement.** Files on the left, editor in the center,
   console (tabbed terminals) on the bottom, AI chat on the right. Panes can
   be resized and collapsed, never rearranged, never floated, never split
   further.
1b. **The environment panel is the app's single top-level control; every
   other pane shows the selected environment's resources.** The file tree,
   the git views, the editor's tab set, the console and the chat all
   render one environment's world — the one the panel says you are in.
   Selecting there IS the context switch, and it is the only one. See
   "The environment panel is the single top-level control" under The
   panes.
2. **ACP is the primary agent abstraction.** The IDE is an
   [Agent Client Protocol](https://agentclientprotocol.com) client first.
   Claude Code, Gemini CLI, GitHub Copilot, and anything else that speaks ACP
   are interchangeable agents. A thin escape hatch exists for direct Agent SDK
   embedding, used only for capabilities ACP does not model yet.
3. **Git lives in the file tree.** There is no separate git panel. The file
   tree shows status, staging is a click on the row, sync/push live in the
   tree's header bar, and committing happens in the Staged view's bottom
   pane. Local work meets the remote by *fetch + rebase onto the remote
   tip* (the Sync tool), never by merge-pull.
4. **The IDE never reloads to change containers.** The devcontainer is a
   *supervised resource*, not the IDE's own runtime. Building, starting,
   stopping, and reconnecting to the container happen while the window, the
   editor buffers, and — critically — the AI session all stay alive.
5. **The AI can operate the IDE.** taste-ide runs an MCP server exposing IDE
   state (devcontainer status, pending config changes, reload triggers) so
   the agent in the chat pane can supervise and repair the devcontainer it is
   running against.
6. **There is no general local mode.** The devcontainer is the only real
   working mode. Everything else is **safe mode** — see below.
7. **Convention over configuration over code.** Projects behave uniformly
   because things live in fixed places. The IDE dictates conventions;
   per-project configuration exists only where a convention genuinely can't
   hold; per-project scripting of IDE behavior does not exist. No plugin or
   extension mechanism, ever — capabilities are built in-tree, curated, and
   integrated.

## Conventions

The fixed places. A project that follows these needs zero IDE-specific
configuration:

| Thing | Where it lives |
|---|---|
| Devcontainer | `.devcontainer/devcontainer.json` (spec'd fallbacks honored) |
| Flatpak manifest | `build-aux/flatpak/<app-id>.json` (or reverse-DNS `<app-id>.json` at root) |
| Offline cargo sources | `cargo-sources.json` beside the manifest |
| Editor behavior | `.editorconfig` at the root |
| Tree filtering | `.gitignore` |
| User file templates | `~/.config/taste-ide/templates/<file-name>/<variant>` — one plain file per variant; the directory listing *is* the configuration |
| An environment's branch | `agents/<env>` in the main checkout — one per environment, derived from its id (`taste_git::env_branch`), never chosen |
| The issue queue and its backlog order | `refs/taste/issues`: `issues/<id>/issue.md`, comments beside it, and one `order` file of ids |
| Repo-level IDE config | `.taste.yaml` at the root — currently nothing needs it (state is not config and lives in `$XDG_STATE_HOME`); any future project-level setting that survives the convention-over-configuration bar goes here and nowhere else |

The ghost files in the tree are these conventions made visible: a project
missing one shows it faintly, one activation away from existing — created
from a user template (offered when any exist for that file name) or the
built-in default.

## The two modes

> **Status.** The multi-environment program (`docs/ENVIRONMENTS.md`,
> approved 2026-08-31) generalizes both modes to apply **per
> environment** — every environment (the primary workspace and each
> agent/human environment) is in exactly one of the two modes, and the
> safe-mode repair loop applies to each environment's own devcontainer
> config. This section describes the shipped single-environment
> behavior, which becomes the primary environment's behavior unchanged.

taste-ide is always in exactly one of two modes. **Both are containers**;
what separates them is whose configuration built the one that is running —
`taste_core::ConfigAuthority`, recorded on the exec target so the mode and
the container can never disagree:

- **Container mode** — the working mode. The container is built from the
  project's own `.devcontainer/`. Terminals and builds run in it; the
  workspace is writable.
- **Safe mode** — the fallback whenever that config is absent, unbuilt, or
  broken. The environment runs the IDE's own **baseline** definition
  instead (`taste_devcontainer::baseline`, bundled in-tree: node for the
  adapters and the MCP bridge, git, and an inspection set, on a
  digest-pinned base). Think of it as the project's recovery console — its
  purpose is defining, debugging and entering the project's own setup — but
  it is a console with tools in it now. In safe mode, *both the user and
  the AI* may write only the safe-mode scope: the devcontainer setup
  (`.devcontainer/`, `.devcontainer.json`) plus the workspace-ergonomics
  dotfiles (`.editorconfig`, `.gitignore`, `.gitattributes`) — configuring
  the container is work, and work deserves its comforts. Everything else is
  readable (context matters when writing config) but locked. The agent
  reconfigures the environment — which is not the lesser permission it
  sounds like, since applying a config runs its lifecycle commands, so the
  *user* applies it (see Trust model). The persistent banner names the mode
  and carries the start/rebuild/retry action; the file tree shows locks on
  out-of-scope rows; the editor refuses out-of-scope saves.

  **Exec exists in safe mode**, which is the change the baseline made.
  "No exec in safe mode" was *derived* from there being no container — the
  only target would have been the host — and was never the principle
  itself. The principle is that no agent-triggered process runs on the
  user's machine, and it is untouched: `ide_exec`, rust-analyzer and agent
  terminals ask `ExecContext::has_exec_target()`, and when that is false
  they refuse rather than fall through. What the repair loop gains is real:
  an agent debugging a broken build can now run things to find out why.

  **The checkout is bound read-only in the baseline**, on both binds. That
  is the mount half of the write wall, and it exists because safe mode now
  has a shell — `taste_core::policy::write_allowed` remains the single
  source of truth for writes that go through the IDE, and the mount is
  strictly the more restrictive of the pair rather than a second opinion.
  Reads go native, which is the one mode where a read-only bind was always
  the right answer: the agent must read the repo to repair its config, and
  must write nothing but the config.

  The mode is evaluated per operation, not baked into anything at startup.
  An agent session started in safe mode sees the workspace unlock the
  moment the project's devcontainer comes up — it does not need restarting.

Below both sits one **last rung**: a substrate too broken to build even the
baseline (no podman). The environment lands in `Failed` with no exec target
at all, and the agent keeps the outside-confined topology, which works
everywhere. That rung is the fallback's fallback — kept because it is what
"no container anywhere" must resolve to, and deletable the day it is judged
unnecessary.

The ladder says *whose config* a container was built from. It deliberately
says nothing about **where that container runs**, which is a separate axis:
the user's host, a `podman machine` behind KVM, or a remote podman over an
ssh connection. Every rung works on every substrate, because the substrate's
whole output is a connection name that `taste_core::PodmanTarget` composes
into each podman invocation. See ENVIRONMENTS.md → "The substrate".

`NoConfig` is therefore no longer a dead state: a repo with no devcontainer
gets the baseline, so one environment is always usable. The IDE opens in
safe mode and enters container mode only on a successful start from the
project's config. A failed start drops back to safe mode with the build log
in the console — exactly the state in which the chat agent (which can read
that log, edit that config, and now run commands) is most useful.

The agent *process* relocates into the baseline too: the chat's relocation
gate asks `has_exec_target()`, so safe mode puts the agent beside the files
in the IDE's own container rather than outside-confined against a stand-in.
The mode itself is unchanged by that — `is_container()` still answers
"whose config is in force", which is what the write scope, the tree locks
and the agent's own aim read — and only the bottom rung, with no podman to
relocate into, keeps the outside-confined topology.

## Trust model

**The boundary is the host.** The agent and the containers are on one
side; the host, the user's `$HOME`, their ssh keys, their credentials and
their processes are on the other. That line is what the user should not
have to audit anything to keep, and weakening it is a design change rather
than a bug fix. Everything below is in service of it.

**Inside that line there is one principal, not two.** It is tempting to
treat "the agent" and "the repo's code" as separate untrusted parties and
put a boundary between them. There is no such boundary to build: the agent
*writes* `build.rs`, the tests and the devcontainer config, and can run
them. Any mechanism that confines the agent but not the code it authors is
decoration. Design accordingly — and do not justify mediation, the write
check, or the exec broker on these grounds. They earn their place on user
experience and correctness (see Process topology).

What that leaves genuinely enforced, in every mode:

- **No host home, ever.** Neither container bind-mounts the user's home.
  The devcontainer's `/home` holds only the image's own user; the agent's
  home is a named volume. The only host path either sees is the workspace.
- **No host execution.** No agent-triggered process falls back to the host
  when no container is running — every spawn site refuses that case
  explicitly rather than inheriting `ExecContext`'s passthrough, because
  "nowhere to run" must never resolve to the user's machine.
- **No container runtime inside.** Nothing in either container can start a
  container, which would be host root by another name.
- **Read-only remote git.** Fetch and pull work; push does not, because no
  credential that could push is reachable — no ssh keys, no credential
  helpers, and `taste-devcontainer::security` refuses any repo config that
  would mount some in. The push-URL rewrite
  (`taste_core::policy::agent_git_config`) is defense-in-depth and a clear
  error, never the enforcement. Push is a deliberate, user-only action in
  the file tree header — and that one action is now what carries the issue
  queue to a remote as well: the push includes
  `refs/taste/issues:refs/taste/issues` when the ref exists. Agents write
  issues through IDE tools all day and still cannot publish one anywhere;
  a queue leaves this machine because a human pressed the button that
  already meant "send my work out".
- **The repo cannot break out via its devcontainer config**
  (`taste-devcontainer::security`): `runArgs` allowlisted (no
  `--privileged`, no `--security-opt`, no devices, no extra volumes);
  mounts must be named volumes or binds inside the workspace, resolved
  through symlinks; and the build names no host path at all — the context
  is the config directory by convention, staged before use, so there is
  nothing to point elsewhere and nothing to swap after checking.
- **Configuration authority is execution authority.** Applying a
  devcontainer config runs its lifecycle commands, and `.devcontainer/` is
  the one thing writable in safe mode. So authorship is split from
  application: the agent may write it, the user applies it, and
  `devcontainer_reload` asks — naming the commands — when the config has
  drifted from the running container.
- **Supply chain**: agent adapters fetched from registries are version-
  pinned; they run next to the agent's own auth material.

And one thing that is *not* enforcement, stated plainly because it reads
like it should be:

- **One write check** (`taste_core::policy::write_allowed`): every agent
  write arriving as ACP `fs/write_text_file` passes the same check the
  user's own edits do — inside the workspace, never `.git`, and in safe
  mode only the safe-mode scope. Symlinks are resolved before deciding,
  because the repo can commit them. **In container mode this bounds the
  mediated path, not the agent**: `ide_exec` gives it a shell with the
  workspace writable, so treat the check as the IDE keeping its own
  writes honest, not as a wall around the agent. Safe mode is where it
  confines, because there is nothing to exec into.
- **Why configuration authority needed splitting.** `.devcontainer/`
  defines what runs at container start (`postCreateCommand`);
  `security.rs` validates `runArgs` and mounts but deliberately not
  commands, since a devcontainer without hooks is useless; and
  `devcontainer_reload` used to apply it on the agent say-so. Write a
  hook, call reload, execute — in the mode whose premise is that the agent
  runs nothing. Hence the split above. Reloading an *unchanged* config is
  not gated: it re-runs only what the user already accepted, and prompting
  for that trains people to click through.
- **The ACP terminal extension is served in container mode and not in safe
  mode**, which is the two-mode form of the "no third route to a process"
  rule rather than an exception to it. That rule was argued for the
  outside-confined topology, where a client-served terminal would have been
  a genuinely new way into a container the agent does not live in — and
  there it still holds, because safe mode has no exec target at all. Once
  the agent relocates it is already inside its environment's container with
  a shell (`ide_exec`) and a writable workspace, so serving terminals adds
  visibility, not authority: every command becomes a live read-only console
  tab with a Kill button, instead of a summary in a transcript. The gate is
  relocation's own, so an environment that cannot host agent processes
  advertises no terminals; creation is not separately prompted, because it
  is authority the agent already holds and a per-command dialog is one
  people learn to click through. See ENVIRONMENTS.md → "Watching an
  environment" and "Trust model deltas" for the reasoning, and
  `taste_acp::terminal` for what ACP v1 actually models.

Accepted residual risks, stated plainly: agents need network access for
their APIs, so a hostile agent can exfiltrate *workspace contents* — the
sandbox bounds what it can read, not what it can transmit. Rootless podman
is the container boundary for repo-supplied build/lifecycle code; kernel
escapes are out of scope. The user's own terminals are the user's.

And one that touches the line that does matter. An agent can plant a hook
in `.git/hooks` — through `ide_exec`, or by writing the file, and neither
the mount set nor `write_allowed` meaningfully prevents it, because the
agent and the repo code are one principal. Inside the container that is
unremarkable: it is the agent running code it wrote, which it can do
anyway. It becomes a **host-boundary crossing** the moment the user runs
git host-side and their own shell executes it.

What blunts it: `core.hooksPath` (`taste_core::policy::agent_git_config`)
points agent git at an empty path, so a repo cannot hijack the agent's
commits either; and the IDE's own git is libgit2 (`taste-git`), which does
not run hooks for the operations it performs. What would close it is a
`.git`-aware confinement that rootless podman's single-uid mapping cannot
express today. Worth knowing rather than worth pretending about.

## Many windows, one machine

Each `taste-ide <folder>` is its own process and window — the application is
`NON_UNIQUE`, deliberately, because a person works on several projects at
once. That makes every shared name on the machine a collision waiting to
happen, so there is exactly one rule and one exception.

**The rule: everything derives from the canonicalized workspace path.**
`taste_core::environment::workspace_key` is 64 bits of SHA-256 over the
folder, and it keys the container names, the volumes, the per-environment
MCP sockets, the fleet socket, the build staging directory, the clone
directory, the restore-state file, the sign-in URL bridge and the desktop
notification ids. Canonicalized because the key is an *identity*: a folder
reached through a symlink is the same folder, and two keys for one checkout
would be two sets of containers fighting over one working tree. There is no
instance id and no setting — convention over configuration, and the folder
is the convention.

Three things are shared on purpose, and each is safe for its own reason:

- **The podman machine** (`taste-ide`) — one per user by design. Every
  window's containers live inside it. Nothing stops or removes it outside an
  explicit recreate, so there is no idle-stop to race; `ensure_running` is a
  check-then-act that several windows can enter together, and it settles on
  the world (*is it running now?*) rather than on any one command's exit.
- **Images** (`taste-img-<hash>`) — content-addressed. Two projects with
  byte-identical devcontainer configs genuinely want one image, and nothing
  looks an image up by workspace. `podman rmi` without `-f` refuses while
  any container anywhere still uses it, which is the right answer whoever
  the other container belongs to.
- **The baseline staging directory** — one fixed path, because the config
  hash covers the path and a per-workspace one would give every workspace
  its own copy of an identical image. Writes go to a pid-and-thread-unique
  temporary and `rename(2)` over the target, so concurrent writers are
  indistinguishable from one.

**The exception: two windows on the SAME folder.** Keying cannot help here —
the key *is* the folder, so both windows compute the same names. No
arbitration makes two supervisors correct, so there is none: the first
window to open a folder supervises it, and a second opens for editing and
supervises nothing. It runs no reconciliation, binds no fleet socket, and
does not write the restore-state file; files, git, search and the editor
work exactly as always, which is most of the IDE. The second window says so
once, calmly, naming what still works.

The claim is an advisory `flock` on a file under the state directory, held
for the window's life (`taste_core::instance`) — the right tool for the
reason a pid file is the wrong one: the kernel drops it however the process
dies, so a crashed window never leaves a folder permanently unsupervisable.
A state directory that will not cooperate grants supervision rather than
refusing to open the folder; this coordinates cooperating windows and is not
a security boundary. The fleet socket enforces the same rule independently —
it probes before binding and refuses a path something is already answering
on, rather than unlinking a live service out from under the window that owns
it.

## Process topology

This is the design decision everything else hangs on:

```
Host (Flatpak sandbox)                      ← the boundary is HERE
└── taste-ide (GTK4/libadwaita app)
    ├── taste-mcp server        (unix socket per environment, + channels)
    ├── taste-authproxy         (loopback, + channels)
    └── environment registry    (one supervisor per environment)
          └── podman (rootless, via flatpak-spawn --host when sandboxed)
                ├── channel helper  ← `podman exec -i node`, one per env
                │     · binds /tmp/taste-ide-<env>/{mcp,auth}.sock IN HERE
                │     · muxes every connection over its own stdio to the IDE
                └── devcontainer   ← terminals, builds, AND the agent
                      └── agent subprocess (e.g. claude-code-acp)
                            · the workspace is right there
                            · talks ACP over stdio to taste-ide
                            · talks MCP, and pays for turns, over the
                              channel's in-container sockets
```

**Nothing the IDE binds is mounted into a repo-built container.** The MCP
and auth sockets used to ride in at their host paths, and on an
SELinux-enforcing host that was theatre: a `container_t` process is refused
`connectto` on a socket the unconfined IDE bound, so the file was there and
`connect(2)` returned EACCES. The direction is inverted — the container
binds, the IDE dials, and the bytes ride a `podman exec` pipe the IDE
already owns — so the container's whole view of the host is its own
checkout. See `taste_devcontainer::channel`.

**The agent runs beside the files.** This follows VS Code, which for Dev
Containers, Remote-SSH and WSL moves the extension host to where the files
are rather than brokering file access across a boundary. An agent with the
workspace in front of it needs no translation layer, no private toolchain,
and no second copy of the environment: its `cargo test` *is* the user's
`cargo test`, because it is the same container.

In **safe mode** there is no devcontainer, so there is nowhere to be
beside the files. The agent runs confined outside one, against a read-only
stand-in workspace, with no exec target at all. Two modes, two topologies,
each falling out of its own premise rather than being arranged.

> **Status: SHIPPED** (multi-environment phase 4). A chat whose
> environment has a container running spawns its agent inside it, via
> `podman exec`; a chat whose environment is down keeps the
> outside-confined topology, which is permanent infrastructure and not
> legacy. The diagram above is what runs in the first case.
>
> The move costs no conversation, because nothing addressable changes
> across it: same working directory (the checkout at its real host path,
> which is how the adapter's cwd-keyed history stays findable), same home
> volume at the same mount point, no path translation anywhere. The
> transition is a respawn bridged by `session/load` — the mechanism a
> reload already used.
>
> Relocation was gated on the auth proxy and still is: the token never
> sits beside repo-supplied build code, and inside the environment's
> network namespace the proxy is reached over that environment's channel
> rather than loopback.
>
> **The SELinux gate is lifted.** Relocation used to be refused outright
> on every enforcing host, because the agent could not dial the sockets
> the IDE bound. Inverting the direction removed the question rather than
> answering it: both endpoints are now bound by the container's own
> helper, so the only connections are container-to-container, which
> SELinux permits. Verified live on Fedora 44, `Enforcing`, against a
> container with no `label=disable` and no policy of ours.
>
> **Where it still does not happen, it is refused rather than attempted.**
> A devcontainer must carry `node`, offer a writable agent home, and
> answer through its channel — the IDE makes each service reply as itself
> (a JSON-RPC ping, the proxy's own 401) rather than settling for a
> socket that exists. It probes each container once and keeps the chat
> outside-confined when it cannot host, saying so in the transcript.
> Details in `docs/ENVIRONMENTS.md` → Relocation.

**Continuity comes from persisted state, not from the process.** The
earlier design made the agent a sibling of the IDE so a container reload
could not touch it. That turned out to be the wrong mechanism for the
right goal: what must survive is the *conversation*, and it does because
the IDE persists every chat's session id (`taste_core::state`,
one `ChatEntry` per environment) while the agent
keeps its history, and `session/load` reassembles them. An IDE restart
already kills the agent outright and the chat comes back; a container
rebuild is the same event. Covered by tests, not by hope.

**What mediation is for.** The IDE still serves the agent files, search
and commands, and it is worth being exact about why:

- `fs/read_text_file` answers from open editor buffers, so an agent reads
  the user's **unsaved** edits rather than stale disk.
- The IDE-applied write lands in the buffer the user is looking at — their
  undo stack, their tab — and never clobbers unsaved work.
- `ide_exec` names one environment of record, and carries the agent git
  policy on the command rather than on the container.
- `ide_search` / `ide_list_files` answer from what the IDE already knows.

None of that is a security boundary, and it must not be argued as one.
**The agent writes the code the container runs** — it authors `build.rs`,
the tests and the devcontainer config — so agent and repo code are one
principal and a boundary between them means nothing. Mediation is user
experience and correctness. The boundary is the host.

The escape hatch (direct SDK embedding) follows the same topology.

## Crate layout (Cargo workspace)

| Crate | Role |
|---|---|
| `taste-core` | Shared state, event bus, workspace model, config. No GTK. |
| `taste-acp` | ACP client: agent registry, subprocess lifecycle, session model, the SDK escape hatch trait. No GTK. |
| `taste-authproxy` | HTTP proxy holding the Anthropic credential so agent processes hold only a revocable placeholder. Serves loopback, plus any byte stream handed to `serve_stream` — which is how a relocated agent reaches it, over its environment's channel, from inside a container that can neither route to the IDE's loopback nor dial a socket the IDE bound. On by default; `TASTE_AUTH_PROXY=0` opts out. No GTK. |
| `taste-git` | Status/stage/unstage/commit/push over libgit2, the `refs/taste/*` substrate (compare-and-swap writes that touch neither HEAD, index nor working tree), and the issue queue that lives on one of those refs. No GTK. |
| `taste-devcontainer` | devcontainer.json discovery, config-change detection, Podman lifecycle state machine, the **environment channel** (`channel`) that carries the IDE's services into a container that may not dial out to them, and the **substrate** (`substrate`, `machine`) that decides *which* podman all of that reaches. No GTK. |
| `taste-flatpak` | Flatpak manifest discovery and the build→install→launch pipeline (user-triggered only). No GTK. |
| `taste-mcp` | MCP server exposing IDE state and control tools. No GTK. |
| `taste-fleetlink` | The `net.davidstrauss.taste.Fleet` varlink service: the fleet read model, the wire protocol, the checked-in IDL. Read-only, holds no inventory of its own. No GTK, and no dependency on any other taste crate. |
| `taste-app` | The libadwaita application. The only crate that links GTK. |

Everything below `taste-app` is UI-free and unit-testable in a plain
container. `taste-app` subscribes to `taste-core`'s event bus and renders.

### Threading model

GTK owns the main thread (GLib `MainContext`). A multi-thread tokio runtime
runs the ACP connection, MCP server, podman supervision, and file watching.
The two meet only through channels: tokio-side code emits `Event`s on an
`async-channel`; the GTK side drains it with `glib::spawn_future_local`.
No GTK object ever crosses a thread.

## The panes

### The environment panel is the single top-level control

Pinned to the bottom of the file-tree pane (`envstrip.rs`), below the
intervention panel and below anything else that pane opens, because an
indicator a transient panel can displace is not an indicator. It names the
environment the panes are aimed at, carries its state dot and a lock while
the view is read-only, tints itself when that is not home, and opens the
switcher on a click or Ctrl+Shift+E.

**Every other pane is that environment's, and holds nothing of any
other's.** This is the layout rule's companion: the arrangement never
changes, and neither does what the panes are *about*. Concretely —

| Pane | What the selection decides |
|---|---|
| File tree, git views | which checkout is walked, staged, filtered |
| Editor | which tab set is on screen (`Editor::aim_at`) |
| Console | which environment's log, shell roster, podman resources, actions |
| Chat | which conversation is on screen (`Chats::show`) |

The one thing in the flank that is **not** the selected environment's is
the Backlog panel under the Environments panel (`backlog.rs`): the issue
queue lives on one ref for the whole workspace, and an unclaimed issue
belongs to no environment at all. It is workspace-scoped on purpose and
sits where it does because the two panels are one thought and each says
one half of it: a panel row above names an environment and what it is
working on, a backlog row below names an issue and which of its four
states it is in (queued, active, completed, declined). Environments
narrate; issues have states. Drawing the env↔issue link from both ends put
the same pair of facts on screen twice, eight pixels apart, in opposite
orders — the queue's copy is now the state glyph's tooltip.

**One selection, stored once.** `window.rs`'s `aim_panes` is the only
thing that moves it; every surface that can ask (a panel row, a console
action, a notification click, a gadget row, the editor following a file)
asks *it* rather than moving anything of its own, and it pushes the new
environment into each pane. The per-pane `current`/`selected`/`aimed`
fields are mirrors written only from there — never a second opinion that
has to be kept in agreement. An environment `aim_panes` cannot resolve is
**refused**, not silently replaced by the primary: there is no fallback
environment anywhere in this design, and a switch that quietly landed
elsewhere would move every pane at once. Coming home when the watched
environment is *destroyed* is a different act, and `EnvironmentRemoved`
asks for it by name.

**Switching is O(tabs), never a rebuild.** Editor pages transfer between
the visible `AdwTabView` and an unparented one per environment, so buffers,
undo history and unsaved edits survive because the widgets are never taken
apart; only the scroll offset is recorded and restored, since an unmapped
widget may forget it. Chat panes are stack pages that are never destroyed,
so a hidden conversation keeps streaming. Nothing on this path touches the
filesystem or git on the main thread — the tree's own refresh is the
existing async machinery.

**Three surfaces are exempt, and each for the same reason: they are
monitors, not panes.** Gadget mode (which *replaces* the panes below a
breakpoint), the `taste-fleetlink` varlink service (which is external),
and the panel's own switcher all enumerate every environment on purpose.
They read the same `fleet::FleetRow`s through one projection, so no
surface grows an inventory of its own.

**A chat the user cannot see still reaches them.** `ChatBinding::attention`
(a permission request nobody has answered) lights an amber dot on that
environment's row in the switcher, and on the strip itself when the waiting
chat is in some *other* environment — deliberately "other", since the
selected environment's own prompt is already on screen. Desktop
notifications are the out-of-window half of the same fact.

### The one exception to four panes: the responsive ladder

Two `AdwBreakpoint`s, three rungs, and at each one what gives way is
**moved rather than rebuilt**:

| Width | Flank | Chat | Console | Editor |
|---|---|---|---|---|
| full | column | column | pane under the editor | column |
| ≤ `CONSOLIDATED_MAX_WIDTH_SP` (960sp) | column | tabs in the one strip | tabs in the one strip | **is** the strip |
| ≤ `GADGET_MAX_WIDTH_SP` (520sp) | **is** the window | — | — | — |

The middle rung is a window tiled beside a browser, and it **consolidates
tab sets — all of them**. The chat column and the console pane stop being
panes: their views graft onto the end of the editor's `AdwTabView`
(`Editor::graft`, `Editor::graft_pages`), so the window has exactly one
tab strip — `[file 1] … [chat] [usage] [agent] [log] [shells] [resources]
[services] [terminal…]` — and whichever tab the user is reading gets the
whole width.

The principle is the one the console follows at every width: **no nested
tab sets.** Every leaf view is a first-class tab in its region's one
strip, and at this rung there is one region. The chat's own toggle strip
hides, and the console's sections are already siblings of Services and the
terminals rather than pages of a switcher inside one tab.

Everything is reparented, never rebuilt. The console's pages are
`transfer_page`d between views, so a terminal's pty crosses the breakpoint
untouched; the chat is the same widget the column was, so switching
environments keeps working; its utilization and settings shades are lifted
out of its overlay into slots that follow the selection. A chat stopped on
the user sets `needs-attention` on its page, which is how a tab strip says
what the panel says with a mark.

Grafted pages are guests: `tabfamily` says which families the strip
carries at a rung and what order they sit in, and the editor enforces it
on every reorder and attach, so a file dragged past a pane is put back in
front of it. They are deliberately **not pinned** — a pinned `AdwTabPage`
is forced leftmost, which would put the panes in front of the user's files
— and they refuse to close, delegated to the pane that owns the tab. The
console's header rides along and shows above the content of the tabs it
describes; the section the user was reading is remembered by name, so it
survives a strip that also holds files and terminals. `AdwTabOverview`,
on both strips, is how a tab that scrolled off is found.

The flank does not move: it keeps its column, so the geometry above the
console is what it was. This rung once collapsed the flank as well, which
made the window read as a stack of full-width bands and removed the
Environments panel at exactly the width where there is least room to name
the environment.

Gadget mode replaces the panes with the two panels that were already
answering the supervision question — the Environments panel and the Backlog
— moved into `gadget::Gadget`'s slot by `FileTree::stow_panels`. It was a
bespoke card rendering `taste_fleetlink::Snapshot`, which was a second
widget tree drawing the same facts as the panel; the subscription gauge
comes along for free, being a child of the panel's own header.

**The two breakpoints are added widest-first, and that is load-bearing.**
libadwaita applies the *last* breakpoint whose condition matches, and at
400sp both match — added the other way round, the middle rung shadows
gadget mode and a window dragged into a corner keeps its panes and merely
squeezes them.

ENVIRONMENTS.md → "The responsive ladder" is the design; three things make
it not a violation of the fixed-layout rule:

- **One window, one layout.** The panes and the gadget's container are two
  children of one `GtkStack`, swapped by a breakpoint setter. The panes are
  never torn down, nothing is rearranged, and every setter the breakpoint
  applies is restored when the window grows back — as is every reparent,
  which is why `stow`/`release` and `graft`/`ungraft` are written as exact
  inverses — one function applies a rung in both directions
  (`tabfamily::strip_families` says what it should hold), so growing back
  is the same code path read the other way and cannot forget half of what
  shrinking did. `TASTE_PROBE_ROUNDTRIP=1` makes the trip and shoots what
  came back, which is how the claim is checked rather than asserted. There is no second window and no always-on-top attempt (Wayland
  grants apps no keep-above, and panes never float).
- **The stack is `hhomogeneous: false`.** A homogeneous `GtkStack` requests
  room for every child at once, which would make the window's minimum width
  the panes' minimum even while the card is showing — the window could then
  never be dragged small enough to reach the breakpoint at all.
- **520sp is unreachable by accident.** Every width GNOME's own tiling
  hands out is larger (half of the narrowest targeted display, 1280, is
  640). Gadget mode is entered by dragging a corner, never by snapping the
  IDE beside a browser. 960sp is deliberately the opposite — *above* those
  widths — because being tiled beside a browser is exactly when
  consolidating helps.

Choosing an environment from a window with no panes in it is a request for
the panes back: the panel's own row hook grows the window to a size with
four columns in it before it moves the selection, and `restore_panes` is a
no-op at every other width.

### Left: file tree = git interface

- `GtkListView` over a `GtkTreeListModel`; rows lazily expand directories.
- `.gitignore` honored via the `ignore` crate (ignored files hidden by
  default, toggleable).
- Every row carries git status (new/modified/staged/conflicted) as icon +
  color, fed by `taste-git`; in safe mode, out-of-scope rows carry a lock.
- Find-in-project lives in the pane header (case-insensitive, .gitignore-
  aware, binary-skipping); results replace the tree and click through to
  file:line. Right-click rows for file operations (new/rename/delete),
  policy-checked like every other write.
- A workspace watcher makes external edits visible: content changes reload
  clean editor buffers, structural changes rebuild the tree, `.git` changes
  refresh status — an agent's work shows up like your own.
- Rows stay uniform icon+label+badge; stage/unstage lives in the row's
  context menu (with file operations), and directories aggregate their
  children. Header bar: branch indicator, the Sync tool (upstream
  ahead/behind indicator + fetch-then-rebase-onto-remote-tip button;
  ahead/behind counts stay honest via a throttled background fetch that
  rides on status refreshes, quiet when offline; it also fetches the
  remote's issue queue into a tracking ref and fast-forwards the local one
  when that is clean, warning in one line and changing nothing when both
  sides moved), push button (user-only; agents cannot push, and this is
  the one place the issue ref goes out), and the git-state filters.
- The filters (All / Stashed / Dirty / Staged, with live counts)
  are one-at-a-time radio toggles; the git states swap the tree for a
  changed-files list whose rows open as diffs (the editor's Changes face)
  and carry selection checkboxes for bulk ops in a non-modal pane anchored
  under the list. Bulk ops are **directional**: the views sit on a
  pipeline — files are furthest from the commit in the stash, closest in
  the stage — and each view offers exactly the single-step moves out of
  it (left = away from the commit, stays put; right = toward it, the view
  follows the files). The **Staged view** is where committing happens: its
  pane is permanent (ops row + the commit composer), every staged file
  starts checked, and a partial selection grays the composer behind a
  banner — a commit takes the whole index, never a subset.
- **Conflicts are a first-class view, not a dead end.** A paused rebase
  (or any conflicted state) surfaces a Conflicts filter — auto-entered
  when conflicts appear, auto-left when the rebase ends — listing the
  conflicted files: rows open at the first conflict marker; bulk ops are
  Keep Yours / Take Remote (meaning-stable across rebase's ours/theirs
  inversion) and Mark Resolved for hand-fixed files. Continue Rebase and
  Abort Rebase sit in the header while a rebase is paused. That is the
  entire git UI, by design.
- **Review is an environment's state, not a list of branches.** Work an
  agent environment published (docs/ENVIRONMENTS.md, "Git topology:
  mediated publish" and "The review lifecycle") lands in this checkout on
  that environment's one branch of record, `agents/<env>`, and review
  happens where the environment already is — the console's fleet — because
  the environment is the unit of review. What the file tree still owns is
  the *diff*: a branch's changed files against the merge base, and the
  merge itself.

  The Inbox filter is **deleted**, not deprecated. It listed published
  branches, and publishing stopped being the thing a user judges: an
  environment has one branch, publishes to it as a checkpoint as often as
  it likes, and asks for a judgment once. A list of checkpoints is not a
  review queue.

  What survived is the half that was about *files*: `FileTree::open_review`
  takes one branch of record and its merge target, and lists the changed
  files **against the merge base**.

  A review row does **not** open the same diff the other changed lists do.
  Those diff the working tree against HEAD, which is the right answer for
  your own uncommitted work and the wrong one for someone else's branch —
  it showed the reviewer's edits instead of the agent's, and a file the
  branch added has no copy on disk to open at all. `Editor::open_review_diff`
  opens `taste_git::review_blobs` instead: the merge base's blob against
  the branch's, out of the object database, no checkout and no working
  tree. Such a tab is not a file — it is keyed outside the filesystem's
  namespace so it can sit beside your own tab for the same path, it is
  read-only and refuses saves by name, it carries the branch in its title
  the way a watched file carries its environment, and a bar over the diff
  says what is being compared. Leaving the review closes them, through the
  one `FileTree::leave_review` every exit goes through — the Close Review
  row, entering a filter, aiming the panes elsewhere, and merge or reject
  settling the environment.
  It replaces the list rather than joining the filter group — a review is a
  view of its own, not a sixth state to get out of — and it outranks the
  filters on refresh, so a status pass cannot paint the Dirty list over a
  review nobody left. The console's review band is what aims it here, and
  the band owns the judgment: merge (host-side libgit2, computed in the
  object database and refused whole if it would conflict) and reject both
  live where the branch's mergedness is.
- The ignored-files eye moved out of the filter row and up beside the
  search-ghosting toggle: both are listing choices, and the filter group
  needed the row (ROADMAP's crowded-header debt, paid).
- **The environment panel is pinned to the bottom of the pane** — below
  the intervention panel, below everything this pane can open, so the one
  thing that says which world you are in is the one thing that never gets
  displaced (`envstrip.rs`; VS Code's remote-indicator corner is the
  acknowledged precedent). **It is a persistent list, not an indicator with
  a menu behind it:** one row per `FleetRow`, always visible, the primary
  first as the return path and named "Yours". Clicking a row calls the
  window's one watching transition, exactly as a fleet row does — one
  click, no menu. The panel tints itself whenever the context is not home,
  and the row the panes are aimed at is bold, selected, and carries the
  read-only lock.
  Each row carries two signals and no more, because a row is about 180px:
  - a **traffic light** — green (up; busy or idle alike), amber (building,
    starting, a config the running container no longer matches, safe mode
    on the baseline, or a chat stopped on a question only the user can
    answer), red (failed, stopped, never configured — nothing runs here).
    The mapping is `FleetRow::light`, beside the assembly, so the panel and
    the fleet view cannot disagree about whether an environment is healthy.
  - an **activity sparkline** — five minutes of `taste_core::activity` in
    44×14px, drawn in the theme foreground at reduced alpha. Silence draws
    nothing: a flat line at zero claims a measurement, and a row that just
    appeared has no history rather than a history of nothing.

  One badge joins them, and only one: an **amber dot on a row whose chat is
  waiting for an answer**. It is not a third reading of activity but the
  opposite of it — that environment will not move again until a person
  answers — and the light cannot carry it alone, because amber also means
  rebuilding and also means baseline, steady states a whole fleet can sit
  in. With one chat per environment and only the selected one on screen, an
  unanswered question in an environment nobody is looking at has no other
  way to ask. (The unpublished-work dot is the other conditional mark, and
  it is about the checkout rather than the chat.)

  The switcher's busy spinner did NOT survive the move — it animated
  permanently in the corner of the eye and drew as a broken ring in any
  still frame — so `busy` reaches the reader through the row's tooltip, and
  the fleet view keeps the spinner where a column has room. Past six
  environments the panel grows a type-to-filter entry and starts scrolling
  inside itself rather than growing into the tree. The header holds the one
  action that is not "go somewhere" — **New Environment**, mirroring the
  fleet view's, because the way to make a world lives where the moving
  between them does. Ctrl+Shift+E focuses the panel and walks the rows;
  Enter switches. A single 1 Hz tick refreshes the fleet (pure,
  equality-guarded) and repaints the sparklines (guarded on their own
  samples), because a permanent list has no open-moment to refresh on. The
  panel renders assembled `FleetRow`s and derives nothing of its own.
- **The tree can be aimed at another environment — read, never edit.**
  Selecting it in the panel — or a console action, a notification, or the
  editor being told to open a file another environment owns, all of which
  ask the same one transition — points the tree and every git view at that
  environment's clone: its branch, its statuses, its filters, and the other
  panes with it: the editor's tab set, the console's detail, the chat. The
  panel says so — that is its whole job,
  and there is no second indicator in the header. The active *filter*
  survives the move on purpose — the Dirty view over an agent's clone is a
  live review of work in progress, which is what watching is for — while
  the search, the selections and any open panel do not, because they were
  about the other checkout. The state is never persisted (a fresh IDE
  opens on the user's own checkout).
  - Every row wears the lock, the same affordance safe mode uses, because
    it means the same thing to the user: you are looking, not editing.
    Watching's reason wins where both apply — "this is calm-1's file" is
    the more useful answer than "the devcontainer is down".
  - File operations, stage/discard/stash/commit/push and branch
    operations are **disabled, never hidden**, and every
    one of them refuses at its entry point as well, naming the
    environment. The background fetch stops too: fetching another
    environment's repository on its behalf is not watching.
  - The clone gets a workspace watcher **while, and only while, it is
    watched** (`taste_core::watcher::WatchSlot`), so the agent's edits
    reload clean buffers, restyle the tree and refresh git state — the
    same machinery as your own edits, aimed at the agent's world. Going
    back drops it rather than accumulating one watcher per environment
    ever opened.

### Center: editor

- Tabbed (`AdwTabView`): one buffer per open file. Switching never discards
  anything; closing a dirty tab asks (save / discard / keep editing).
  External changes (agents, builds) reload clean buffers in place and flag —
  never clobber — dirty ones.
- `GtkSourceView` 5: syntax highlighting, line numbers, style schemes that
  follow the libadwaita dark/light preference.
- **A file from another environment opens read-only and badged.** The tab
  title carries the environment (`main.rs · calm-1`), the buffer is not
  editable, and a save is refused by name. Such a tab lives in *that*
  environment's tab set rather than beside the user's own — switching stows
  it and switching back restores it, selection and scroll intact
  (ENVIRONMENTS.md, "Each environment owns its editor tab set"). The
  predicate is
  *whose checkout the file is in*, not *what the tree is showing*, so a
  tab opened while watching stays read-only after the user has gone home.
  The same ownership decides what bounds a **write**: an agent's mediated
  write to a file in its own clone is checked against that environment's
  checkout and mode, not against the window's workspace — the window's
  root was the wrong wall for a file the window does not own.
- `.editorconfig` (via `ec4rs`) applied per-file on load: indent style/size,
  charset, trailing-newline and trailing-whitespace policy on save.
- AI inline suggestions render as grey "ghost text" after the cursor
  (Tab accepts, Esc dismisses), sourced from the active ACP agent where the
  agent supports completion-shaped prompts, or from the escape hatch.
- **Markdown is WYSIWYG, low-distraction, in place.** `.md` files open with
  the buffer itself styled from a pulldown-cmark parse: headings scale,
  emphasis renders, code sits on a subtle wash — while markup characters
  (`#`, `**`, fences, link URLs) stay present but dimmed. Dimmed, not
  hidden: hiding markers makes the cursor jump and the file lie about
  itself. The buffer content is always plain markdown; a toggle drops to
  raw source with ordinary syntax highlighting. No web engine — repo
  content is untrusted, and raw HTML in documents renders as inert, dimmed
  text.

### Bottom: console

- `AdwTabView` of VTE terminals.
- When a devcontainer is *running*, new tabs spawn inside it
  (`podman exec -it <container> <shell>`); otherwise on the host. Each tab is
  labeled with its context. A terminal opens in the **selected
  environment** when that environment has a container of its own, and in
  the workspace's own context otherwise — a clone with no container
  resolves to the host, and a shell there would claim to be that
  environment's while showing the user's files.
- The pinned first tab is the **environment view**: the ONE environment
  the panes are aimed at, in depth (docs/ENVIRONMENTS.md, "Supervision").
  It listed every environment as a row until the file tree's panel started
  doing that permanently; two lists of the same `FleetRow`s are two things
  to keep in agreement, and the one that goes stale is whichever the user
  is not looking at, so the list here was deleted rather than kept in
  parallel. **The panel enumerates; this tab details.** It follows the
  panes through `note_watching` and chooses nothing itself — which is also
  why it has no "Open Environment": going somewhere is the panel's job, and
  the panel is the only place that does it.
  The header names the environment, carries the same traffic light the
  panel shows (one mapping, `FleetRow::light`), and states in words what a
  sidebar row has no width for: mode and container state — including
  **safe mode (baseline)**, which is a container running the IDE's own
  config rather than nothing running at all — branch, unpublished and dirty
  counts, published-branch count, disk footprint, token spend, and the one
  chat bound to it, with the busy spinner, which lives here now for exactly
  that reason. Its menu carries the lifecycle: Start/Stop/Rebuild/Nuke,
  Rename, Destroy, gated on whether a container is *running* rather than on
  whose config it is, because a baseline container is just as stoppable.
  Beneath it are that environment's build log, shell roster, podman
  resources, and the workspace issue queue. The row model is **pure data**
  (`taste-app/src/fleet.rs`), assembled from the six places those facts
  live — registry, workspace state, chats, git, podman, proxy — and
  unit-tested as such, because the panel, gadget mode and the varlink read
  model render the same rows rather than each re-deriving them.
  - Two things are never computed on a render: the per-environment git
    pass (branch, unpublished work) and the footprint (a directory walk
    plus each volume's mountpoint). Both run off-thread, cache, and
    refresh on demand — a state event must not cost a `du`.
  - Actions live in a `⋮` menu on that row: Start / Stop / Rebuild / Nuke
    (the supervisor operations, per environment), Rename, and Destroy.
    Inapplicable ones are disabled, never hidden. The primary refuses
    Destroy — it is the user's checkout, not a clone the IDE made. There
    is no "Open Environment" here: this menu belongs to the environment
    the panes are already aimed at, and going somewhere is the panel's.
  - **Destroy enumerates before it offers.** The panel under the list
    (the file tree's intervention convention, in the console) names the
    unpublished branches, the uncommitted files and the chat that works
    there *before* the destructive button becomes sensitive; the clone can
    be the only copy of an agent's unreviewed work.
  - The panel below swaps between that environment's
    build log (one buffer each, seeded from the supervisor's ring), its
    **shell roster**, and its podman resources (container, image, and its
    volumes with their own guarded removal). Debugging a broken container
    build stays a first-class, visible activity — one the chat-pane agent
    can follow via the read-only `devcontainer_resources` /
    `devcontainer_logs` MCP tools.
- **The shell roster is per environment and complete** (`taste_core::
  shells`): the user's own terminals (interactive, registered when the
  console spawns them — closing the tab is how they end, so there is no
  Kill button hijacking them), the agent's ACP terminals and `ide_exec`
  mirrors (read-only, killable where there is a process to signal), and
  the build/lifecycle stream, which is a roster row of its own mapping to
  the log view. An environment building itself is something it is running.

### Right: AI chat

- Session view over `taste-acp`: streamed agent message chunks, tool-call
  cards with expandable detail, plan display, permission prompts rendered as
  inline libadwaita banners (approve/deny), file-diff previews for
  agent-proposed edits.
- **Context attachments**: a "+" menu queues the current selection, the
  active file, any file (embedded text resource), or an image (base64) as
  prompt content blocks, shown as removable chips until sent.
- **Stop** cancels the in-flight turn (`session/cancel`); the prompt
  resolves with `Cancelled` rather than being torn down.
- **Utilization**: session-cumulative token usage (in/out/total/cached)
  from each turn's response renders in the pane footer. Account-level
  quotas (5-hour/weekly limits) are not modeled by ACP; agents announce
  those in-band. (Behind the crate's `unstable_end_turn_token_usage`
  feature until it stabilizes.)
- Agent picker (Claude Code / Gemini / Copilot / custom command) is a
  dropdown; switching agents starts a new session, never a new window.
- **One chat per environment, and the pane shows the selected one's**
  (`chats.rs`). There is no tab strip: a chat *is* an environment's
  conversation, so a strip of them was a second environment switcher
  sitting beside the real one and able to disagree with it about where
  you are. The pane is a `GtkStack` of chat panes keyed by environment,
  and the environment panel's selection chooses the visible one — see
  "The environment panel is the single top-level control" below.

  A chat carries its session, transcript, composer, model, permission mode
  and auto-approve, and `ChatPane` takes its environment at construction
  and never re-aims: there is no "give this chat an environment" row any
  more, because a chat is born in one. The invariant is enforced where it
  cannot be forgotten — `ChatEntry::environment` is required and
  `WorkspaceState::set_chat` is keyed by it, so a second write for one
  environment is an update rather than an addition (state v5; a v4 file is
  discarded rather than merged, since nothing in it says which of two
  conversations an environment keeps).

  Chats restore **lazily** and, once alive, stay alive: a remembered chat
  arms its session id and connects the first time its environment is
  selected, and selecting away never disconnects it — the pane goes on
  streaming into widgets nobody is looking at and comes back mid-sentence.
  The window addresses the **visible** chat (sign-in completion, the
  destroy-session toast, commit-message suggestions, the `chat` / `chat.*`
  ui-probe targets), and `Chats::selected()` is an `Option`, because an
  environment need not have a chat at all.

  **An environment with no chat offers to start one**, and that is the
  only way a chat is made by hand. Making another chat means making
  another environment, which is the panel's own New Environment
  (`environments.rs` — one creation path, shared with `chat_create`).
  Destroying an environment destroys its chat with it: there is nowhere
  else for a conversation to live.

  The container is still **not** started on creation (environments are
  lazy, and starting one runs its config's lifecycle commands — the user's
  call, through the existing reload gate).
- **One chat can be the orchestrator.** The same settings list carries an
  "Orchestrator" switch: the designated chat's *environment socket* serves
  the orchestration tools (`env_list`, `env_status`, `chat_create`,
  `chat_send`, `chat_status`, `chat_transcript_tail`,
  `review_list`), and no other socket lists them. One per
  workspace, reassignable, persisted as `ChatEntry::role`.

  The binding requirement is the load-bearing part: sockets tell
  *environments* apart, not chats, and the primary's is the hub every
  unbound connection shares — designating the primary's chat would serve
  execution authority to all of them. So the switch is insensitive there
  and says why, and `designate` refuses it a second time rather than
  trusting the control. (It used to clone an environment in the same
  gesture; with one chat per environment there is nothing left to clone —
  the chat is already somewhere.) Moving the role takes it off the previous
  holder *before* telling the server, and both chats respawn afterwards,
  because ACP sends the tool list once per session (the relocation
  mechanism, and `session/load` carries the conversation across it exactly
  the same way). The chat header marks the role with a quiet glyph beside
  the conversation's name — where the tab's indicator used to sit — and the
  environments tab's bound-chat column repeats it.

  A sub-chat created by the orchestrator is created in the background: it
  does not steal the selection, its permission prompts go to the *user* in
  its own environment (which lights that row in the panel), and the user
  can take it over by selecting it. The orchestrator
  has no tool for answering those prompts; `chat_status` reporting
  `awaiting-permission` is how it learns to ask the user instead. The pane
  keeps a bounded plain-text mirror of its transcript for
  `chat_transcript_tail` — forgetful at the front, and it counts what it
  forgot, so a truncated view never reads as a quiet agent.
- **The permission mode belongs to the chat, not the process.** Each chat
  re-applies its mode (default: the agent's `auto`) to every session it
  connects — fresh, restored, or respawned after a crash — through the
  session-modes state where the agent advertises one and its `mode`
  config option where it does not. The choice is persisted per chat, as
  the model and the client-side auto-approve switch are.

## ACP client (`taste-acp`)

- Wraps `agent-client-protocol` (Zed's Rust implementation).
- **Agent registry**: a small curated table of known agents — display name,
  spawn command, args, sandbox-bind paths. **Claude Code is the first-class,
  default agent**; Gemini CLI and GitHub Copilot are supported alternatives
  held to a very-good bar. No extension mechanism: new agents are added
  in-tree.
- **Session model**: `AgentSession` owns one ACP session; exposes
  `prompt()`, a stream of `SessionUpdate`s, and cancellation. Sessions
  survive devcontainer transitions because nothing in them references the
  container.
- **Every agent is aimed at one environment.** `AgentAim` is that binding
  in the shape a spawn takes it — the environment's checkout as `cwd`, that
  environment's MCP socket (and the bridge command spelled around it), and
  that environment's mode — computed together from one id so no caller can
  pair one environment's socket with another's working directory.
  `AgentClient::spawn_aimed` is the IDE's entry point. Two things follow
  from the `cwd` and are invisible at the call site: the **stand-in
  workspace is keyed by checkout**, so each environment's agent gets a stub
  carrying its own clone's `CLAUDE.md`; and **`write_allowed` is evaluated
  against that `cwd`**, so a bound chat's writes are bounded by its clone
  and its mode. The aim is not the confinement: relocation decides that
  separately and identically for both modes (`ENVIRONMENTS.md` →
  Relocation), which is exactly why a chat can move between the two
  topologies and keep its conversation.
- **Client-side services**: taste-ide implements the ACP client callbacks.
  Both filesystem directions are declared and served, because the agent has
  no workspace of its own — this *is* its filesystem, not a shortcut past
  one.
  - `fs/read_text_file` — answered from the editor's open buffers, so the
    agent reads what the user sees, unsaved edits included. Falls back to
    the disk; a read degrades to slightly-stale, never to failure.
  - `fs/write_text_file` — checked against `write_allowed`, then applied by
    the editor. A clean open file takes the edit through the user's own
    buffer, so it lands in their undo stack and their view updates. A file
    with **unsaved** edits is never clobbered: the write goes to disk and
    the watcher raises the same conflict banner an edit from a terminal or
    a container build would ("Reload takes the disk version, Save keeps
    yours"). Unlike a read, a write does not fall back on timeout — the
    editor may already have applied it, and the honest answer is an error
    the agent can retry (the request carries whole file contents, so a
    retry is idempotent).
  - Permission requests (surfaced in the chat pane) and terminal creation
    (surfaced as console tabs).

  Both handlers run with the IDE's privileges, not the agent's, so each
  enforces the workspace boundary itself rather than assuming a mount does.
- **Escape hatch**: `trait EmbeddedAgent` mirroring the session model's
  surface, for direct Agent SDK embedding when ACP lacks a capability.
  Kept deliberately thin; anything that graduates into ACP moves there.

## Devcontainer supervision (`taste-devcontainer`)

**One supervisor per environment, not one per workspace.** An
`EnvironmentRegistry` owns them: the **primary** environment (the main
checkout, always present, holding the workspace's own `ExecContext` — which
is why terminals and `ide_exec` are unchanged) plus any number of named
environments, each rooted at its own git clone under
`$XDG_STATE_HOME/taste-ide/environments/<workspace-key>/<env>/repo`. The
lifecycle mutex, drift flag, running hash, log ring and config watcher are
per-environment by construction rather than by threading an id through a
singleton. The primary is the environment with the reserved slug
`primary`, not a special case. See `docs/ENVIRONMENTS.md` for the design of
record; per-environment MCP sockets and the chat↔environment binding have
landed with it, and relocation and the fleet view are queued there.

An environment whose checkout is a **clone** gets
`ExecContext::for_cloned_environment()`, which never inherits the
self-hosting "the IDE's own container IS the environment" flag. That is
true of the primary alone — its checkout is what that container has
mounted. A clone is in safe mode until its own supervisor starts its own
container.

State machine, per environment:

```
NoConfig → ConfigDetected → Building → Starting → Running
                 ↑______________________________↓
                     ConfigChanged (pending)
```

- **Naming and labels.** Every podman-visible string is derived in
  `taste_core::environment` and nowhere else:

  | Resource | Name |
  |---|---|
  | Container | `taste-<workspace-key>-<env>` |
  | Image | `taste-img-<build-hash12>` — keyed by config content, **shared** across environments that hash the same |
  | Agent home volume | `taste-env-<workspace-key>-<env>-home` |
  | Repo-declared volume | `taste-env-<workspace-key>-<env>-cfg-<declared>` |
  | MCP socket | `<container>-mcp.sock` (one per environment) |
  | Channel endpoints | `/tmp/taste-ide-<env>/{mcp,auth}.sock` — **inside** that environment's container, bound by its channel helper, never mounted from the host |
  | Build staging dir | `<container>` |

  Containers and images carry `taste.workspace=<key>` and
  `taste.env=<slug>`; containers additionally carry `taste.config-hash`.
  **Reconciliation and resource listing enumerate by those labels, never by
  a name lookup** — a name is only what some build of the IDE happened to
  compute, while the labels are the container's own claim about what it is.

  Two hashes, deliberately: the **config hash** (config files *plus* the
  IDE's own mounts) answers "is this container stale?" and is therefore
  per-environment; the **build hash** (config files alone) keys the image,
  so N environments running identical config share one image instead of
  each holding a copy. Volumes go the other way — a repo declares a volume
  with a verbatim string, so each environment's is namespaced or two agents
  would build into one cache.

- **Old-scheme resources are removed, not adopted.** At startup the
  registry sweeps this workspace's containers and images from the
  single-environment naming scheme (`taste-<key>`, `taste-<key>-image`,
  recognised by that name *and* the absence of a `taste.env` label) and
  reports the removal once, as a toast and an app-log line. Other
  workspaces' resources, and anything claiming an environment, are left
  alone. This is the alpha compatibility posture applied to podman state.
- **Events name their environment.** `DevcontainerState`,
  `DevcontainerPendingChanges` and `DevcontainerLog` all carry an
  `EnvironmentId`. There is no untagged variant and no default: a
  subscriber compares the tag and drops what is not its environment's. The
  window's panes speak for the primary, so that is what they filter to.
- **Discovery**: `.devcontainer/devcontainer.json` (and the other spec'd
  locations), resolved against *that environment's* checkout — which is
  what lets one config serve N environments with no conditionals.
  Parsed leniently (JSONC).
- **Change detection**: content hash of the config and everything it
  references (Containerfile, compose files, build context inputs). A
  `notify` watcher re-hashes on change; a mismatch with the *running*
  container's recorded hash raises `PendingChanges`.
- **Pending changes UX**: a persistent `AdwBanner` ("Devcontainer
  configuration changed — Rebuild") that stays until acted on. The same
  state is exposed via MCP so the agent can see it and initiate the reload.
- **Lifecycle**: direct rootless-Podman drive (`podman build` / `podman run`
  with `--userns=keep-id`, bind-mounting the workspace) implementing the
  core devcontainer spec: image/Containerfile/compose, mounts, env,
  runArgs, remoteUser, lifecycle hooks (onCreate/postCreate/postStart).
  Before the hooks run, the container inherits the host's git identity
  (`user.name`/`user.email` into its global config) unless it already has
  one — a fresh container must commit as the user, not refuse with
  "Author identity unknown". Identity is not a credential: the same pair
  rides on every commit the user pushes. Agents get the same identity
  through their `GIT_CONFIG_GLOBAL` policy file, which would otherwise
  mask the global config entirely; the self-hosting bootstrap inherits it
  in `bootstrap.sh` the same only-if-absent way.
  Build output streams to the supervisor console tab and into a ring buffer
  the MCP server serves.
- **Nuke is per environment, and honest about sharing**: it removes that
  environment's container and attempts its image, but an image another
  environment's container still uses survives — podman's refusal is the
  right answer, and it is logged rather than forced.
- **Reload without interruption**: reload = stop container → rebuild →
  start → re-point *that environment's* "container context" handle its
  terminals and exec use.
  Editor buffers, git state, and agent sessions never pass through the
  container, so they cannot be interrupted by this.
- Inside Flatpak, every podman invocation goes through
  `flatpak-spawn --host` (`--talk-name=org.freedesktop.Flatpak`).

## Flatpak packaging (`taste-flatpak`)

The devcontainer is where work happens; the Flatpak is how work leaves the
machine. The IDE supervises packaging natively: a header-bar button (shown
when a manifest is discovered under `build-aux/flatpak/` or as a
reverse-DNS-named JSON at the root) runs build → install (user
installation) → launch, host-side via `org.flatpak.Builder`, streaming into
a "Flatpak" console tab. Preflight checks turn the two common
failures — builder not installed, `cargo-sources.json` missing — into
actionable messages before a long build starts.

**The AI never triggers this pipeline.** Installing to the host is exactly
the "user deploys" line in the trust model, so the trigger is the user's
button only. Agents get read-only `flatpak_status` and `flatpak_logs` over
MCP — enough to see the failure and fix the manifest, not enough to deploy.

## MCP server (`taste-mcp`)

**The channel is the identity.** One server per workspace, on **one unix
socket per environment** in `$XDG_RUNTIME_DIR`, and the environment
recorded at accept time and carried through dispatch. The wire carries no
caller identity and gains none: which socket a connection arrived on IS
which environment the caller is. That is the whole mechanism — no protocol
change, no field for a client to set or an agent to get wrong, and no
fallback environment.

A relocated agent arrives the other way, and the mechanism is the same
shape: its connections come out of its environment's channel
(`McpServer::serve_stream`), and the environment is which container the IDE
exec'd the far end into — decided before a byte is read, exactly as accept
decides it. "The socket is the identity" generalized rather than weakened;
there is still nothing on the wire to forge.

Binding follows the `EnvironmentRegistry`, which
announces environments appearing and disappearing, so a clone restored at
startup gets a socket exactly as a fresh one does and a destroyed
environment loses both its socket and its per-environment services.

Tools split in two, and the split is not arbitrary:

- **Environment-facing** — they describe a world with a checkout, a
  container and a mode, so they route on the accept environment: `ide_exec*`
  (that environment's `ExecContext`, and its **own job-handle namespace**,
  so two agents polling handle 1 collect their own builds),
  `devcontainer_*` (its supervisor, and its own config in the reload
  consent prompt), `ide_git_status` / `ide_list_files` / `ide_search` /
  `ide_write_policy` / `ide_conventions` (its checkout and its mode), and
  `ide_references` (a **rust-analyzer per environment**, spawned in that
  environment's container over that environment's checkout, respawned on
  that environment's reloads).
- **Environment-only** — `publish` and `update_from_main` route on
  the accept environment *and* are absent from the primary's tool list.
  The main checkout is what environments publish INTO; publishing it to
  itself would mean nothing, so the tools are not offered there rather
  than offered and always refusing. Calling them anyway says why.
- **IDE-facing** — they describe the IDE the user is looking at, of which
  there is one, so they do not route: `ide_open_files`, `ide_selection`,
  `ide_open_file`, `ide_screenshot`, `ide_widget_geometry`, `ide_app_log`,
  `ide_permission_log`, `flatpak_*`. Per-environment copies of the editor
  or the screenshot would be an invention.

`ide_environment` sits across the line on purpose: it names the IDE *and*
says which environment the caller is in, its checkout, and its mode.

Tool surface:

- `devcontainer_status` / `devcontainer_reload` / `devcontainer_logs` /
  `devcontainer_resources` — supervise the caller's environment (reload is
  the one agent-triggerable lifecycle action, by design).
- `flatpak_status` / `flatpak_logs` — read-only packaging visibility.
- `ide_git_status` — per-file state + branch, as the file tree sees it.
- `publish` / `update_from_main` — the mediated-git pair, on agent
  environments only. Publish fetches one branch out of the caller's clone
  into the main checkout at `agents/<env>` — the environment's one branch
  of record, derived from its id, moved by every publish — host-side,
  libgit2, no hooks; it is fast-forward only, reports divergence with the
  commit count a force would cost, and has no force of its own:
  `force: true` asks the *user* in a prompt naming the loss, and fails
  closed. `ready: true` additionally flags the environment for review and
  stops its container. Update brings the hub's branches and every
  environment's branch of record down as remote-tracking refs, moving
  nothing local.
- `ide_open_files` / `ide_selection` — what the user is looking at: open
  tabs with dirty state, and the current selection with its line range.
- `ide_open_file` — direct the user's attention to a file:line
  (workspace-confined, non-destructive).
- `ide_list_files` / `ide_search` — the agent's `ls` and `grep`. The
  workspace is not mounted where the agent runs, so the IDE enumerates and
  searches it: `.gitignore` honored, `.git` and binaries skipped, absolute
  paths so results feed straight back into `fs/read_text_file`. Caps report
  themselves — a truncated list must not read as a complete one.
- `ide_exec` / `ide_exec_output` / `ide_exec_kill` — the agent's shell, in
  its environment's devcontainer. Commands resolve through
  `ExecContext::resolve_for_agent`, so they land where the user's builds
  land and carry the agent git policy. Refused in safe mode; never run on
  the host. A command that finishes inside `timeout_seconds` returns its
  result directly; anything slower becomes a handle to poll, because a cold
  build outlives the MCP watchdog and being guillotined at 150s with no
  result is worse than being asked to poll. Output is capped at both ends —
  a compiler's first error and its final summary both survive, and the
  elision says how much it dropped.
- `ide_write_policy` — the write policy, queryable per-path. When an agent
  hits the safe-mode wall (EROFS), this tool explains the philosophy
  concisely and invites it to act accordingly: author the devcontainer
  config, diagnose with logs, reload, and the workspace unlocks.
- `ide_environment` — where the agent is: **which environment**, its
  checkout root and mode, plus IDE version and uptime, display backend,
  dark/light, and the topology in words (an
  agent's `/proc` shows only its own confinement; this tool answering IS
  the IDE's liveness proof). The same story is told twice more so no layer
  misses it: the MCP `initialize` response carries `instructions`
  introducing the environment to the model, and every agent spawn exports
  `TASTE_IDE_VERSION` / `TASTE_IDE_CONFINEMENT` (container | bwrap |
  direct) so even a bare `env` in a shell says whose process it is.
- `ide_screenshot` / `ide_widget_geometry` — the agent's eyes on the UI.
  A pane (or a named widget inside one, `chat.composer`) rendered to a PNG
  exactly as the compositor sees it, and the widget subtree's geometry *as
  computed* — allocations, margins, CSS classes, scroll offsets. Together
  they close the "unverified on screen" loop: an agent's UI change is
  checked by the agent, analytically where possible, visually otherwise.
  Served by the GTK main thread over `taste_core::ui_probe` (the MCP
  server asks, the window answers; requests are bounded so a wedged main
  thread degrades to a tool error).
- `ide_app_log` — GTK/GLib structured-log warnings (unknown CSS
  properties, missing icons, unparented widgets) plus the IDE's tracing,
  in one ring buffer. What GTK grumbles to stderr is a first-class answer
  for the agent that just changed the UI.
- `ide_permission_log` — how the IDE answered recent permission requests,
  *and why*. ACP's reply is an option id or `Cancelled`; "the user clicked
  Deny", "auto-approve had no allow option", and "the turn was stopped"
  all look identical on the wire. The log keeps them distinct so an agent
  never spends turns concluding the user is refusing work they never saw.
- `ide_references` — exact references for a symbol across the caller's
  checkout, from a rust-analyzer the MCP server keeps alive *inside that
  environment's devcontainer*
  (spawned through `ExecContext`, respawned when the container changes;
  container↔host paths translated at the boundary). Replaces
  grep-and-count for rename impact and call-site questions.

This mirrors what Claude Code gets from its editor integrations (open
editors, selection, open-file navigation) — the parity target is a
first-class Claude Code experience — while diff review deliberately stays
in ACP (tool-call diffs + permission prompts). Broader diagnostics still
await a full LSP integration; `ide_references` is the deliberate first
slice of one.

The devcontainer tools are the point: the chat agent can notice the pending
config change, read the failing build log, edit the Containerfile, and
trigger the reload — the exact "AI helps me debug the devcontainer" loop.

## Fleet service (`taste-fleetlink`)

The second socket the IDE serves, and the opposite of the first. MCP is
**per environment** because the socket an agent connects on *is* which
environment it is; the fleet service is **per workspace** because it
answers "what is this window supervising", all of it at once. One window,
one open folder, one socket:
`taste_core::environment::fleet_socket_path` — `<workspace-key>.socket`
inside a discovery directory (`taste-ide/fleet` under the runtime
directory), socket mode 0600, directory 0700, derived beside every other
podman- and socket-visible name.

- **A directory, because enumeration is the point.** N windows are open at
  once by design, so a client that wants all of them — the shell extension
  aggregating every project the user has open — reads a directory rather
  than matching a pattern over the runtime directory, where it might sweep
  up an environment's MCP socket. Every entry is one open window's fleet and
  there is nothing else in there. Keys are not meant to be reversed: dial
  each socket and ask who it is (`GetInfo` names the folder, `List` gives it
  in full as `workspaceRoot`). An entry that refuses a connection is a dead
  window's leavings.
- **Binding refuses a live service** rather than unlinking it. Two windows
  on one folder derive one path, so the old unconditional unlink let the
  second silently steal the first's socket — the first kept serving a name
  nothing pointed at, and every watching client followed the thief. Only a
  socket nobody answers on is cleared. See "Many windows, one machine".

- **varlink, not D-Bus, and that is a rule rather than a preference.**
  ENVIRONMENTS.md states it: *varlink for interfaces we design; the
  established contract — D-Bus included — when implementing someone
  else's.* This interface is ours, so it is varlink. The GNOME search
  provider, when it lands, is `org.gnome.Shell.SearchProvider2` over D-Bus,
  because that one is GNOME's.
- **Hand-rolled protocol**, the same call `taste-mcp` made for JSON-RPC:
  NUL-terminated JSON over a unix stream, ~150 lines, fully specified at
  varlink.org. The `varlink` crate's model is a synchronous `std::io`
  server plus a build-time code generator — a second concurrency style and
  a codegen step in a workspace that has neither.
- **The IDL is checked in** at
  `crates/taste-fleetlink/src/net.davidstrauss.taste.Fleet.varlink` and
  served verbatim over `org.varlink.service.GetInterfaceDescription`. A
  test parses it and compares its fields against what serde actually
  emits, so the description and the wire cannot drift.
- `List()` returns the fleet once; `Watch()` streams it, using varlink's
  `more` flag, driven off the same tagged events that redraw the console.
  Both return the same shape. Updates coalesce (`tokio::sync::watch`): a
  slow client sees the latest fleet, never a backlog.
- **Read-only by design, not by omission.** No method mutates anything. A
  process that can open a socket in the user's runtime directory is not
  thereby entitled to start containers or answer permission prompts; a
  control interface, if ever wanted, arrives under its own name with its
  own argument about authority.

## Desktop notifications

`notify.rs` holds the whole policy as pure logic — `decide(Moment,
Attention, scope) -> Option<Notice>` — and the gio calls are three lines
each. `scope` is the workspace key, and it is in every id because gio ids
are per *application* while a taste-ide window is not one: chat keys are
process-local ordinals, so without it two windows' first chats are both
`chat-1` and one window's "needs permission" replaces the other's in the
shell. `notification_id` is the only place an id is spelled, so the sender
and the withdrawer cannot drift.
**One rule: never notify about the surface the user is already looking
at**, where "looking at" means the window has focus *and* that surface is
on screen. A permission prompt in a chat whose environment is not
selected notifies even with the window focused — and lights that
environment's row in the panel, which is the in-window half of the same
fact.

Coalescing is the notification id, scoped per chat and per environment
(`taste-permission-chat-3`, `taste-build-calm-1`, `taste-review-wry-4`):
two chats each needing the user are two facts, one chat asking twice is
one, and two environments asking for review are two. `Digest` supplies the
other half of quiet — the first sighting of anything is a baseline, so an
IDE opened onto an already-failed environment, or onto a fleet that was
already waiting when it last quit, comes up silent. Clicking a
notification activates the application-scoped `app.surface` action, whose
target names a chat, an environment, or an environment under review; the
window grows back to a size with panes in it and lands there.

## Flatpak

- App id `net.davidstrauss.Taste`; GNOME runtime + `rust-stable` SDK extension.
- Cargo dependencies vendored offline (`cargo --offline` +
  `cargo-sources.json` generated by `flatpak-cargo-generator`).
- Sandbox: `--talk-name=org.freedesktop.Flatpak` (host podman),
  `--filesystem=home` initially (project directories; to be narrowed to the
  portal once the file tree drives selection).
- Agent subprocesses also launch via `flatpak-spawn --host` so they are real
  host processes with access to podman and user credentials.

## Self-hosting

taste-ide develops taste-ide. This repository carries its own
`.devcontainer/` (Fedora + Rust + the GTK stack + node for agents); the IDE
opened on this repo supervises that container, builds itself inside it, and
the chat agent fixes the container when it breaks.

**Bootstrap semantics** (the IDE running *inside* a container, before the
packaged IDE exists): detected via `/run/.containerenv`. The container the
IDE runs in **is** the environment — it is the devcontainer image, running
the same tools a supervised container would. `Running (self)`; lifecycle
operations (build, reload, stop, nuke) are refused with a pointer to the
host IDE, because a container cannot rebuild itself.

**No container runtime is reachable from in there, and that is a design
commitment, not a missing feature.** Forwarding the host's podman socket
would make the devcontainer a supervisable *sibling* — and would also hand
every process inside the container the ability to start host containers
with arbitrary mounts, which is host root by another name. That reach would
extend to the agent (which spawns without bwrap here — it cannot nest in a
rootless container) *and* to the repo's own build and test runs, both of
which are untrusted by the rules above. Exercise real devcontainer
supervision from a host-side IDE instead: `./bootstrap.sh --host` or
`--flatpak`. As belt and braces, an IDE that finds itself inside a
container strips `CONTAINER_HOST`/`DOCKER_HOST`/`CONTAINER_CONNECTION` from
its own environment at startup, so no child inherits a runtime handle that
leaked in from the launch environment.

Agents spawn without bwrap because the container already *is* the
confinement: the user's real home is not mounted. This is the one topology
where the agent and the IDE share a process space and the workspace really
is present for both — a container cannot hand the agent a stand-in for a
workspace it is itself running inside. Mediation still applies to
everything the IDE serves (writes are checked, `ide_exec` still resolves
through `ExecContext`), but self-hosting is a bootstrap convenience, not
the confinement story; `--host` or `--flatpak` is. Sign-in OAuth works via
the URL bridge (`$BROWSER` → drop dir → IDE confirmation dialog with
Open/Copy) and `--network=host`, with credentials persisted in the
`taste-ide-home` volume.

## Testing posture

- `taste-core`, `taste-git`, `taste-devcontainer`, `taste-acp` carry unit
  tests runnable headless in the devcontainer (`cargo test --workspace`).
- `taste-devcontainer` gets integration tests against real rootless podman
  (gated behind `--ignored` so CI without podman still passes).
- UI logic stays thin enough that pane view-models are testable without a
  display; GTK snapshot testing is deferred.
