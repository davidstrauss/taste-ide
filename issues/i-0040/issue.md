---
title: The host volume is full, so the workspace build gate cannot run in any environment
state: open
reporter: i-0032
created: 2026-09-16T03:31:31Z
updated: 2026-09-16T03:32:41Z
labels: environments, infrastructure
---

**What is wrong.** `/var/home/straussd` has 1.4 GiB free of 930 GB — 100% used, and 8.6 GiB under the IDE's own floor. `issue_list` already says so:

```
free: 1.4 GiB, floor: 10.0 GiB, below_floor: true
"free disk is 8.6 GiB under the floor: issue_start and devcontainer_reload refuse
 whatever the budget says, until space is freed on this machine"
```

**What it costs beyond the refusals the IDE already names.** A build fails with no message. From i-0032, building `taste-app` for the first time in a fresh clone:

```
error occurred in cc-rs: command did not execute successfully (status code exit status: 1):
LC_ALL="C" "cc" … "-o" ".../libgit2-sys-…/out/build/…-errors.o" "-c" "libgit2/src/util/errors.c"
```

No stderr from `cc`, no "No space left on device" anywhere in the output — just a non-zero exit. Read cold, that is a broken toolchain or a bad dependency, and the first hour of chasing it goes in the wrong direction. It is ENOSPC.

So the workspace gate (`cargo test --workspace`, and `cargo clippy --workspace`) is not runnable in any environment that has not already built `taste-app` once: a first debug build of that crate is tens of gigabytes of GTK, WebKit, whisper and llama artifacts and there is nowhere to put them. An agent in a fresh clone can gate the crates it happens to fit and no more, which is a batch that cannot be verified the way CLAUDE.md requires.

**Where the bytes are.** Not in the clones: the disk budget measures 141.7 MiB across 21 environments under the clones-only scope, which excludes build artifacts — and the build artifacts are the whole of it. 21 environments on disk, 4 running. `environment_destroy` on the ones the user has already merged or rejected (`review_list` says which) is the only thing that gives those bytes back; stopping an environment gives back nothing.

**Done looks like** two things, and the second is the one worth the issue:

1. The bytes come back. That is the user's to do — freeing space on this machine is not the IDE's and not an agent's — but the fleet is what filled it, so the coordinator is where the ask belongs.
2. **An IDE that says which it is.** The IDE knows the volume is under its floor: it refuses `issue_start` and `devcontainer_reload` with a sentence naming the shortfall. Nothing carries that fact to the place it actually bites, which is a build inside a container failing with an exit code and no words. A `cargo` run that dies of ENOSPC in an environment whose supervisor already knows the volume is below the floor should be told so — in the environment's log, and to the agent that ran it. The same shape as i-0030 and i-0035: a host resource the fleet exhausts together, failing as something that reads like a bug in the work.

Sibling to i-0030 (inotify instances) and i-0035 (saying that fleet-wide condition out loud). Filed from i-0032, which hit it trying to run its own gate.
