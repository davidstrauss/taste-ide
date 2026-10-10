# Tightening the sandbox around running environments

2026-10-10. An audit of the environment sandbox against the standard in
ENVIRONMENTS → "Isolation: the standard, and what meets it" — a project
presents no special threat beyond what a random VM on the internet could
do — read off the code and measured where a claim depended on a tool's
behaviour rather than on ours. Part one is what was found crossing the
line and fixed the same day. Part two is what would tighten it further,
ranked, each with what it costs and what has to be decided, none of it
built.

Measurements were taken on the development host (Fedora 44, passt and
pasta `0^20261002.gcba3570`, libvirt 12.0.0, podman 5.8.7). The guest's
own podman was not measured; where the host's stood in for it, that is
said.

## Part one: found and fixed

### 1. A file in the checkout could overwrite any file in the user's home

`taste_git::mirror` wrote each file beside its place as
`.{name}.taste-mirror` and renamed it over, with a plain
`std::fs::write`, which follows a link. A checkout holding the link
`.notes.taste-mirror -> /home/u/.bashrc` and a file `notes` — the link
sorts first, so one pass writes both — put the file's bytes into
`~/.bashrc`. That is a process on the user's machine, run from a VM's
files: clause 4 of the standard, crossed by an agent writing two files.

Fixed by `taste_git::beneath`, which every host write a VM names now goes
through: the path stays in the folder and out of `.git`, no directory on
the way may be a link, the staging name is refused as a tree path and is
created fresh (`O_CREAT|O_EXCL`, which does not follow a link) after
whatever held it is removed, and the bits are set through the
descriptor, with setuid, setgid, and sticky never carried over.

### 2. Two other writers had no checks at all

`GitWorkspace::adopt_first_commit` (a project's first commit, brought into
a folder whose branch has none) and `peer::copy_ignored_home` (an ignored
file the user asked for) wrote VM-named paths with `create_dir_all` and
`std::fs::write`, so through any link on the way — and the mirror puts
links into the folder with whatever target the checkout gave them. The
second also took its paths from the VM's `git ls-files`, whose answer
the VM controls. Both now go through `beneath`.

### 3. A submodule path through a link reached other projects

