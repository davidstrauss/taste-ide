# The host side of the sandbox

2026-11-14. A second audit against the standard in ENVIRONMENTS →
"Isolation: the standard, and what meets it", five weeks after
[sandbox-tightening.md](sandbox-tightening.md). That one found the line
crossed from inside a VM, by a config or a mount. This one finds it
crossed from the host's side: the programs the IDE runs here to reach a
VM — podman, git, and ssh — carry things of the host's into it, or act
with the user's credentials on what the VM names. Nothing here is fixed
yet. Part one is what was found, each with how it is known. Part two is
what David decided the same day, and what is still his to decide. Part
three is the work, in batches an environment can take one at a time.

Read off the code, and measured where a claim rests on a tool's behaviour
rather than ours: on the development host (Fedora 45, libvirt 12.6.0, qemu
11.1.3, passt `0^20261002.gcba3570`, podman 6.1.3, OpenSSH 10.5p1, SELinux
enforcing), with one VM of another workspace running. No VM was entered:
what is said of a guest is the code's word.

## Part one: what was found

### 1. A config's `runArgs` can copy this host's environment into its container

`security::validate_run_args` passes `-e NAME`, `--env NAME`,
`--env=NAME`, and `--env=*`: the value after `-e` or `--env` is refused
only when it starts with a dash, and `--env=` is a prefix that takes
anything. Podman fills a variable given without a value from its own
environment, and a name ending in `*` with every variable that starts
with it. Over a connection it is the CLIENT that does this, on this host,
in the environment the IDE gave it, which is the IDE's own
(`Substrate::command`; nothing but one test calls `env_clear`).

Measured, with a service started in an empty environment and a client
holding two variables the service never saw:

```sh
env -i HOME="$HOME" PATH="$PATH" XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  podman system service --time=30 "unix://$XDG_RUNTIME_DIR/t.sock" &
export X_SECRET=client-only X_OTHER=second
podman --url "unix://$XDG_RUNTIME_DIR/t.sock" create --name a -e X_SECRET IMAGE true
podman --url "unix://$XDG_RUNTIME_DIR/t.sock" create --name b '--env=X_*' IMAGE true
podman inspect a b --format '{{range .Config.Env}}{{println .}}{{end}}' | grep X_
```

`a` held `X_SECRET=client-only`, and `b` held both. So `"runArgs":
["--env=*"]` in a cloned repository's `devcontainer.json` puts every
variable of the user's session into the project's container at its first
start, and an agent can write that line and apply it itself
(`devcontainer_reload` asks nobody). CLAUDE.md already has the rule this
breaks — "any agent-writable path that feeds something executed OUTSIDE
the VM needs the author/apply split back" — and podman's client is
executed outside the VM.

What it carries is whatever the session holds. The IDE running on the
development host had 64 variables, none with a secret's name. On a
machine whose shell exports a `GITHUB_TOKEN` it carries that, and it
carries the Anthropic credential itself, as bytes, where the IDE was
started with `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` set:
`credentials::discover` takes either ahead of the project's own file.

Not passed by default, measured the same way: the client's proxy
variables (`http_proxy`, `HTTPS_PROXY`).

### 2. The host's registry logins go to the VM on every pull and build

Podman's client sends the service the credentials of the machine it runs
on. Measured with a file of two made-up registries (`REGISTRY_AUTH_FILE`)
and a listener that answered as the service and wrote down the headers:
`podman pull dummy-one.invalid/x/y:z` sent both entries in
`X-Registry-Auth`, and `podman build` sent both in `X-Registry-Config` —
every login, whichever registry the image is on.

So each `pull` and `build` the supervisor runs through a connection hands
the VM's podman whatever `podman login` or `docker login` left on this
host (`$XDG_RUNTIME_DIR/containers/auth.json`,
`~/.config/containers/auth.json`, `~/.docker/config.json`). A container
does not see those headers; anything that is `core` in the guest can (the
service's socket is `core`'s to replace), and a `ghcr.io` login is a
GitHub token. The development host has none of those files, so it is not
exposed today. It is also the house rule turned over: a machine-wide
credential imported into every project.

