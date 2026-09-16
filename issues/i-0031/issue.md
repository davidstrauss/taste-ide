---
title: The inotify instance limit is a shared host resource, and it fails four watcher tests whenever the fleet is busy
state: declined
reporter: i-0011
created: 2026-09-16T01:56:48Z
updated: 2026-09-16T01:57:20Z
labels: bug, tests, environments
---

**What is wrong.** `cargo test --workspace` fails in `taste-core::watcher::tests` and `taste-devcontainer::configwatch::tests` whenever enough of the fleet is running, with the code's own diagnosis:

```
called `Result::unwrap()` on an `Err` value: Too many open files (os error 24):
the per-user inotify instance limit (fs.inotify.max_user_instances) is exhausted.
It is shared by everything running as this user — the desktop session, every editor,
and every container in the fleet, which all run as this uid under rootless podman.
Raising it is a host change: `sysctl fs.inotify.max_user_instances`
```

Found while running the gate for i-0011, and **confirmed pre-existing**: stashing that branch's changes and running the same two test sets against clean `main` fails identically (in fact worse — 7 `configwatch` failures on `main` against 3 with the branch applied, which is the giveaway).

**Why it is worth writing down rather than shrugging at.** The failure count moves between runs, which is exactly what makes it expensive: every agent that runs the gate has to work out from scratch whether it broke the watcher, and the honest answer takes a stash, two builds, and a paragraph in a handover. Three environments were live on this host today; six is the cap. The limit is per-*user*, and rootless podman means every container in the fleet shares it with the user's own desktop session, so this gets worse exactly as the fleet gets used.

It is not a real defect in the watcher — the error message proves the code already understands the failure — but a test that fails for a reason outside the repository is a test that stops being read.

**Some options, none obviously right.**

- Skip rather than fail when the watcher cannot be created, the way the GTK tests skip without a display (`chat::tests` prints "no display — skipped"). Cheap and consistent with the house style; the cost is a test that silently stops covering anything on a busy machine.
- Share one inotify instance across the tests in a crate. `configwatch` already exists to share one across the fleet, so the tests fighting over instances is slightly ironic.
- Raise `fs.inotify.max_user_instances`. A host change, so the user's, and it only moves the ceiling.

**Done looks like** `cargo test --workspace` giving the same answer on a busy host as on an idle one — either passing, or saying plainly that it could not run rather than that something is broken.