`.gitmodules` is mirrored, links are mirrored, and `GitWorkspace::submodules`
checked a submodule's path only lexically. A link `sub -> ../sibling`
beside a `.gitmodules` naming `sub` is lexically inside, resolves on the
host to `~/Projects/sibling` (another of the user's projects) and in the
VM to `/var/home/core/taste/<workspace>/sibling` (another environment's
checkout, which has a `.git`). The sync would then have pushed the
sibling project's heads, remotes, and tags into the VM — clause 5,
another project — and mirrored the checkout into its working tree.
libgit2 does list a submodule that only `.gitmodules` declares (the
regression test confirms it). Fixed in `submodules()`, which now leaves
out any path with a link anywhere on it, its own name included.

### 4. A repo's config could pass any podman flag

The check skipped `--privileged` wherever it stood, a value-taking flag
before it swallowed it as its value, and the run then deleted it. So
`["--label", "--privileged", "--label", "--network=host"]` validated as
two labels and reached podman as `--label=--label --network=host` —
and the same for `-v`, `--pid=host`, `--cap-add=ALL`, anything. Fixed:
the check reads `security::run_args_to_pass`, the list the run passes,
and a value-taking flag's value may not start with `-`.

### 5. A repo's mount could bind any path in the VM

Measured against podman 5.8.7, both passing the old check:

- `"type=bind",source=/anywhere,target=/x` — podman parses a mount as
  CSV, so the quotes go and it is a bind; split on commas, it has no
  `type` key, and an untyped mount was accepted as a volume.
- `type=volume,source=v,target=/x,volume-opt=type=none,volume-opt=o=bind,volume-opt=device=/anywhere`
  — podman's own way of making a volume that is a bind, and volumes were
  accepted "with any options".

With `label=disable` (allowed) beside either, the container could write
`/var/home/core`, which is a systemd user unit run as `core`, and `core`
has passwordless sudo (Part two, item 2): guest root from a
`devcontainer.json`, no kernel bug needed, and from guest root the host's
loopback (item 1). Fixed: no quotes, `type` stated, no repeated or
aliased key (`source` beside `src` was the same trick again), and only
the options listed for that type.

### 6. A repo's config could raise its own memory ceiling

The pool's grant was passed before the config's `runArgs`, and podman
takes the last `--memory` and `--cpus`. Fixed by passing the grant last.

## Part two: what would tighten it further

Ranked by what each closes against the standard, highest first.

### 1. Enforce the guest's egress limits on the host, not in the guest

**What is open.** The guest's network is libvirt's passt backend, and
passt maps the guest's gateway address to the HOST'S LOOPBACK by default.
Measured with pasta, which shares that code: a server on the host's
`127.0.0.1:48631` answered a request to `http://192.168.86.1:48631/` from
inside the namespace, and `--no-map-gw` refused it. On this machine the
host's loopback holds VS Code's extension-host servers, CUPS, Steam, and a
Java service; in general it holds every other workspace's forwarded ports
(`ssh -L 127.0.0.1:N`) and VM ssh ports — other projects, clause 5.

What stands in the way today is an nftables table in the guest
(`provision::egress_ruleset`), which the code itself says root in the
guest can remove, and `core` has passwordless sudo. It also leaves out
`100.64.0.0/10` (CGNAT, and every Tailscale address: passt connects from
the host, so the guest reaches the user's whole tailnet), lets port 53
through to any address, and a host whose gateway is not in an RFC1918
range has no block on its loopback at all.

**The proposal.** Run the VM's passt ourselves instead of letting
libvirt spawn it, inside a network namespace that filters, and connect
qemu to it over vhost-user:

```
qemu ──vhost-user (unix socket)──► passt --vhost-user --no-map-gw   (in namespace N)
                                         │ sockets in N, nftables in N decides
                                         ▼
                                   pasta --config-net --no-map-gw   (N ↔ host)
```

N is a user and network namespace the IDE owns, so it can load nftables
there without host root — the reason the in-guest firewall was chosen is
that "a host firewall rule would need root", and this needs none.
Measured: in `pasta --config-net --no-map-gw`, `nft -f` loaded as the
namespace's root; with a ruleset refusing private, CGNAT, link-local,
loopback, and multicast destinations and allowing port 53 only to the
resolver, the router's web UI was refused (it answers 200 without the
ruleset), and DNS and `https://example.com` worked. The guest can no
longer touch any of it, so guest root stops being the question.

Costs and unknowns:

- libvirt has to take an `<interface type='vhostuser'>` with
  `<source type='unix' mode='client'>`, which needs shared memory
  backing (`<memoryBacking><source type='memfd'/><access mode='shared'/>`).
  The memory is committed already (`provision` module docs), so sharing
  it changes no number, but it is a VM boot nobody has done yet.
- ssh's inbound door moves: pasta forwards the host's `127.0.0.1:PORT`
  into N (`-t`), and passt forwards it to the guest's 22.
- Two more long-lived processes per VM, which the IDE then supervises
  rather than libvirt.
- A cloud host does not need it: its egress is the internet, by design.

**Cheaper, partial, and doable now:** add `100.64.0.0/10`, `0.0.0.0/8`,
and multicast to `PRIVATE_NETWORKS`, and narrow port 53 to the gateway
address. Still in the guest, so still removable by guest root, but it
closes the tailnet for everything short of that.

### 2. Take passwordless sudo away from `core`

Every container in a VM runs under rootless podman as `core`, so anything
that becomes `core` — a container escape, the mount bypass above, a
systemd user unit written by either — is one `sudo -n` from guest root,
and guest root removes the egress table and reads every environment's
files regardless of SELinux. The IDE uses root in the guest for two
things: `guest_tuning_script` (re-patching a running VM) and the port
counters (`ports.rs`, `sudo -n nft`). Both could be a second user with its
own key, never handed to podman, or root-owned units the guest runs on
its own. FCOS gives `core` NOPASSWD through `/etc/sudoers.d`, which
Ignition can overwrite. This makes the in-guest firewall and SELinux a
real second layer instead of a first one with a door in it. It needs a VM
to verify, and the failure mode is a guest the IDE cannot re-patch.

### 3. Stop allowing `label=disable`

It is allowed because podman-in-podman guides write it. Nesting is
already granted as `container_engine_t`, which is confined and works
(ENVIRONMENTS → nesting, measured). With SELinux on, a bind of a path
outside the checkout is refused by the label even when the mount check
has a bug, which is exactly what fixes 4 and 5 would have needed. The
proposal is to translate `label=disable` into the nesting set the way
`--privileged` already is, rather than pass it. Cost: a project whose
container genuinely needs SELinux off stops working, and the probe after
start would say so.

### 4. Give each environment's checkout its own SELinux categories in a VM

In a VM every checkout is bound with the shared `z` label, so SELinux does
not separate one environment's files from another's — a container that
escapes its mount namespace but stays `container_t` reads its
neighbours' checkouts. The keeper already runs with every category
(`label=level:s0-s0:c0.c1023`), so private `Z` labels per environment
would not cost it access. Why the VM moved to `z` should be found before
changing it; this is clause 5's "environments of one workspace share a
VM" residual made smaller, not closed.

### 5. Fence the port tab's browser at the network, not by URL pattern

The port tab's WebKit view keeps a page off the host's other loopback and
LAN services with a content-blocker list of URL patterns
(`portview.rs`, `network_guard_rules`). Patterns are the wrong layer:
`localhost.` with a trailing dot, public names that resolve to
`127.0.0.1` (`localtest.me`), and DNS rebinding all get past a list of
spellings. The fix is to give the view's `NetworkSession` a proxy the IDE
runs, which resolves each request itself and connects only to the one
forwarded port — a decision made on the address actually dialled, which
rebinding cannot get under. `set_enable_developer_extras(true)` should
also be looked at.

### 6. The cloud host's Ignition stays in its metadata

A cloud guest's whole Ignition, the ssh host private key in it, is the
instance's `user-data` metadata for the instance's life, readable by guest
root and by anyone in the project with `compute.instances.get`. And the
host key is shared by every VM in the workspace's pool, so one guest's
copy is every guest's. Two fixes, independent: clear `user-data` once
the guest has booted (the IDE's role already holds
`compute.instances.setMetadata`), and mint a host key per VM.

### 7. Harden the IDE's own git against the VM peer

The sync's `git fetch` from the VM runs with the user's global config and
the folder's hooks, and no `-c` of its own. `-c core.hooksPath=/dev/null`
on the IDE's own sync invocations (never the user's Push and Pull) costs
nothing. `fetch.fsckObjects=true` would refuse malformed trees — duplicate
entries, `.git` paths, a `.gitmodules` that is a link — before anything
host-side walks them, which is the class fixes 1 to 3 were in. Its cost is
real: an old history with a zero-padded file mode fails fsck, so it wants
`fetch.fsck.<msg-id>=ignore` for the harmless messages, chosen by
measuring real repositories rather than guessed.

