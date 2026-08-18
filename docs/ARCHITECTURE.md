# taste-ide Architecture

> In an era of AI software authoring, all that's left is taste.

taste-ide is an opinionated, AI-supported coding IDE: Rust, libadwaita-native,
Flatpak-first, devcontainer-native via rootless Podman.

## Non-negotiable opinions

1. **One window arrangement.** Files on the left, editor in the center,
   console (tabbed terminals) on the bottom, AI chat on the right. Panes can
   be resized and collapsed, never rearranged, never floated, never split
   further.
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
| Repo-level IDE config | `.taste.yaml` at the root — currently nothing needs it (state is not config and lives in `$XDG_STATE_HOME`); any future project-level setting that survives the convention-over-configuration bar goes here and nowhere else |

The ghost files in the tree are these conventions made visible: a project
missing one shows it faintly, one activation away from existing — created
from a user template (offered when any exist for that file name) or the
built-in default.

## The two modes

taste-ide is always in exactly one of two modes, derived from whether the
devcontainer is running:

- **Container mode** — the working mode. Terminals and builds run in the
  container; the workspace is writable.
- **Safe mode** — the total fallback whenever the devcontainer is absent,
  stopped, or won't start. Think of it as the project's recovery console:
  its sole purpose is defining, debugging, and entering the rootless
  devcontainer setup. In safe mode, *both the user and the AI* may write
  only the safe-mode scope: the devcontainer setup (`.devcontainer/`,
  `.devcontainer.json`) plus the workspace-ergonomics dotfiles
  (`.editorconfig`, `.gitignore`, `.gitattributes`) — configuring the
  container is work, and work deserves its comforts. Everything else is
  readable (context matters when writing config) but locked, and no
  agent-triggered process runs at all: no container, nowhere to run, and
  the host is not a fallback. The agent reconfigures the environment and
  nothing else. The persistent banner names the mode and carries the
  start/rebuild/retry action; the file tree shows locks on out-of-scope
  rows; the editor refuses out-of-scope saves.

  The mode is evaluated per operation, not baked into anything at startup.
  An agent session started in safe mode sees the workspace unlock the
  moment the devcontainer comes up — it does not need restarting.

The IDE opens in safe mode and enters container mode only on a successful
container start. A failed start drops back to safe mode with the build log
in the console — exactly the state in which the chat agent (which can read
that log and edit that config) is most useful.

## Trust model

**Neither the agent nor the project repo is trusted.** The user should not
have to audit either to keep their host safe.

What the AI is allowed, in every mode:

- read/write inside the workspace only (narrowed further in safe mode) —
  never the home directory. If a project needs broader host access, the
  path is: build the thing in the devcontainer, and the *user* deploys it.
- local git operations (stage, commit, rebase) and **read-only** remote
  operations: fetch and pull work, push does not. Push is a deliberate,
  user-only action in the file tree header.

Enforcement (mechanisms, not requests):

