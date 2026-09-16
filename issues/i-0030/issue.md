---
title: The workspace test gate fails on four filesystem-watcher tests once the fleet has exhausted the host's inotify instances
state: open
reporter: i-0029
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-16T01:56:42Z
updated: 2026-09-16T01:57:28Z
labels: bug, tests, fleet, host
---

**What happens.** With three environments live on this host (2026-09-16: i-0011, i-0028, i-0029), `cargo test --workspace` fails in every environment, on four tests that have nothing to do with the change under test:

```
taste-core   watcher::tests::end_to_end_modify_event_reaches_bus
taste-core   watcher::tests::the_slot_watches_only_while_aimed_at_something
taste-core   watcher::tests::unreadable_subdir_does_not_kill_the_watcher
taste-devcontainer  configwatch::tests::every_environment_shares_one_instance
```

All four fail the same way, and the IDE's own error text names the cause exactly:

```
called `Result::unwrap()` on an `Err` value: Too many open files (os error 24): the
per-user inotify instance limit (fs.inotify.max_user_instances) is exhausted. It is
shared by everything running as this user — the desktop session, every editor, and
every container in the fleet, which all run as this uid under rootless podman.
Raising it is a host change: `sysctl fs.inotify.max_user_instances`
```

The same exhaustion shows up in the IDE at runtime, not only under test — app log, 2026-09-16 01:31:12:

```
WARN taste_core::watcher: watching …/environments/799fd7acd369bf5c/i-0025/repo failed:
Too many open files (os error 24): the per-user inotify instance limit … is exhausted
```

`the_slot_watches_only_while_aimed_at_something` fails with a plain `assert_eq!` rather than the message (left `None`, right `Some("/tmp/.tmp…")`), which is the same starvation wearing a disguise — the watcher never attached, so the slot reports nothing.

**Why it matters.** CLAUDE.md makes `cargo test --workspace` the gate every batch has to pass, and this makes the gate red for reasons no agent in an environment caused and none can fix: the limit is per-user and host-wide, and raising it is a `sysctl`. Today that leaves every worker having to judge for itself which failures are its own — which is exactly the reading a gate exists to remove, and a worker that gets the judgement wrong either ships on a real failure or stalls on a false one. It gets worse as the fleet grows, which is the direction the fleet goes.

**What is not the answer.** Skipping the tests: they cover the watcher's actual behaviour, and the IDE's live file watching starves for the same reason, so the test failing is honest news about the machine. Nor should the watcher be made to swallow the error — it already reports it well.

**Done looks like** one of these, decided deliberately:

- the four tests recognise the exhaustion and report it as *skipped, host limit reached* rather than failed, the way the GTK tests already skip without a display (`if gtk::init().is_err() { println!("…skipped"); return; }`). Honest, keeps the gate readable, and leaves the real assertions in force on a machine with room. This is the cheapest and probably right.
- and/or the IDE says it once, where it matters: the exhaustion is a fleet-wide condition, and it is currently a WARN line in a log. A machine that can no longer watch files is something the coordinator should be able to tell the user about (CLAUDE.md → "supervision happens at the orchestrator level").
- and/or `bootstrap.sh`/README name `fs.inotify.max_user_instances` as a host prerequisite with the number a fleet of six needs, since it is a host change and the host is expected to be bare.

**Where.** `crates/taste-core/src/watcher.rs` (the tests around lines 390, 495, and 528), `crates/taste-devcontainer/src/configwatch.rs:437`. The error text those tests trip over is already written and already good — whatever is done should reuse it rather than restate it.

Found while working i-0029, and kept out of it: that issue is about a model choice, and this is about the machine the gate runs on.