### 8. Ask whether the mirror should write links that leave the folder

The mirror writes the checkout's links with any target, absolute ones
included; `beneath` makes sure the IDE never writes through one, but the
user's own tools in that folder still might — an editor, a build, `cp
-r`. Refusing (or writing as a plain file) a link whose target resolves
outside the folder would close that, at the cost of the rare project that
commits such a link. This is the "Mirror all, note the risk" decision of
2026-09-23 narrowed to links, so it is David's to make.

### 9. Smaller items

- **The userns probe runs the project's image with no limits**
  (`supervisor.rs`, `podman run --rm --entrypoint sh <image> -c "id -u; id
  -g"`). `--network=none`, `--memory`, and `--pids-limit` cost nothing.
- **`taste-embed` is not confined** (CLAUDE.md says so); it tokenizes the
  checkout's text with llama.cpp in a host process.
- **The build has no process limit** (ROADMAP → "Bound a runaway build
  step"), still open.
- **No container in the guest reaches the guest's loopback**, measured
  with the host's podman 5.8.7 standing in for the guest's: pasta's
  defaults refused a container's request to a server on the host's
  `127.0.0.1`. So one environment cannot reach another's published ports
  on the guest's loopback. Worth confirming in a guest, since the whole
  cross-environment story leans on it.

## What was checked and found sound

- The auth proxy is not an open proxy: scheme and host come from the
  configured upstream, only path and query pass, and every request needs
  a placeholder the proxy issued.
- No container gets the guest's podman socket, and no socket is mounted
  at all; the MCP and auth sockets are bound inside by an exec'd helper.
- The guest's ssh is reached with a pinned host key, its own identity
  and known-hosts file, `IdentityAgent=none`, and `BatchMode`.
- The cloud host has no service account, so its metadata server holds no
  token, and its only ingress is ssh from IAP's range.
- `fetch` never recurses into submodules and the host runs no `git
  submodule update`, LFS, or clone on VM data.