- **Agent sandbox** (`taste-acp::sandbox`): every agent subprocess runs
  confined — inside the agent image via podman, or under bubblewrap. tmpfs
  over `$HOME` (only the agent's own auth/config paths bound back in),
  tmpfs over `$XDG_RUNTIME_DIR` (hides the session D-Bus socket — which
  could otherwise reach the Flatpak portal and execute on the host — with
  only the IDE's MCP socket bound back in), tmpfs over `/tmp`, OS
  read-only. No bwrap → no agent launch; confinement is not best-effort.
- **The workspace is not mounted where the agent runs.** A read-only
  stand-in occupies its path (one README explaining where the project
  went), so the agent's working directory exists and paths mean the same
  thing on both sides, while the only route to the project's bytes is
  through the IDE.
- **One write check** (`taste_core::policy::write_allowed`): every agent
  write arriving as ACP `fs/write_text_file` passes the same check the
  user's own edits do — inside the workspace, never `.git`, and in safe
  mode only the safe-mode scope. Symlinks are resolved before deciding,
  because the repo can commit them. **In container mode this bounds the
  mediated path, not the agent**: `ide_exec` gives it a shell with the
  workspace writable, so treat the check as the IDE keeping its own
  writes honest, not as a wall around the agent. Safe mode is where it
  confines, because there is nothing to exec into.
- **No agent-triggered process runs on the host, and none runs at all in
  safe mode.** `ExecContext` degrades to a plain host passthrough when no
  container is running, so every agent-reachable spawn site refuses that
  case explicitly rather than inheriting it: `ide_exec` (twice — the tool
  checks the mode, `exec::Jobs::spawn` refuses a missing container target)
  and `ide_references`, whose rust-analyzer would otherwise have been
  launched on the user's machine. That it currently *fails* on a bare host
  for want of rust-analyzer is absence, not policy, and absence is not
  something to rely on.

  In safe mode the agent may therefore reconfigure the environment and
  nothing else: write the devcontainer scope, read `devcontainer_logs`,
  call `devcontainer_reload`. That is the whole point of safe mode — it is
  a recovery console, and an agent's job in it is to get the container up,
  after which everything else returns. The ACP terminal extension is
  deliberately not served, so there is no second route to a process.
- **No credentials anywhere the agent reaches**: the tmpfs home has no ssh
  keys and no credential helpers, and `taste-devcontainer::security`
  refuses any repo config that would mount some into the devcontainer. So
  authenticated push is *impossible*, not just forbidden — from the agent's
  own process and from its brokered commands alike. A push-URL rewrite adds
  a clear error for unauthenticated cases (defense-in-depth; an agent
  controls its own env, so env-based measures are never the primary
  enforcement).
- **One git policy, two renderings**: `taste_core::policy::agent_git_config`
  is the single definition. `taste-acp::sandbox` renders it into the
  `GIT_CONFIG_GLOBAL` file a sandboxed agent gets;
  `ExecContext::resolve_for_agent` passes it as `GIT_CONFIG_*` environment
  on brokered commands. It rides on the agent's *command*, so the user's
  own terminals in the same container are unaffected. It also points
  `core.hooksPath` at nothing, so an untrusted repo cannot hijack an
  agent's `git commit` with a hook of its own.
- **The repo cannot break out via its devcontainer config**
  (`taste-devcontainer::security`): `runArgs` are allowlisted (resource
  limits, env, `--userns=keep-id`, hostname, init, labels — no
  `--privileged`, no `--security-opt`, no devices, no extra volumes);
  mounts must be named volumes or binds inside the workspace. A config
  outside the allowlist refuses to start, with the reason in the banner,
  the log tab, and MCP — fixable from safe mode.
- **Supply chain**: agent adapters fetched from registries are version-
  pinned (they run adjacent to the agent's own auth material).

Accepted residual risks, stated plainly: agents need network access for
their APIs, so a hostile agent can exfiltrate *workspace contents* — the
sandbox bounds what it can read, not what it can transmit. Rootless podman
is the container boundary for repo-supplied build/lifecycle code; kernel
escapes are out of scope. The user's own terminals are the user's.

And one the mediated topology makes explicit rather than introduces:
`ide_exec` runs a shell in the devcontainer, where the workspace is
writable, so an agent can reach `.git/hooks` through a command even though
`write_allowed` refuses to write it directly. A hook planted there executes
on the user's next git invocation. This was already true of the
container-confined agent, which had the workspace bind-mounted read-write
and no `.git` masking at all; the bubblewrap path masked `.git/hooks` and
that masking is what the stand-in workspace replaces. What mediation *does*
change is that the surface is now one narrow, logged tool rather than the
agent's entire filesystem — and `core.hooksPath` masking means the agent's
own git will not run a repo's hooks either. Closing it properly needs a
`.git`-aware exec confinement that rootless podman's single-uid mapping
cannot express today.

## Process topology

This is the design decision everything else hangs on:

```
Host (Flatpak sandbox)
└── taste-ide (GTK4/libadwaita app)
    ├── taste-mcp server        (unix socket; IDE state + control tools)
    ├── agent subprocess        (e.g. claude-code-acp) — confined, sibling
    │     │                       of the IDE, NOT a child of the container
    │     │                       · no workspace mount (a stand-in instead)
    │     │                       · no project toolchain — only its adapter
    │     ├── talks ACP over stdio to taste-ide   (fs reads AND writes)
    │     └── talks MCP over the socket to taste-mcp  (search, list, exec)
    └── devcontainer supervisor
          └── podman (rootless, via flatpak-spawn --host when sandboxed)
                └── devcontainer  ← terminals, builds, AND agent commands
```

**The agent's process is a sibling of the IDE; its effects land in the
container.** Two separate claims, and keeping them separate is the whole
design.

*Sibling of the IDE:* when the devcontainer is rebuilt or reconnected,
agent processes and their sessions are untouched — that is what makes "the
AI session is never interrupted by a container reload" structurally true
rather than best-effort.

*Effects in the container:* the agent has no workspace mounted where it
runs and no toolchain beyond what its own adapter needs. Everything it does
to the project goes through the IDE — file contents over ACP
`fs/read_text_file` and `fs/write_text_file`, navigation over
`ide_list_files`/`ide_search`, and commands over `ide_exec`, which runs them
in the *project's* devcontainer via `ExecContext`. So an agent's
`cargo test` is the user's `cargo test`: same image, same cache, same
failures. There is one environment of record and the agent does not get a
private copy of it.

This is what the mediation buys, beyond tidiness:

- **One write check, and an honest account of its reach.**
  `taste_core::policy::write_allowed` decides every *mediated* write, by
  the user and the agent alike, replacing a split where mount topology
  bounded the agent and `write_allowed` bounded the user — two mechanisms
  for one rule, free to drift. It is not a confinement boundary in
  container mode: `ide_exec` runs a shell where the workspace is
  writable, so an agent can write any workspace file, `.git` included, by
  running a command. Verified, not theorised. In **safe mode** it is real
  enforcement, because there is no exec target at all.
- **The mode is no longer baked in at spawn.** Confinement used to encode
  safe-vs-container mode in the mount set, so a session started in safe
  mode stayed confined until it was restarted. Policy is now checked per
  write, so bringing the devcontainer up unlocks the workspace for the
  session already running.
- **The agent's image stops mattering.** It runs the adapter, nothing else,
  so it no longer needs to resemble the project's devcontainer — which it
  never did for any project but this one.

The escape hatch (direct SDK embedding) follows the same topology: a
sibling process, container-side effects, the same mediated interface.

## Crate layout (Cargo workspace)

| Crate | Role |
|---|---|
| `taste-core` | Shared state, event bus, workspace model, config. No GTK. |
| `taste-acp` | ACP client: agent registry, subprocess lifecycle, session model, the SDK escape hatch trait. No GTK. |
| `taste-git` | Status/stage/unstage/commit/push over libgit2. No GTK. |
| `taste-devcontainer` | devcontainer.json discovery, config-change detection, rootless-Podman lifecycle state machine. No GTK. |
| `taste-flatpak` | Flatpak manifest discovery and the build→install→launch pipeline (user-triggered only). No GTK. |
| `taste-mcp` | MCP server exposing IDE state and control tools. No GTK. |
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
  rides on status refreshes, quiet when offline), push button (user-only;
  agents cannot push), and the git-state filters.
- The filters (All / Stashed / Dirty / Staged, with live counts) are
  one-at-a-time radio toggles; the git states swap the tree for a
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

### Center: editor

- Tabbed (`AdwTabView`): one buffer per open file. Switching never discards
  anything; closing a dirty tab asks (save / discard / keep editing).
  External changes (agents, builds) reload clean buffers in place and flag —
  never clobber — dirty ones.
- `GtkSourceView` 5: syntax highlighting, line numbers, style schemes that
  follow the libadwaita dark/light preference.
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
  labeled with its context.
- The pinned **Devcontainer tab** is the environment view: the podman
  resources backing this workspace (container with status, image with size,
  the config's named volumes) with Stop / Rebuild / Nuke actions (nuke =
  container + image, from-scratch next start; volumes are caches with their
  own guarded per-row removal — never nuked implicitly), above the
  supervisor's build/startup log. Debugging a broken container build is a
  first-class, visible activity — one the chat-pane agent can follow via the
  read-only `devcontainer_resources`/`devcontainer_logs` MCP tools.

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

State machine:

```
NoConfig → ConfigDetected → Building → Starting → Running
                 ↑______________________________↓
                     ConfigChanged (pending)
```

- **Discovery**: `.devcontainer/devcontainer.json` (and the other spec'd
  locations). Parsed leniently (JSONC).
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
- **Reload without interruption**: reload = stop container → rebuild →
  start → re-point the "container context" handle terminals and exec use.
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
a pinned "Flatpak" console tab. Preflight checks turn the two common
failures — builder not installed, `cargo-sources.json` missing — into
actionable messages before a long build starts.

**The AI never triggers this pipeline.** Installing to the host is exactly
the "user deploys" line in the trust model, so the trigger is the user's
button only. Agents get read-only `flatpak_status` and `flatpak_logs` over
MCP — enough to see the failure and fix the manifest, not enough to deploy.

## MCP server (`taste-mcp`)

Serves on a unix socket in `$XDG_RUNTIME_DIR`; the socket path is injected
into every spawned agent's MCP config. Initial tool surface:

- `devcontainer_status` / `devcontainer_reload` / `devcontainer_logs` /
  `devcontainer_resources` — supervise the environment (reload is the one
  agent-triggerable lifecycle action, by design).
- `flatpak_status` / `flatpak_logs` — read-only packaging visibility.
- `ide_git_status` — per-file state + branch, as the file tree sees it.
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
  the project's devcontainer. Commands resolve through
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
- `ide_environment` — where the agent is: IDE version and uptime, workspace
  root, mode, display backend, dark/light, and the topology in words (an
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
- `ide_references` — exact workspace-wide references for a symbol, from a
  rust-analyzer the MCP server keeps alive *inside the devcontainer*
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