### 3. Signing in mints the year-long token inside the project's container

"Get a token" runs `claude setup-token` in a console tab
(`ChatPane::open_token_terminal`), through `taste_acp::login_command` with
the pane's relocation — `relocate::relocated_login_command`, a `podman
exec -it` into the environment's own container, as the config's user,
with the agent's home volume as `HOME` and the checkout as the working
directory. When it ends, the IDE takes the token off the tab's last rows
(`find_setup_token`) and writes `anthropic.json` on this host.

After that the design holds: the proxy has the credential and a container
has a placeholder. But the credential is BORN on the other side of the
line, in a container where the project's hooks have already run, under
the uid they ran as. Anything there can take it — a process left waiting
for it, or a committed `.npmrc` that sends `npx` to another registry for
the CLI (the capture itself was not measured; that the sign-in runs there
is read off the code). `Console::find_in_scrollback` also reads every tab
that holds a terminal, so `ide_find` from any socket reads the Sign In tab
for as long as it stands. This is clause 1, for the minute a sign-in
takes, and the token it yields is good for a year.

The other agents' own credentials (Gemini CLI, Copilot) live in the home
volume for good, which ENVIRONMENTS already says.

### 4. The host's git runs hooks the VM wrote

No git the IDE runs on this host sets `core.hooksPath`; the only
overrides are in scripts run inside a VM (`keeper.rs`, `disposable.rs`,
`registry::QUIET_GIT`). The folder's `.git/hooks` is safe — the mirror
refuses `.git` at any depth (`beneath::checked`) — but a folder whose own
config points hooks into the tree is not, because the mirror writes those
files from the VM with their executable bit.

- `core.hooksPath=.githooks`, the directory itself tracked: an agent adds
  `.githooks/reference-transaction`, and the sync's own fetch from the VM
  (`peer::fetch_from_guest`, run in the folder) executes it. No click.
  `pre-auto-gc` runs the same way when a fetch decides to collect.
- husky (`core.hooksPath=.husky/_`, set by an `npm install` once run in
  the folder): `.husky/pre-push` and `.husky/post-merge` are tracked, and
  the ignored stubs beside them call them. One Push or Pull.

The config has to have been set on this host, by the user or a setup
script, before or beside the IDE. The IDE cannot know, and one flag
closes it.

### 5. Set Up clones what `.gitmodules` names, with the user's keys

`submodule_setup.rs` (2026-10-13, three days after the last audit
recorded that the host runs no `git submodule update` on VM data) runs
`git submodule update --init --recursive -- <path>` in the folder, with
the user's keys loaded for the URL's host (`ssh_agent::network_env`). The
URL is `.gitmodules`', which the mirror carries from the VM; the banner
never shows it (`submodule_bar.rs` does not read it); `--recursive` takes
each further URL from what was just fetched; and no protocol is
restricted beyond git's defaults. The sync then gives the VM what was
cloned, so a private repository the user can read, named by an agent, is
fetched with the user's keys and handed in. A URL on a host of the
attacker's is offered the user's public keys.

Beside it: `presence::ssh_target` hands a host taken from such a URL to
`ssh -G` with no `--`, so a URL whose authority starts with a dash is read
as an option. `-G` runs nothing; the dash should still not get there.

### 6. An agent can run on this host's kernel

Two ways, both by design, and both below VM isolation.

**The bwrap rung** (`sandbox::wrap`), reached when Personal has settled
with no container and the machine has `npx` (`chat::container_gate`): the
whole of `/` bound read-only with the agent's own home over the user's,
this host's network, no seccomp, and no namespace but pid. Two things in
it read the user's files:

- `sandbox::agent_context_binds` keeps a path when `path.exists()`, which
  follows a link, and bwrap binds what the link names. A repository that
  commits `.claude -> ../../.ssh` has that directory bound in.
- `McpServer::checkout_file` checks a path's spelling, and while the
  checkout is `Files::Local` the read is `std::fs::read`: `ide_read_file`
  reads through any link in the folder. (`fs/read_text_file`,
  `ide_list_files`, and the writes all resolve links first.) A `.pdf` is
  handed to this host's `sh` and poppler the same way.

A bare host has no node, so there the rung never runs.

**"Inside a container"** is `/run/.containerenv` or `/.dockerenv`
existing (`ExecContext::host`, `sandbox::inside_container`). The
bootstrap's container has neither the user's home nor a runtime socket,
which is what the direct spawn and `has_exec_target()` rely on. A toolbox
or a distrobox has the same file and the user's real home.

### 7. The transport reads the user's ssh config, and podman's client pins nothing

`Keys::ssh_args`, `tunnel_argv`, and `git_ssh_command` pass the identity,
`IdentitiesOnly`, `IdentityAgent=none`, the workspace's `known_hosts`,
and `StrictHostKeyChecking=yes` — and no `-F`. So `~/.ssh/config` applies
to every connection into a VM: a `Host *` or `Host 127.0.0.1` stanza with
`ForwardX11`, `ProxyCommand`, or an explicit `ForwardAgent` socket is in
force toward an untrusted guest.

Podman does not use that ssh at all. `podman system connection add
--identity …` leaves it its own client, which trusts a host key the first
time it sees one and writes it to the user's file. Measured:
`~/.ssh/known_hosts` on the development host holds 39 `[127.0.0.1]:PORT`
lines, eight of the nine registered connections' ports among them, every
one `ecdsa-sha2-nistp256` — a key the guest's sshd made for itself — where
the IDE mints and pins `ssh-ed25519`. Everything that rides podman (exec,
the keeper, the agent's stdio) is authenticated by a key the IDE never
saw, and each VM leaves a line in a file that is the user's.

### 8. Egress is still the guest's to enforce

October's first item, unchanged, with two measurements. The running
passt's whole argv is `passt --one-off --socket … --pid … --tcp-ports
127.0.0.1/42165:22`: no `--no-map-gw`, so the guest's gateway address is
this host's loopback. And `provision::PRIVATE_NETWORKS` is still four
ranges: `100.64.0.0/10` (every Tailscale address) is not in it, a LAN
device's global IPv6 address is in neither family's list, and port 53 is
let through to any address. What keeps a guest from all of it is a table
`root` in the guest removes, and section "Between environments" below is
how an environment becomes that.

### 9. Smaller

- **The proxy forwards any path.** `proxy::upstream_uri` keeps the
  client's path and query, and `handle` checks neither path nor method,
  so a placeholder reads the account's files and batches where the
  credential is an API key. `Handle::revoke` has no caller outside the
  tests: a placeholder outlives its environment.
- **A cloud host's `user-data` is never cleared.** `hosts.rs` sets the
  whole Ignition there, the host's private key in it, and nothing takes
  it back (October's item 6). The role already holds
  `compute.instances.setMetadata`.
- **The guest has nested virtualisation and a USB controller.** `<cpu
  mode='host-passthrough'>` with nothing disabled, on a host whose KVM
  has `nested=Y`; and libvirt adds a `qemu-xhci` nobody asked for. Both
  are host code a guest can reach for no purpose of ours.
- **A config's labels override the IDE's.** `runArgs` come after
  `resource_labels()` in the run's argv, `--label` is allowed with any
  key, and `Supervisor::adopt_running_container` finds its container by
  `taste.workspace` and `taste.env` alone, taking its authority, its
  config hash, and its ports from labels as well. Compose strips
  `taste.*`; a single container does not.
- **The port browser's passkeys take the page's word for its origin**
  (`passkey_menu.rs`: `origin: location.origin`, posted by the page and
  read back from its message), and the HTML preview and the archive
  viewer have no download handler where the port tab cancels every one.
- **The fetch from a VM follows tags and checks nothing.** No `--no-tags`
  beside refspecs that have destinations, so a tag the VM makes on
  fetched history lands in the folder's `refs/tags/` as well as in
  `refs/taste/vm-tags/` (measured with the sync's own refspecs, git
  2.56.0); and no `fetch.fsckObjects` (October's item 7).
- **The userns probe still runs the project's image with no limits**
  (October's item 9).
- **The guest image is checked by digest alone**, against a stream
  document fetched over TLS; the stream's signature is not checked.

### Between environments: one trust domain to a VM

Recorded because finding 8 leans on it, not to be closed (Part two).

- `peer::push_to_guest` is `git push ssh://core@…/<checkout>`, so
  `receive-pack` runs as `core`, outside every container, in a repository
  whose `.git/config` and hooks its environment writes; an agent's
  `git_fetch` ends in such a push. `core` has passwordless sudo, which
  `ports.rs`, `provision.rs`, and `golden.rs` use.
