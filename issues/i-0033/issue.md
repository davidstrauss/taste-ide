---
title: The watcher tests fail on a busy machine: fs.inotify.max_user_instances is 128 and the fleet shares it
state: declined
reporter: i-0028
created: 2026-09-16T01:59:36Z
updated: 2026-09-16T02:03:11Z
labels: bug, tests, fleet
---

**What happens.** `cargo test --workspace` fails in `taste-core::watcher` on this host, with a different subset of those eight tests failing on each run:

```
called `Result::unwrap()` on an `Err` value: Too many open files (os error 24):
the per-user inotify instance limit (fs.inotify.max_user_instances) is exhausted.
```

The message is the codebase's own (`taste-core/src/watcher.rs`), so the condition was anticipated. What was not is that it now happens routinely: `/proc/sys/fs/inotify/max_user_instances` is **128** on this machine, and that pool is shared by the desktop session, every editor, and every container in the fleet, which all run as the same uid under rootless podman. With a few environments up there is nothing left for a test to take.

**It is not test parallelism.** `--test-threads=1` fails the same way, on a different pair. The instances are gone before the test binary starts.

**Why it matters.** It is the project's own gate (CLAUDE.md → House rules: `cargo test --workspace` before every commit), and it fails for a reason that has nothing to do with the change under test. An agent that does not read the message carefully will either chase a phantom regression or learn to wave test failures through, and the second is much worse.

**Not in scope of what found it.** Noticed while finishing i-0028; nothing in that change is anywhere near the watcher.

## Options, roughly in order of how much they cost

- **Take fewer instances.** One inotify instance per workspace rather than one per watched root, or per test — worth checking what `notify`'s backend actually allocates and whether the slot abstraction multiplies it. This is the only option that fixes it for the user as well as for the tests, since the fleet is what exhausts the pool and the fleet is us.
- **Skip rather than fail.** A watcher test that cannot get an instance is measuring the host, not the code; `os error 24` at setup could print and return, the way the GTK tests do without a display. Honest, and cheap, and it hides a real degradation from the user if the first option is never taken.
- **Raise the limit.** A host change (`sysctl fs.inotify.max_user_instances`), so it is the user's to make and not something the IDE can arrange. Worth saying out loud in the docs either way, next to the fleet cap — it is a third ceiling the fleet runs into, alongside the environment cap and the disk budget.

## Acceptance

`cargo test --workspace` passes with a full fleet running, or says plainly that it cannot and why — and if the first option is taken, the IDE's own watcher footprint is measured before and after.
