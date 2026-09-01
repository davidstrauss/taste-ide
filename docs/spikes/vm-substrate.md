# Spike: VM substrate

Empirical answer to ENVIRONMENTS.md → "VM substrate (direction, spike
pending)". Measured on the author's host, 2026-08-31. No product code was
changed; every number below comes from a command recorded here.

**Conclusion up front: `podman machine`, for everything. Not a hybrid.**
`krun` is disqualified by capability, not by speed — it cannot `podman
exec`, which is the transport the environment channel, relocation,
`ide_exec` and the coming live shells are all built on. `podman machine`
costs +7% on a cold `cargo build` and passes the relocation test
unmodified. Both candidates need binaries the immutable host image does
not ship; that packaging gap, not the substrate choice, is the real risk.

## Environment facts

| Fact | Value |
| --- | --- |
| Host OS | Fedora Linux 44.20260828.0 Silverblue (rpm-ostree), kernel 7.1.10-200.fc44.x86_64 |
| podman | 5.8.4-1.fc44, rootless, netavark, cgroups v2 |
| OCI runtime | crun 1.28-1.fc44, `+SYSTEMD +SELINUX +CAP +SECCOMP +LIBKRUN` |
| SELinux | enforcing; user context `unconfined_u:unconfined_r:unconfined_t` |
| KVM | `/dev/kvm` is `crw-rw-rw- root:kvm` — **accessible rootless** |
| Hardware | 24 CPUs, 31 GiB RAM, 68 GiB free on `/var/home` |
| Delegated cgroup controllers | `cpu io memory pids` — **no `cpuset`** (`--cpuset-cpus` fails rootless) |
| `podman machine` provider | `qemu` (`podman machine info` → `vmtype: qemu`) |
| Present for virtualization | qemu-kvm 10.2.2, `/usr/libexec/virtiofsd` 1.14.0, edk2-ovmf |
| Guest (machine-os:5.8) | FCOS, kernel 7.1.4-200.fc44, podman 5.8.6, crun 1.29.1, SELinux enforcing, rootless |

### What the host image does not ship

Neither candidate runs out of the box on Silverblue. All of these exist in
the Fedora 44 repos; none is in the base image, and installing them is an
rpm-ostree layering operation:

| Missing | Package | Needed by |
| --- | --- | --- |
| `gvproxy` | gvisor-tap-vsock 0.8.9 | `podman machine start` (user-mode networking) |
| `virtiofsd` **in `$PATH`** | virtiofsd 1.14.0 — installed, but at `/usr/libexec/virtiofsd` | `podman machine start` looks it up in `$PATH` only |
| `libkrun.so.1` | libkrun 1.19.0 | crun's krun handler (`dlopen`ed at runtime) |
| `libkrunfw.so.5` | libkrunfw 5.5.0 | ships the microVM guest kernel |
| `krun` binary | crun-krun | selects the krun handler by `argv[0]` |

Both gaps were closed **in user space**, using documented interfaces, to
get the measurements:

- `podman machine`: a `containers.conf` with `[engine] helper_binaries_dir`
  naming a directory holding `gvproxy` and a `virtiofsd` symlink, applied
  via `CONTAINERS_CONF_OVERRIDE`, with that directory also on `$PATH`.
- `krun`: `libkrun.so.1` and `libkrunfw.so.5` in a directory on
  `LD_LIBRARY_PATH`, plus a symlink named `krun` → `/usr/bin/crun`, which
  is exactly the mechanism the `crun-krun` package itself uses.

The binaries were extracted from the official Fedora packages inside a
throwaway container; nothing was installed on the host.

**This matters for shipping.** The IDE is Flatpak-first but podman runs on
the *host*, so bundling `gvproxy` inside the Flatpak does not by itself put
it where podman looks. It can be pointed at one — `helper_binaries_dir` may
name a Flatpak-exported path under `/var/lib/flatpak/app/...` — but that is
a deliberate arrangement the IDE has to make and verify, and on a host
where it fails the IDE must say so rather than degrade silently.

