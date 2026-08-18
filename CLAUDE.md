# taste-ide

Opinionated AI-supported IDE: Rust, GTK4/libadwaita, Flatpak-first,
devcontainer-native via rootless Podman, ACP client as the primary agent
abstraction. Read `docs/ARCHITECTURE.md` before structural changes — the
opinions in it (fixed pane layout, git-in-the-file-tree, IDE-mediated
agents, reload-without-restart) are design commitments, not defaults.

## Building

The host is expected to be bare (immutable Fedora + podman only). Everything
builds inside this repo's own devcontainer:

```sh
# one-time image build
podman build -t taste-ide-devcontainer .devcontainer

# any cargo command
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
  -v "$PWD:/workspaces/taste-ide:Z" -v taste-ide-cargo:/home/dev/.cargo \
  taste-ide-devcontainer cargo build --workspace
```

`cargo test --workspace` runs headless the same way. Running the GUI needs
`--env` forwarding of `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` plus the socket
mount, or a host GTK stack — or no display at all via GTK Broadway:
`gtk4-broadwayd :5` + `build-aux/headless/broadway-client.py` (echoes the
roundtrips that gate the frame clock), then run with
`GDK_BACKEND=broadway BROADWAY_DISPLAY=:5`. `TASTE_PROBE_CHECK=1` makes
the app screenshot its own panes to `/tmp/probe-*.png`, dump their
computed geometry, and quit — the headless way to *see* a UI change.

## Layout

- `crates/taste-core` — events, workspace state. No GTK below `taste-app`.
- `crates/taste-acp` — ACP client, agent registry, SDK escape hatch.
- `crates/taste-git` — status/stage/commit/push (libgit2).
- `crates/taste-devcontainer` — config discovery/hashing, podman lifecycle.
- `crates/taste-mcp` — IDE MCP server (unix socket).
- `crates/taste-app` — the libadwaita app; the only GTK-linking crate.

## Rules of the road

- GTK objects never leave the main thread; tokio-side code communicates via
  `taste_core::EventBus` only.
- Agent processes are siblings of the IDE, not children of the container;
  container reloads must never touch them.
- **The agent reaches the workspace only through the IDE.** Nothing mounts
  the project where an agent runs, with one read-only exception: the
  agent's own instructions and settings
  (`taste_core::policy::agent_context_scope`), which it loads from its
  working directory before it can ask the IDE for anything. Contents
  travel over ACP `fs/read_text_file`/`fs/write_text_file`, navigation over
  `ide_list_files`/`ide_search`, commands over `ide_exec` — which runs them
  in the project's devcontainer, the one environment of record. Giving an
  agent a workspace mount or a private toolchain undoes this; adding a
  capability means adding it to the mediated surface.
- New agent integrations go through ACP. The `EmbeddedAgent` escape hatch is
  for capabilities ACP cannot express yet — justify in the PR.
- **Neither agents nor the repo are trusted.** Agents launch only confined
  (`taste-acp::sandbox`) — never unconfined, no home access, no push, no
  runtime-dir sockets, no workspace. Agent commands never fall back to the
  host: no devcontainer means no exec. Repo-supplied devcontainer configs
  pass `taste-devcontainer::security` or refuse to start. Weakening any of
  these is a design change, not a bug fix.
- Two modes only: container mode and safe mode (devcontainer down → writes
  confined to `.devcontainer/`, and no exec target at all).
  `taste_core::policy::write_allowed` is the single source of truth for
  write checks — for the user and the agent alike; consult it, don't
  reimplement it, and don't reintroduce a second mechanism (mount topology)
  that has to agree with it.
- Adapter packages fetched from registries stay version-pinned.
- **Performance is a no-compromise requirement — snappy, always.** The GTK
  main thread never blocks: no filesystem IO, git operations, process
  spawns, or network on it (offload via `runtime().spawn_blocking` and
  apply results with `glib::spawn_future_local`). Per-keystroke work must be
  bounded or coalesced (see the markdown restyle debounce). Widget lists are
  virtualized (`ListView`) or hard-capped (transcript). Measure before
  shipping hot paths: `cargo test -p <crate> perf_ -- --ignored --nocapture`
  runs the in-repo profiling harness; for frame-level analysis run the GUI
  under sysprof or `GDK_DEBUG=frames`.
- **Convention over configuration over code.** Fixed locations for project
  behavior (see ARCHITECTURE → Conventions); add configuration only where a
  convention can't hold; never add per-project scripting of IDE behavior,
  plugins, or extension points. User-level customization is plain files in
  `~/.config/taste-ide/` (e.g. `templates/<file-name>/<variant>`).
