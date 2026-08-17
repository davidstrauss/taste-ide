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