## Candidate matrix

| | `podman machine` (qemu) | `krun` (libkrun microVM) |
| --- | --- | --- |
| Runs rootless | yes | yes |
| Isolation proven | guest kernel 7.1.4 vs host 7.1.10 | guest kernel 6.12.91, PID 1 = `init.krun` |
| `--userns=keep-id` / `containerUser` | **yes** — uid 1000 → `dev`, host files owned by the user | **no** — process is uid 0 inside regardless; image `USER dev` ignored |
| `:Z` bind of a host path | accepted, no error | accepted |
| `runArgs` from devcontainer.json | survive verbatim | `--memory` honored; **`--cpus` ignored** (`nproc` = 16 either way) |
| Named volume on fast local disk | **yes** — inside the VM's own disk | **no** — every mount is host-backed virtio-fs |
| systemd as PID 1 in a container | **yes** (dbus-broker active) | **no** — `init.krun` is PID 1; systemd exits 1: `Explicit --user argument required to run as user manager` |
| `podman build` RUN under isolation | yes (ordinary podman inside the VM) | **yes, proven** — RUN step sees kernel 6.12.91 |
| `podman exec` | **yes, byte-exact** | **no** — `Error: the handler does not support exec` |
| Relocation live test | **4/4 pass** | cannot run (no exec) |
| Host memory, idle | ~1.35 GB per machine | **103 MB per microVM** |
| Host memory, after load | ratchets to the configured ceiling, never returns | scales with actual use |

### Timings

Cold `.devcontainer` image build, `--no-cache`, base image pre-pulled on
both sides so the number isolates the repo's `RUN` steps:

| Where | Time |
| --- | --- |
| Inside the machine (8 vCPU) | **131.2 s** |
| Host, unrestricted (24 CPU) | 146.8 s |
| Host under `krun` (16 vCPU) | 258.2 s |

The machine *beat* the host here. Mirror and network variance dominates a
`dnf install` of this size; the honest reading is that image builds carry
no measurable VM penalty, not that virtualization is free. CPU parity could
not be forced on the host side — `cpuset` is not a delegated controller
rootless, so `podman build --cpuset-cpus` fails.

Cold `cargo build --workspace --locked`, registry pre-warmed in every case
(`cargo fetch` inside the machine over gvproxy took 4.6 s):

| Configuration | Time | vs CPU-matched host |
| --- | --- | --- |
| Machine, workspace virtiofs, **target in a VM-local named volume** | **70.5 s** | **+7%** |
| Machine, workspace virtiofs, target on the virtiofs share | 100.0 s | +52% |
| Host, bind mount, target on host FS, `--cpus=8` | 65.9 s | — |
| Host, bind mount, target on host FS, unrestricted (24 CPU) | 64.3 s | — |
| `krun`, target in a (host-backed) named volume | 206.6 s | +214% |

Two things fall out. **Per-environment volumes are load-bearing, not an
optimization**: moving `target/` off the shared filesystem is worth 30% of
a cold build (70.5 s vs 100.0 s), and the design already has them. And
`krun` has no way to earn that back, because it has no VM-local disk at
all — every mount it offers is host virtio-fs.

Inner loop, no-op `cargo build --workspace` with everything already built:

| Where | Time |
| --- | --- |
| Machine (source over virtiofs) | 1.59 s |
| Host | 0.69 s |

Cargo's fingerprint pass pays about **0.9 s extra per incremental build**
just to stat the tree over virtiofs. Against "performance is a
no-compromise requirement", this is the cost the user will actually feel —
not the cold build. It is the strongest argument for moving agent
environments before the primary one.

Machine lifecycle:

| Operation | Time |
| --- | --- |
| First boot after init | 17.9 s |
| Warm boot (stop → start) | 13.8 s, 14.8 s |
| Stop | 1.0 s |
| `init` including image download | under 120 s (not timed precisely) |

