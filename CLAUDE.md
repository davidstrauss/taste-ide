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
  -v "$PWD:/workspaces/taste-ide:z" -v taste-ide-cargo:/home/dev/.cargo \
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
`TASTE_PROBE_VIEW` picks which face gets shot (`hero`, `fleet`,
`watching`, `review`, `review-diff`, `gadget`, `consolidated`,
`consolidated-console`, `backlog`, `backlog-composer`, `orchestrator`,
`envstrip`, `utilization`) and `TASTE_PROBE_CHAT` the transcript's
(`empty`, `top`, `busy`, `permission`, `permission-edit` — the last two
are the permission card asking about a command and about a file edit,
where the default asks the devcontainer consent question);
the fixtures behind them live beside the code they exercise, so a shot
that looks wrong is a fixture to fix, never a screenshot to retouch.
`TASTE_PROBE_WIDTH` (and `TASTE_PROBE_HEIGHT`) override the window size a
view is posed at: a responsive rung is a *band*, and a dump at one point
in it says nothing about the rest. `TASTE_PROBE_ROUNDTRIP=1` poses the
view at that width, lets the rung apply, then grows the window back to
1440x900 and shoots THAT — the only way to check that a rung gives
everything back (the window's own frame comes out blank right after an
X11 resize, so judge the round trip from the pane shots and the geometry
dump). Every probe run also prints `fit
<pane>: … ok|OFF-WINDOW` — a pane whose right edge is past the window's
is a layout that does not fit, whatever the screenshot looks like.
`TASTE_PROBE_WALK=1500-380[:20]` is that check as a *sweep*: it poses the
window at every width in the range (descending if you write it that way,
which is the gesture bugs get reported from), prints each rung's verdict
with the layout's own minimum and the thresholds in force, and exits
non-zero on any width where a pane leaves the frame — the gate for the
responsive ladder. `TASTE_MEASURE_MIN=1` prints each pane's minimum width
and attributes it down the widget tree (`TASTE_MEASURE_FLOOR` moves the
reporting cutoff), which is where a minimum that grew gets pinned on a
label that stopped ellipsizing.
`build-aux/headless/near-miss.py <probe-run.log> [pane…]` reads the
geometry dumps a probe run prints and lists every pair of column-spanning
edges that differ by a few pixels without being equal — a card inset 12
beside a bar inset 10 — naming the widgets that own them. That is the
defect this UI has been caught at most often, and it is invisible in the
source (the two numbers live in different files, or one is a theme
default) and nearly invisible in a screenshot; run it on the dump before
judging the frame, and treat a new near-miss as a regression.
Broadway clamps the display to 1024x768 (see broadway-client.py), so a
shot that must be a given size wants Xvfb and `GDK_BACKEND=x11` instead —
that is how `docs/screenshots` is made, and
`build-aux/headless/shoot.sh <view>` is that recipe as a thing you run
(1440x900, dark, optipng, straight into `docs/screenshots/<view>.png`).
The docs set is shot against a fixture repository, not a working checkout
— the file-tree pane shows the branch you are on, so a shot taken from an
agent worktree bakes that worktree's generated branch name into the frame:
`build-aux/headless/fixture-repo.sh` builds it and `WORKSPACE=` points
shoot.sh at it.

## Layout

- `crates/taste-core` — events, workspace state. No GTK below `taste-app`.
- `crates/taste-acp` — ACP client, agent registry, SDK escape hatch.
- `crates/taste-authproxy` — loopback proxy holding the Anthropic
  credential; agents get a placeholder. On by default
  (`TASTE_AUTH_PROXY=0` opts out); also serves a unix socket, which is how
  a relocated agent reaches it from inside its container.
- `crates/taste-git` — status/stage/commit/push (libgit2).
- `crates/taste-devcontainer` — config discovery/hashing, podman lifecycle,
  and the substrate: which podman (host, `podman machine`, or a remote
  connection) those containers actually run on.
- `crates/taste-mcp` — IDE MCP server (unix socket).
- `crates/taste-app` — the libadwaita app; the only GTK-linking crate.

## Rules of the road

- GTK objects never leave the main thread; tokio-side code communicates via
  `taste_core::EventBus` only.
- **The boundary is the host, not the agent.** The agent and the
  containers sit on one side; the host and `$HOME` sit on the other.
  Nothing an agent or a container runs reaches the user's home, their ssh
  keys, their credentials, or a host process. That is the line to defend,
  and the only one whose weakening is a design change.
