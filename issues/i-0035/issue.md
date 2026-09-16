---
title: An exhausted host inotify budget is a fleet-wide condition the coordinator should say out loud, not a WARN line
state: open
reporter: i-0030
created: 2026-09-16T02:09:01Z
updated: 2026-09-16T02:09:01Z
labels: fleet, host, orchestrator, ux
---

Split out of i-0030, whose scope was the test gate. i-0030 made the four (in fact nine) watcher and config-watcher tests recognise `fs.inotify.max_user_instances` exhaustion and report *skipped, host limit reached* instead of failing, and named the sysctl in the README. What it deliberately did not do is the issue body's second outcome:

> the IDE says it once, where it matters: the exhaustion is a fleet-wide condition, and it is currently a WARN line in a log. A machine that can no longer watch files is something the coordinator should be able to tell the user about (CLAUDE.md → "supervision happens at the orchestrator level").

**Why it is worth doing.** Measured from inside i-0030 on 2026-09-16 at 02:05 with four environments live: `free: 0` — the host's entire budget of 128 instances was spent, sampled every 30s for twenty minutes without once recovering. In that state:

- `WatchSlot::aim` logs `watching … failed` and leaves the slot empty, so a watched environment's pane silently stops reflecting its own files (`crates/taste-core/src/watcher.rs`, the `Err` arm of `aim`);
- `ConfigWatch::add` fails, so an environment stops noticing `.devcontainer/` drift — and drift is what gates `devcontainer_reload`;
- nothing anywhere says so to the user. `crates/taste-devcontainer/src/configwatch.rs` already calls this out for the old per-supervisor watcher ("a silent failure … no banner, no toast"), and the condition it describes is back, one layer up.

The IDE cannot fix it — raising the limit is a host sysctl, and CLAUDE.md's boundary means the IDE does not reconfigure the user's machine — so the whole of the work is *saying* it, once, in the place the user is actually looking.

**Where it would go.** The error text already exists and is already good (`taste_core::watcher::name_the_inotify_limit`), and `taste_core::watcher::INSTANCE_LIMIT_SYSCTL` is now the string that makes an exhaustion recognisable after the error has been flattened into an `anyhow::Error` (`is_the_host_inotify_limit` in the same module is the private predicate the tests use; it would want to become public or move). The surface is the coordinator's transcript, which today renders only the coordinator's own acts (`act_kind`, `crates/taste-app/src/chat.rs`) — so this needs a representation for "something happened to the machine", which is the same gap CLAUDE.md notes for an environment that fails to build or blocks on a permission prompt. Worth designing once for all three rather than three times.

**Done looks like** the first watcher failure of this kind reaching the user once — not per environment, not per retry — with the sysctl named and the number from README → "From stock Silverblue to self-hosting", and going quiet again when the budget recovers.