Memory is the machine's real price. Idle after boot: qemu 1314 MB +
gvproxy 27 MB + virtiofsd 4 MB ≈ **1.35 GB**. After one image build and
one cargo build, the qemu RSS had climbed to **8.44 GB** — the full
configured 8 GiB. The command line is `-object memory-backend-memfd,
size=8192M,share=on` with **no balloon and no free-page reporting**, so
guest page cache ratchets host RSS to the configured ceiling and never
returns it. The memfd is shmem, so it is swappable, but the IDE should
size the machine as a commitment, not a ceiling.

Disk ratchets the same way, and further. The machine's qcow2 was 2.0 GB
fresh and **13 GB** by the end of this spike — one devcontainer image, two
throwaway images and two cargo builds — and qcow2 does not shrink when the
guest frees space. Add the 1.1 GB image cache and the substrate costs
about 14 GB of host disk for one machine that has done a day's work.
Against ENVIRONMENTS.md's "disk honesty" requirement this is a real
regression: the fleet view's per-environment footprint becomes an
*in-guest* number that no longer explains what the host lost. Reclaim
needs `fstrim` in the guest plus a discard-enabled backing store, which
this provider does not appear to configure; that is worth confirming
before shipping, because "the IDE ate 14 GB and freeing environments did
not give it back" is exactly the complaint the disk-honesty rule exists to
prevent.

### The channel across the VM boundary

The stdio-over-`podman exec` bridge crosses the boundary intact. 8 MiB of
`/dev/urandom` piped through `podman exec -i <ctr> cat` came back with an
identical sha256 through the connection.

| | Host | Through the connection |
| --- | --- | --- |
| 8 MiB round trip | 0.209 s | 0.491 s |
| 20 `podman exec` invocations | 3.877 s (194 ms each) | 8.267 s (413 ms each) |

Each `podman exec` costs about **+219 ms** through the connection. The
environment channel is opened once per environment and multiplexed, so
this lands on environment startup, not per message. It does land on every
`ide_exec` and every agent-requested terminal, which is worth knowing
before the terminal work assumes exec is cheap.

### The relocation live test

`crates/taste-acp/tests/relocation.rs`, built in the devcontainer and run
from the host, **passes 4/4 against a machine-hosted container**:

| Run | Result |
| --- | --- |
| Host (control) | 4 passed in 11.3 s |
| Machine-hosted container | **4 passed in 18.1 s** |

That is the whole of phase 4 across a VM boundary: the agent spawned inside
a machine-hosted container, reported `TASTE_IDE_CONFINEMENT=container-exec`,
saw its checkout at its real host path, got answers from the IDE's MCP
tools through the channel, and had a real credential swapped in by the auth
proxy — with the container confined normally, no `label=disable`.

The pass is genuine, and it is worth saying why, because the test also uses
`/tmp`. The container binds `TASTE_TEST_REPO` (`{root}:{root}:Z`), which
lives under `$HOME` and is therefore on the machine's virtiofs share; the
`/tmp` tempdir is the MCP server's own workspace, host-side only, reached
through the channel and never mounted. Had it been mounted, the run would
have failed loudly, not silently — see below.

One retargeting trap: the environment variable is **`CONTAINER_CONNECTION`**
(singular). `CONTAINERS_CONNECTION` — the spelling the plural
`containers.conf` family trains you to write — is silently ignored, and the
test then runs against the host while looking like it ran against the VM.

## Path visibility: the one real compatibility constraint

Only explicitly shared host paths exist inside the machine. The default
share is `$HOME:$HOME` (`podman machine init --volume`, default
`[$HOME:$HOME]`), and shares can only be set **at init** — `podman machine
set` has no `--volume`.

`/tmp` cannot be shared at all:

```
Error: machine mount destination cannot be "/tmp": consider another
location or a subdirectory of an existing location
```