- The keeper mounts every checkout of the workspace with every MCS
  category. Its `gitSafe` reads `.git/config` for `[filter]` and `[diff]`
  sections, which `include.path` steps around, and the file tree's commit
  is a `git commit` there (`Worktree`, `git_ok`), where `commit.gpgsign`
  and `gpg.program` name a program to run.
- Checkouts share the `z` label and the uid, and `label=disable` passes.

So an environment that means to can be `core`, and then root, in its VM.

## Part two: decided, and still to decide

David, 2026-11-14:

1. **Personal has a VM to itself; agent environments share.** "Personal
   apart with agents sharing."
2. **The host runs nobody's hooks; a container runs them.** "Never run
   them on the host (run them in the container). A pull should obtain the
   data on the host but run the actual "pull" in the container."
3. **No agent runs without a VM.** Of the bwrap rung: "Remove it."
4. **A submodule is fetched only from a host the project already uses.**
   "Refuse any unshared host for now. Give me at least a toast or error
   if that happens."
5. **Environments are not walled from each other.** Of one socket reading
   another's chat and writing notes all of them start with: "I'd like
   robust inter-env comms for the agents, actually. The main isolation is
   versus the host, not between envs."

What 5 settles: "Between environments" above stays as it is, between
agents. What must not follow from it is anything on this host, and that
is three things, each closed from this side: what a guest's root can
reach on the network (batch 8), the nested virtualisation it can drive
(batch 7), and what the host's clients hand it (batch 1). With those, a
guest's root is nobody here. With 1, it is nobody in Personal's VM either.