- **Mediation is user experience, not a gate.** `fs/read_text_file` exists
  so an agent reads unsaved buffers; the IDE-applied write exists so edits
  land in the user's undo stack; `ide_exec` exists so one environment is
  of record. Real value — but do not defend any of it on security
  grounds. The agent writes the code the container runs, so agent and repo
  code are one principal, and a boundary between them means nothing. This
  is VS Code's position and it is deliberate.
- **Where the agent runs follows VS Code: beside the files.** In container
  mode that is the devcontainer, and it is where the agent actually runs
  (relocation shipped — ENVIRONMENTS → Relocation). In safe mode that is
  the IDE's baseline container, and the agent relocates into it too: the
  relocation gate asks `has_exec_target()`, not `is_container()`, because
  "is there somewhere to be" and "whose config is in force" are different
  questions. Only the rung below both — no podman, nothing to relocate
  into — spawns confined outside a container against a stand-in workspace,
  with no exec target at all; that path is permanent infrastructure, not
  legacy. An agent living in the container dies with a
  reload, so continuity comes from the persisted session id and
  `session/load`, never from the process outliving anything — which works
  only because the cwd and the home volume are identical in both
  topologies. Changing either at one spawn site and not the other silently
  loses every conversation.
- New agent integrations go through ACP. The `EmbeddedAgent` escape hatch is
  for capabilities ACP cannot express yet — justify in the PR.
- **Neither agents nor the repo are trusted.** Agents launch only confined
  (`taste-acp::sandbox`) — never unconfined, no host home access, no push,
  no runtime-dir sockets. No agent-triggered process ever falls
  back to the host, and none runs directly in safe mode.
  Any new spawn site must refuse the no-container case itself; inheriting
  `ExecContext`'s host passthrough is a hole, not a default. Repo-supplied devcontainer configs
  pass `taste-devcontainer::security` or refuse to start. Weakening any of
  these is a design change, not a bug fix.
- **Configuration authority is execution authority.** Letting an agent write
  `.devcontainer/` is not a smaller permission than letting it run commands
  — applying that config runs its lifecycle hooks, and safe mode grants
  precisely that write. So the agent authors and the USER applies:
  `devcontainer_reload` asks when the config has drifted from the running
  container, naming what will run, and denies when it cannot ask. Any future
  agent-writable path feeding something the IDE later executes needs the
  same split.
- Two modes only, and **both are containers** — they differ in *config
  authority*, not in whether anything is running. Container mode is the
  project's own `.devcontainer/`; safe mode is the IDE's in-tree baseline
  environment (`taste_devcontainer::baseline`), which runs when the
  project's config is absent, unbuilt, or broken. `taste_core::ConfigAuthority`
  is that distinction and it rides on the exec target so the two can never
  disagree. Exec exists in **both**: "no exec in safe mode" was derived
  from having no container, and that premise is gone. What is untouched is
  the principle underneath it — no agent-triggered process on the host,
  ever — so `ExecContext::has_exec_target()` is what the exec gates ask,
  and a `false` there is still refused by every one of them rather than
  falling through. Below the baseline sits one last rung: no podman, so
  nothing builds, and the agent keeps the outside-confined topology with
  no exec target at all.
  `taste_core::policy::write_allowed` is the single source of truth for
  write checks — for the user and the agent alike; consult it, don't
  reimplement it, and don't reintroduce a second mechanism that has to
  agree with it. Know its reach: it bounds writes that go THROUGH the IDE.
  In container mode `ide_exec` is a shell with the workspace writable, so
  it is not a wall around the agent there. In safe mode the baseline binds
  the checkout **read-only**, which is the mount backing the same answer up
  for the shell the baseline now has — strictly more restrictive than
  `write_allowed`, never a second opinion about what is writable.
- Adapter packages fetched from registries stay version-pinned.
- **The interface must be beautiful — and the chat pane and prompt box
  are held to the highest bar in the app.** Beauty here means libadwaita
  HIG fluency (spacing, typography, focus, motion), streaming that never
  stutters or jumps, and input behavior that anticipates (scroll
  anchoring, keyboard-first, honest busy states). UI work is not done
  until it has been *looked at*: screenshot it (probe/Broadway) and judge
  it before shipping. "Works" is the floor, not the bar.
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
