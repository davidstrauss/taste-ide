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
builds in the console, Claude Code in the chat pane, git in the file tree, and packaging via the
header-bar Flatpak button (its one prerequisite:
`flatpak install flathub org.flatpak.Builder`). The packaged IDE then
replaces step 3.

Packaging internals (manifest, offline cargo sources):
[build-aux/flatpak/README.md](build-aux/flatpak/README.md).

## License

GPL-3.0-or-later.