Still his:

- **Where a sign-in runs** (finding 3). Proposed: a VM of its own, booted
  from the golden base for the sign-in and destroyed after it, the way
  the golden base's builder belongs to no workspace — it has the baseline
  image and the agent runtime already, and it has never held a checkout.
  The other answer is no button: the token is pasted, from a machine the
  user trusts.
- **Whether `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` in the
  IDE's own environment stay credential sources.** They are read ahead of
  the project's file, they are the machine's and not the project's, and
  they are the one way finding 1 reaches the Anthropic credential.
  Proposed: gone from the application, kept for the tests that spend
  real tokens.
- **Whether the mirror writes links that leave the folder** (October's
  item 8). Finding 6's first bullet goes with the rung; the question
  stands for the user's own tools.

## Part three: the work

In order. Each batch says what it closes, where, and what shows it is
done, except the fifth, which waits on a decision, and the last, which is
the test of the rest. The docs a batch changes are part of it.

**1. Nothing of the host's rides a client into a VM.** Closes findings 1
and 2. `validate_run_args` takes `-e`, `--env`, and `--env=` only as
`NAME=value`, refusing a bare name and any `*` by name. Every child that
reaches a VM — `Substrate::command` and `std_command`,
`ExecContext::resolve`'s podman, the three ssh builders in `keys.rs`, and
the gits in `peer.rs` — starts from a cleared environment and a short
list (`PATH`, `HOME`, `XDG_RUNTIME_DIR`, the locale, and what the call
itself sets), with `REGISTRY_AUTH_FILE` at an empty file of the IDE's and
`DOCKER_CONFIG` at an empty directory; through `flatpak-spawn` that is
`--clear-env`. Done when the validator's tests refuse all four spellings,
and a container made by the supervisor in the host suite holds none of a
marker variable set in the test's own environment.