Binding a host path the VM does not have fails loudly rather than mounting
an empty directory:

```
Error: statfs /tmp/taste-visibility-probe: no such file or directory
```

That failure mode is the good one — no silent divergence between what the
IDE thinks it mounted and what the agent sees. But it makes a rule: **every
host path the IDE binds into a container must live under the machine's
shared set.** Today's topology survives because checkouts and per-env
clones live under the user's home. Anything the IDE stages in `/tmp` —
staging directories, socket paths, scratch files — must move under the
workspace or the state directory before the migration. That audit is a
prerequisite, and it is cheap to do now.

## Baseline environment numbers

The "ship it with the IDE, never fetch" image, built as
`fedora-minimal:44` + `git nodejs findutils diffutils less tar gzip
ripgrep` (`--nodocs`, no weak deps), giving git 2.55.0 and node v22.23.1:

| Measure | Value |
| --- | --- |
| Build time | 19.8 s |
| Image on disk | 289 MB (the fedora-minimal base alone is 140 MB) |
| **OCI archive (`podman save`)** | **106 MB** (110,601,728 bytes) |
| `gzip -6` of that archive | 105 MB — **no gain**, layers are already compressed |
| `podman save` | 1.3 s |
| `podman load`, host, cold | 1.7 s |
| `podman load`, into the machine | 2.7 s |

**106 MB and under three seconds** is the number behind shipping the
baseline rather than fetching it. Loading it into the machine at
provisioning time is cheap enough to do eagerly, every time, without asking.

## Recommendation

**Machine for everything.** Not a hybrid, not krun-for-runs.

The decisive facts are capabilities, not timings:

1. **krun cannot `podman exec`.** The environment channel, relocation,
   `ide_exec` and the queued live-terminal work all ride stdio over
   `podman exec`. A hybrid that ran containers under krun would need a
   second transport (vsock) for exactly the containers it hosts — a second
   mechanism that has to agree with the first, which is the shape of thing
   this project's rules keep refusing.
2. **krun cannot run systemd as PID 1**, because `init.krun` already is.
   Devcontainer compatibility and the standing preference for systemd with
   socket activation both argue against it.
3. **krun breaks `containerUser`/keep-id**: everything runs as uid 0 inside
   regardless of what the devcontainer asks for. Devcontainer compatibility
   is non-negotiable; this is a compatibility break, not a quirk.
4. **krun is slower with no way back**: 3.1x on cold cargo, 1.75x on image
   builds, and no VM-local disk to move `target/` onto.

Against that, `podman machine` gives up 7% on a cold cargo build, is not
slower on image builds at all, keeps every devcontainer knob working, and
carries the existing relocation machinery across the boundary unmodified.
It also covers builds for free — builds are just podman inside the VM —
which was the requirement that made this a hard problem.

**krun's honest residual role** is worth recording rather than discarding:
`podman build --runtime krun` genuinely isolates repo-supplied `RUN` steps
under a separate kernel, rootless, today. If per-build microVM isolation is
ever wanted *inside* the machine, or on a host where a machine cannot be
provisioned, that path is proven to work. It is a note, not a plan.

### What the substrate field should be

```
Substrate::Host                              // today: rootless podman on the host
Substrate::Machine { connection: String }    // a named podman connection
```

No `Krun` variant. The podman wrapper — already the one seam — gains a
connection dimension: every invocation takes `-c <connection>`, and every
shelled-out child that runs podman itself gets `CONTAINER_CONNECTION` in
its environment. `AgentHosting` probes through the connection exactly as it
probes locally; nothing it asks changes.

### Migration order

1. **Agent environments first.** They are what the decision is *for* — N
   autonomous agents running semi-unattended — they already have per-env
   volumes so the 30% is already banked, and the relocation path is proven
   against a machine-hosted container. Their inner-loop latency is a
   robot's problem, not a human's.
