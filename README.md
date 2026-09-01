# Taste, an opinionated IDE for Silverblue enthusiasts

Taste is all you need.

An opinionated, AI-supported coding IDE: Rust, GTK4/libadwaita, Flatpak-first,
devcontainer-native via rootless Podman, with the
[Agent Client Protocol](https://agentclientprotocol.com) as the primary agent
abstraction. Files on the left, editor in the center, console on the bottom,
AI chat on the right — and no other arrangement. Convention over
configuration over code: projects behave uniformly because things live in
fixed places, not because each repo scripts its own behavior.

The design and its non-negotiables: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

![The taste-ide window, dark: a file tree on the left showing the taste-ide
repository with git status markers and Dirty/Staged/Inbox filters, an
environment panel reading "Yours" pinned at its foot, a Rust source file open
in the editor with a minimap, the console below showing that environment's
detail and its live shell roster, and an agent chat on the right mid-turn —
streamed prose, a shell tool card, a diff card, and a permission prompt
asking to rebuild an environment.](docs/screenshots/hero.png)

## What it looks like

A workspace is a fleet, not a session: any number of named environments,
each a git clone with its own devcontainer, one chat, disk footprint and
token spend. The panel pinned under the file tree is where you move between
them — and it is the app's only top-level control, because every other pane
shows the selected environment's world.

![The environment switcher, opened from the panel under the file tree: rows
for "Yours" (running, ticked as current), brisk-3 (building), calm-1
(running, with a busy spinner and an unpublished-work dot) and wry-4
(stopped), over a "New environment"
row.](docs/screenshots/envstrip.png)

Select an environment and every pane becomes its: its files, its git state,
its editor tabs, its console, its chat. Non-primary environments are
read-only to you — watch the agent work without racing it. The panel names
where you are and tints itself while you are away from your own checkout;
every tree row carries a lock, and files open as read-only tabs badged with
the environment's name.

![The whole window aimed at one environment: every tree row padlocked, the
environment panel at its foot tinted purple and reading "calm-1" with a
lock, the editor tab labelled "filetree.rs · calm-1", the console showing
"Environment calm-1" and its agent terminal, and the chat headed "Claude
Code · calm-1".](docs/screenshots/watching.png)

Agents publish branches; they never push. Published work lands in a review
inbox beside Dirty and Staged, and opens as a diff.

![The file tree's Inbox filter listing three published agent branches with
ahead/behind counts and ages, and the editor showing the added lines of one
of them highlighted in green.](docs/screenshots/inbox.png)

Issues are a git ref in your own checkout, so every environment can read
them and they ride along on your push.

![The console's Issues page: a queue of three issues — one claimed by
calm-1, one unclaimed, one closed — above the selected issue's detail
showing its linked agent branch and
body.](docs/screenshots/issues.png)

Shrink the window past the breakpoint and the panes give way to one fleet
card. Same window, same data, monitor-sized.

![A narrow window showing the gadget card: "3 of 5 environments up, 2 chats
working", a subscription spend figure, five environment rows with progress
bars, "Review inbox — 2 branches waiting for review" and "Issues — 2
open".](docs/screenshots/gadget.png)

## From stock Silverblue to self-hosting

Runs on an unmodified Fedora Silverblue: podman is already in the base
image, and nothing is ever installed on the host.

```sh
git clone <this-repo> taste-ide && cd taste-ide
./bootstrap.sh
```

The script builds the project's devcontainer image, builds the IDE inside
it, and launches it against this repository — Wayland and GPU forwarded,
agent sign-ins persisted across runs. It is idempotent; run it again any
time (cached layers make subsequent runs go straight to launch). Every flag
is explained inline in [bootstrap.sh](bootstrap.sh).

That's the whole bootstrap. taste-ide opens this repository, recognizes it
is running *inside* its own devcontainer (full container mode — no safe-mode
locks), and from here on the work happens inside the IDE: terminals and
builds in the console, Claude Code in the chat pane, git in the file tree.

## Fast host runs

The quickest way to run Taste *on the host* (real portals, working
devcontainer supervision) without building a Flatpak:

```sh
./bootstrap.sh --host
```

It builds inside the devcontainer as usual, then runs the resulting
binary directly on the host — libgit2 is vendored into the binary and
everything else it links (GTK4, libadwaita, gtksourceview5, vte4) is
already in the Silverblue base. Agents don't need node on the host:
they launch confined inside the devcontainer image via podman.

Running the binary by hand works too — the flag just wraps:

```sh
./target/debug/taste-ide /path/to/some/project
```

## The production build

The self-hosting run lives inside the devcontainer, so it cannot itself
supervise devcontainers (podman does not nest). For real work on other
projects, run Taste as a proper Flatpak on the host:

```sh
./bootstrap.sh --flatpak
```

This builds the release Flatpak with `org.flatpak.Builder` (installed
per-user from Flathub, along with the GNOME runtime — a one-time,
multi-gigabyte download), installs it per-user, and launches it. Nothing
is installed on the host OS itself. The packaged app gets real portals
(browser links, dark-mode tracking) and full devcontainer supervision via
`flatpak-spawn` to host podman. Afterwards it lives in your app grid as
"Taste"; the in-IDE header-bar Flatpak button rebuilds and redeploys it
from inside the self-hosting run.

Packaging internals (manifest, offline cargo sources):
[build-aux/flatpak/README.md](build-aux/flatpak/README.md).

## License

GPL-3.0-or-later.