**2. The host's git runs nobody's hooks.** Closes finding 4, and is
decision 2. Every git run on this host carries `-c
core.hooksPath=/dev/null -c core.fsmonitor=false` (`private::cli_prefix`
and `GitWorkspace::git_command_owned` are where nearly all of them are
built; `filetree.rs` spawns a few of its own). The fetch from a VM gains
`--no-tags`. A Push runs the checkout's `pre-push` in the environment's
container first (`git hook run --ignore-missing pre-push`, given the
remote, its URL, and git's own lines on stdin), and what the hook says
when it refuses is what the Push says; a Pull's rebase and fast-forward
are in the container already (`Worktree::with_sandbox`), which is where
their hooks now run. Done when a folder whose config names a tracked
hooks directory, with a `reference-transaction` and a `pre-push` that
each touch a marker, shows no marker on this host after a sync, a Pull,
and a Push, and the `pre-push` ran in the container.

**3. A submodule is fetched only from a host the project already uses.**
Closes finding 5, and is decision 4. Before a clone or a fetch, the URL's
host is compared with the hosts of the folder's own remotes (its config,
which no VM writes); a relative URL is the parent's host by construction.
Another host is refused, the row saying which host and why, and a toast
with it. The clone is one level at a time, each `.gitmodules` checked
before it is descended into, with `protocol.allow=never` and https and
ssh allowed back. `ssh -G` takes `--` before its target. Done when a
fixture `.gitmodules` naming another host is refused with that sentence,
the banner's refused row is posed for a probe and looked at, and a nested
submodule on another host is refused after its parent is cloned.

**4. No agent runs without a VM.** Closes finding 6, and is decision 3.
`sandbox::wrap`, its arm in `AgentClient::spawn`, and its arm in
`login_command` go; `container_gate` never answers Spawn for an
environment with nowhere to run, and the chat says what it is waiting
for or why nothing is coming, which the launch note already carries.
Self-hosting is recognised by a marker the bootstrap sets, not by a file
every container has. `checkout_file` resolves links as
`fs/read_text_file` does. CLAUDE.md → "Where the agent runs" and
ARCHITECTURE → "The two modes" lose their last rung. Done when the gate's
test has no outside spawn, and the IDE started in a toolbox refuses to
start an agent and says why.

**5. Signing in happens where no project has run.** Closes finding 3.
Waits on the decision above.

**6. Personal has a VM to itself.** Decision 1. `Pool::place` leaves the
primary's VM (`registry::pinned_primary_vm`) out for every other
environment; an agent environment found there moves by the migration
that already exists; and the primary's reservation on a shared VM, which
this replaces, goes. Its VM's size wants deciding with it: a third of the
host is what a VM for many was given. Done when the pool's tests place no
agent beside the primary, and a workspace that had them mixed ends with
them apart.

**7. A guest's root is nobody on this host.** `domain_xml` disables `vmx`
and `svm` and states `<controller type='usb' model='none'/>`, written
into an older VM at its next start as the balloon was. Ignition takes
`core`'s passwordless sudo away (October's item 2), with root-owned units
for what the IDE asks of root: the port counters, the disk's growth, and
the tuning a new golden base carries instead. Needs a golden base
revision. Done when `domain_xml`'s tests hold both, and the live
provisioning test finds neither flag in the guest's `/proc/cpuinfo`, no
USB controller in the domain, and `sudo -n true` refused for `core`, with
the port counters still read and the disk still grown.