2. **Image builds follow automatically.** Building through the connection
   is the same command; an environment that lives in the machine builds in
   the machine. This is where the sharpest untrusted-code edge gets
   covered, and it costs nothing extra once (1) has landed.
3. **The safe-mode baseline.** Load the 106 MB archive into the machine at
   provisioning. `NoConfig` stops being a dead state, in the VM, with
   `podman exec` available for the repair loop.
4. **The primary environment last.** It is where the +0.9 s per incremental
   build is felt by a human, and where a regression is most visible. Moving
   it should be a separate, reversible decision made after (1)–(3) have run
   in anger.

### Prerequisites before any of it

- **Resolve the helper-binary gap.** `gvproxy` and a `$PATH`-visible
  `virtiofsd` are hard requirements, absent from an immutable Fedora base
  image. The IDE must bundle them and point `helper_binaries_dir` at them,
  or detect their absence and explain it. This is the largest portability
  risk in the whole direction and it is a packaging problem.
- **Audit every bound path** against the machine's shared set, and move
  anything under `/tmp` — which cannot be shared — beneath the workspace or
  the state directory.
- **Decide machine sizing as a commitment.** Host RSS ratchets to the
  configured memory and never returns it; there is no balloon. Disk
  ratchets too — confirm whether guest `fstrim` can reclaim qcow2 space
  under this provider before promising disk honesty in the fleet view.
- **Idle-stop should stop containers, not the machine.** A stopped machine
  costs 14 s to come back and takes every environment down with it.

### What this spike did not measure

Precise `podman machine init` time; sustained IO benchmarks; several
environments running concurrently, which is the case the memory ratchet
actually threatens; and krun under any workload past the point where its
lack of `exec` had already decided the question.

## Artifacts created, and cleanup

Everything below was created by this spike, all in user space. Nothing was
installed on the host, no rpm-ostree or systemctl operation was run, and no
SELinux setting was touched. The user's `containers.conf` was never
modified — the helper configuration was applied through
`CONTAINERS_CONF_OVERRIDE` — but `podman machine` did write its connections
to `~/.config/containers/podman-connections.json`, which `podman machine
rm` reverses.

| Artifact | Removal |
| --- | --- |
| Machine `taste-spike` (qcow2, 2.0 GB fresh → 13 GB used) | `podman machine stop taste-spike && podman machine rm -f taste-spike` |
| Machine connections `taste-spike`, `taste-spike-root` | removed by `podman machine rm` |
| Machine image cache (~1.1 GB) | `rm -rf ~/.local/share/containers/podman/machine` *(after the machine is removed)* |
| Helper dir with gvproxy, libkrun, libkrunfw, `krun`/`virtiofsd` symlinks, `containers.conf` | `rm -rf ~/.local/lib/taste-spike-helpers` |
| Work dir (6.1 GB): repo copy at HEAD, build logs, `baseline.tar`, `systemd.tar`, target dirs | `rm -rf ~/.local/share/taste-spike` |
| Host images `taste-spike-devcontainer-host`, `taste-spike-devcontainer-krun`, `taste-spike-systemd`, `taste-spike-baseline`, `taste-spike-probe-krun`, `taste-spike-probe-crun` | `podman rmi -f <name>` |
| Host volumes `taste-spike-cargo-host`, `taste-spike-krun-target` | `podman volume rm <name>` |
| Host containers `ch-host`, `krun-mem` | `podman rm -f <name>` |
| Images, volumes and containers created *inside* the machine | die with the machine |
| `/tmp/taste-visibility-probe` | `rm -rf /tmp/taste-visibility-probe` |

Already cleaned during the spike: the `krun-sleep` and `sd-krun` probe
containers, and the `.keepid-probe` / `.krun-probe` marker files. The
user's own `taste-ide-devcontainer` image, `taste-ide-cargo` volume and
repository `target/` were deliberately never touched — every build here ran
against a separate copy of the repo at HEAD with its own image, volumes and
target directory.
