---
title: An unrecognised subscription window is reported as the session window
state: open
reporter: primary
created: 2026-09-10T06:58:55Z
updated: 2026-09-10T06:58:55Z
labels: bug, quota, authproxy
---

The header reads "Session window · 27% used · resets in 20 d 17 h". The
subscription's session window is a rolling five hours, so both numbers belong
to some other window; nothing on screen says so.

## Where it is

`crates/taste-authproxy/src/quota.rs:319`:

```rust
let weekly = named_weekly || (!named_session && unnamed == Represented::Weekly);
```

`plan_field` runs for every `anthropic-ratelimit-*` family whose name contains
`unified`, `plan`, or `subscription` — the call sites at lines 203, 212, 216,
and 220 pass whatever family they stripped, with no test of whether it named a
window at all. `named_weekly` recognises `week`, `7d`, `seven_day`,
`seven-day`, and an `<n>d` token; `named_session` recognises `5h`, `session`,
`five_hour`, and `five-hour`. A family that matches neither — a monthly window,
an Opus-specific one, an extra-usage one — falls through to `unnamed`, which is
`Represented::Session` whenever the representative claim says `five_hour`, and
writes itself into `snapshot.session`. Last header wins, so a window three
weeks out replaces the real five-hour one.

The `unnamed` fallback is right for the family it was written for: the one
carrying no window name, which
`anthropic-ratelimit-unified-representative-claim` speaks for (the reasoning is
at lines 260-266). It is wrong for a family that named a window we do not
recognise. Those are different cases and the code cannot currently tell them
apart.

The display side is not at fault: `chat.rs:3648` renders
`("Session window", &snapshot.session)`, and the label is correct for the slot.

## The fix

Distinguish "no window in the name" from "a window we do not know". Only the
first takes the representative claim; the second is not forced into either slot
and goes to `QuotaSnapshot::other`, which line 296 already describes as where an
unrecognised family belongs. A window nobody can attribute is better shown as
unattributed than shown under the wrong heading — `console.rs:3297` states that
preference already ("wrong rather than to look comfortable").

## Evidence to capture first

The real header set is not recorded anywhere in the repo, so the first step is
to see it: `crates/taste-acp/tests/live_proxy.rs:230` prints the parsed
snapshot, and the unaccounted-for headers land in `QuotaSnapshot::other`.
Whatever family produced the 20 d 17 h reset should be named in a test fixture,
so the parser is pinned against the shape that actually shipped rather than a
guess.

## Tests

A parser test per case: a named-but-unrecognised subscription family does not
touch `session` or `weekly`; the unnamed family still follows the representative
claim in both of its directions.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. Oxford commas in everything written.
Commit per verified batch; never push.