**8. Egress is enforced on this host.** October's first item, as its
spike designed it: passt in a namespace of the IDE's with its own
nftables and `--no-map-gw`, qemu on vhost-user. Until then the guest's
table gains `100.64.0.0/10`, `0.0.0.0/8`, and IPv4 multicast. A cloud
host's is the VPC's to state. Done when a guest whose own table has been
flushed still reaches neither this host's loopback nor its LAN address,
and still reaches the internet.

**9. The transport is the IDE's.** Closes finding 7. `-F none` on every
ssh. Podman reaches a guest through a unix socket that one ssh of the
IDE's forwards to the guest's `podman.sock`, the connection registered
as `unix://`, so the pinned key authenticates it and nothing is written
to `~/.ssh/known_hosts`. Worth measuring beside it: ENVIRONMENTS has an
exec through a connection at about four hundred milliseconds, and
whatever of that is a handshake, this would not repeat. Done when a test
holds `-F none` in the three builders' arguments, no `ssh://` connection
is registered for a VM, and a VM's bring-up leaves `~/.ssh/known_hosts`
as it was.

**10. The rest of finding 9**, each small, and each done when a test of
its own holds it: the proxy's paths as a list and a placeholder revoked
with its environment; `user-data` cleared once a cloud host has booted,
and a host key per VM; `taste.*` stripped from `runArgs` and the IDE's
labels last; downloads cancelled in the two views that lack it; the
passkey's origin taken from WebKit; the probe given `--network=none`, a
memory limit, and a pids limit. Two of that finding's are in no batch,
and stand as recorded: `fetch.fsckObjects`, which also refuses old
histories that git otherwise takes, and the guest image's signature,
which wants Fedora's keys pinned and a verifier of its own.

**11. An escape suite.** A hostile fixture — a config, a `.gitmodules`,
a hooks directory, and links — run under `TASTE_PROVISION_TESTS`,
asserting each batch's "done when" against a real VM. Findings 3 and 5
arrived in code written since the last audit; a suite is what notices the
next.

## What was checked and found sound

- qemu runs as `svirt_t` with an MCS pair of its own and `-sandbox
  on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny`;
  passt runs as `passt_t`. Measured on the running VM. The domain has no
  display, no shared filesystem, and one inbound forward.
- The proxy's upstream cannot be steered (`Host` dropped, an absolute
  request line reduced to its path), it follows no redirect, and neither
  the credential nor a placeholder is logged. A placeholder is 160 bits.
- The mount and build checks October fixed are an allowlist by type; a
  config cannot name another environment's volume; no IDE socket is
  mounted in a container.
- `beneath::checked` refuses `.git` at any depth, `..`, and the staging
  name, and stages with `O_EXCL`.
- Markdown opens only http(s) links, never fetches an image, and a
  notebook's HTML is named and not drawn. Mermaid, Taskfiles, archives,
  and the embedding model run under `taste_confine`; pictures in glycin;
  pages, PDFs, and media in WebKit's own sandbox.
- A cloud host has no service account, project keys blocked, and ssh
  from IAP's range as its only ingress. No Google credential enters a VM.
- MCP attributes a request to its socket's environment, never to an
  argument, and the eight coordinator tools are refused on every other
  socket as well as unlisted there.

## Not verified

- What WebKit does with a download no handler cancels.
- Whether the port guard's patterns pass `http://a@127.0.0.1/`: the rules
  were read, WebKit's matcher was not run.
- Compose, from a sweep and not re-read: a `post_start` hook stating
  `privileged`, `network_mode: bridge` joining the default bridge, and
  `compose config` reading an `env_file` before anything checks its path.
- Whether podman's own ssh client also offers the desktop agent's keys.
- Anything inside a guest: `core`'s sudo, SELinux there, and git's
  presence are the code's assumptions, which its live test passes on.
