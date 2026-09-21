# Taste, an opinionated IDE for Bluefin and Silverblue

Taste is all you need.

An AI-supported coding IDE: Rust, GTK4/libadwaita, Flatpak-first,
devcontainer-native, with the
[Agent Client Protocol](https://agentclientprotocol.com) as the agent
abstraction. Files on the left, editor in the center, console on the
bottom, agent chat on the right, and no other arrangement. Convention over
configuration over code: projects behave uniformly because things live in
fixed places, not because each repo scripts its own behavior.

Every environment, your own included, runs in a VM the IDE provisions for
the workspace. Nothing a project needs is layered onto the OS, and there is
no mode that runs a project's code on the host with less isolation. The
design and its non-negotiables: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
and [docs/ENVIRONMENTS.md](docs/ENVIRONMENTS.md).

![The window: a file tree with git status and the backlog at its foot, a
Rust file in the editor, the environment's console below, and an agent
chat mid-turn on the right.](docs/screenshots/hero.png)

## What it looks like

The backlog is a git ref, and every issue an agent works on is an
environment with its own checkout, container, and chat.

![The backlog: issues with status dots and activity sparklines, one waiting
for review, one queued, one declined.](docs/screenshots/backlog.png)

Watching an environment aims every pane at its checkout, read-only, with
its agent's conversation on screen.

![Watching an environment: its files, its editor tabs, and its chat, tinted
to say the panes are not at home.](docs/screenshots/watching.png)

A branch an agent published is reviewed in the file tree, file by file,
and merged or rejected from the diff.

![The review face: the branch's changed files listed in the tree, each
opening as a diff.](docs/screenshots/review.png)

![A review diff: the merge base against the branch, six commits ahead,
with Merge and Reject.](docs/screenshots/review-diff.png)

The chat's Utilization tab says what the conversation is spending and
where the subscription stands.

![The Utilization tab: context window, session tokens, cache, thinking,
cost, and the subscription's window.](docs/screenshots/utilization.png)

Narrow windows fold the panes into one strip rather than dropping any of
them, and a gadget face keeps the fleet in view at a glance.

![The consolidated rung: every pane as a tab in one strip.](docs/screenshots/consolidated.png)

![Gadget mode: the backlog's rows and nothing else.](docs/screenshots/gadget.png)

Hold F1 and every key says what it does.

![Every key's speech bubble, drawn in the window.](docs/screenshots/reveal.png)

## Build and run

The host is Bluefin or Silverblue with podman; the toolchain lives in this
repository's devcontainer. Environments run in VMs, so the host also wants
a user-session libvirt and qemu, which Bluefin DX ships and the standard
images do not.

```sh
git clone <this-repo> taste-ide && cd taste-ide
./bootstrap.sh            # build the devcontainer image on host podman, build the IDE in it, launch it on this repo
./bootstrap.sh --host     # build in that devcontainer on host podman, run the binary on the host against $PWD
./bootstrap.sh --flatpak  # the production build: a per-user Flatpak, installed and launched
```

The bootstrap's devcontainer is the one container that runs on the host's
own podman: this repository's toolchain building this repository. Every
environment the running IDE opens, your own included, goes into a VM of
the workspace's pool, which is why the host wants libvirt even for the
`--host` run.

Any cargo command runs the same way, on host podman:

```sh
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
  -v "$PWD:/workspaces/taste-ide:z" -v taste-ide-cargo:/home/dev/.cargo \
  taste-ide-devcontainer cargo test --workspace
```

One host setting is worth changing: rootless podman spends your uid's
inotify budget for every container, and the default of 128 runs out under
a fleet.

```sh
sudo tee /etc/sysctl.d/90-inotify.conf <<'EOF'
fs.inotify.max_user_instances = 1024
EOF
sudo sysctl --system
```

Credentials are the project's: open a Claude Code chat's **Settings** and
sign in under **Anthropic account**. Nothing is read from the machine.

[CLAUDE.md](CLAUDE.md) has the house rules and the headless probes;
[build-aux/flatpak/README.md](build-aux/flatpak/README.md) the packaging.

## License

GPL-3.0-or-later.
