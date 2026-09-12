---
title: The environment cap counts clones on disk, not running environments
state: open
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
created: 2026-09-10T06:12:18Z
updated: 2026-09-12T09:54:54Z
labels: bug, orchestration, mcp
---

`issue_start` refuses at the cap once six agent environments *exist*, whether or
not any of them is running. An environment flagged for review is finished and
its container is stopped, but it still holds a slot, so a workspace where every
piece of work is done and merged cannot start anything new until the user goes
and destroys clones by hand.

Hit today: three environments flagged for review and fully merged (`ahead: 0`),
one working, and `issue_start i-0012` refused with "this workspace already has 7
agent environments".

## Where it is

`crates/taste-mcp/src/server.rs:2612-2626`:

```rust
// 2. The resource cap. Counted from the registry — the clones on
//    disk are the inventory of record — and refused with what to
//    do about it.
let live = self
    .environments
    .ids()
    .into_iter()
    .filter(|id| !id.is_primary())
    .count();
if live >= environment::MAX_ORCHESTRATED_ENVIRONMENTS {
```

The binding is named `live` and the refusal says "already has {live} agent
environments", but the count is registry entries with only the primary filtered
out. Container state is never consulted. `MAX_ORCHESTRATED_ENVIRONMENTS = 6`
lives at `crates/taste-core/src/environment.rs:49`.

## Why the current count is the wrong one

The refusal justifies itself by naming what an environment costs: "a clone, a
container, an agent process, and a share of the user's subscription". Three of
those four — the container, the process, and the subscription share — are
released the moment the environment stops. Only the clone survives, and disk is
the cheapest of the four and the one least in need of a hard stop at six. So the
number being enforced is not the number the justification is about.

## The fix

Count environments that are actually running. The fleet snapshot `issue_list`
already renders as `runtime.state` and `runtime.light` carries that fact, so it
is available on this side without new IO — use it rather than shelling out to
podman on the request path.

Two things to get right beyond the count itself:

- **The check belongs wherever an environment starts running, not only in
  `issue_start`.** If a stopped environment can be brought back up, restarting
  one while six others run walks straight past the cap. Find the restart path
  and gate it the same way, or state in the code why it cannot happen.
- **`issue_list`'s own report should agree.** It currently returns
  `environments: 7, cap: 6`, which is the same registry count and reads as
  though the workspace is already over its limit. Report the running count
  against the cap, and if the total is still worth showing, show it as a
  distinct field rather than as the one being capped.

## Open question for the user, not for the implementer

Nothing would bound total clones on disk after this change. That may be fine —
disk is cheap and the fleet view lists them — or it may want a second, much
looser ceiling. Do not invent one; ask, and if the answer is "leave it", say so
in a comment where the cap is enforced so the next reader knows it was
considered rather than missed.

## Tests

`issue_start_stops_at_the_environment_cap` (`server.rs:5113`) builds its
environments with `environments.create(id)` and no containers at all, so under
the corrected rule it would stop testing the cap and start passing for the wrong
reason. It needs environments that are *running* to trigger the refusal, plus a
new sibling test asserting the opposite: with the cap's worth of environments
present but stopped, `issue_start` succeeds.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. Oxford commas in everything written.
Commit per verified batch; never push.
