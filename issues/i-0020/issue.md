---
title: The subscription gauge belongs beside the context gauge in the chat, not above the backlog
state: open
reporter: primary
created: 2026-09-10T07:12:14Z
updated: 2026-09-10T07:12:14Z
labels: ui, chat, backlog, quota
---

The subscription meter sits in the Backlog header, beside the refresh button.
Hovering it gives the whole picture — "Session window 34% used, resets in 20 d
16 h · Weekly window 48% used, resets in 3 d 22 h · Read off the last agent
turn, just now. · One pool: every environment here, and your own Claude use." —
but it is nowhere near the other thing measuring the same spend.

David, 2026-09-10: *"This should not be the indicator above backlog. It should
be in the chat area near the context window indicator."*

## Where it is now

`crates/taste-app/src/backlog.rs` owns it: the `quota` container and `quota_bar`
gauge, `draw_quota` at 1645-1663, and `set_quota` at 1637. It is fed through
`filetree.rs:1666`, which forwards to `self.backlog.set_quota(snapshot)`, from
`window.rs:1877`.

## Where it goes

The chat header's `usage_box` (`chat.rs:1401-1403`), beside `usage_bar`. Its
sibling is `set_context_gauge` at `chat.rs:3499`.

Two things make this cheaper than it looks. The gauges are already the same
widget by intent — the comment at `chat.rs:1396-1400` says the context bar is
"the same gauge the environments panel draws for the subscription window
(`crate::gauge`) — one width, traffic-light colours". And `quota_tooltip` is
already `pub(crate)` (`backlog.rs:85`), so the text moves without being
rewritten.

## The judgement to get right

The two gauges measure different scopes. The context window is this chat's; the
subscription pool is the workspace's and the user's own Claude use besides.
Adjacent and identical, they will read as one measurement in two halves.
Whatever distinguishes them has to be legible without hovering — order, a
separator, an icon, or a short label. There is already an icon for the concept:
`data/icons/hicolor/scalable/status/taste-utilization-symbolic.svg`. Pick one,
and say in a comment why, so the next reader does not tidy the distinction away.

Second: the chat header is per chat, and the quota is global. Drawn there, the
same number repeats in every chat tab. That may be right — it is where the user
is looking — but decide it rather than inherit it, and consider whether it
belongs once above the tabs instead.

## What comes out of the backlog side

The widget, its `RefCell<String>` tooltip cache, and the `draw_quota` call in
`tick` (`backlog.rs:1634`). Check that the Backlog header still composes with
only the refresh button left in that slot, and that `filetree.rs`'s forwarding
has no other purpose once the backlog no longer wants it.

The chat's Utilization tab already lists Session window and Weekly window rows
(`chat.rs:3647-3664`). Those are the breakdown, and the header tooltip points at
the tab; leave them, and do not turn this into a second place the same numbers
are maintained.

## Related

**i-0018** — the session figure is currently mis-attributed, which is why the
tooltip above says 20 d 16 h for a five-hour window. Do not chase that here; it
is a parser bug in `taste-authproxy` and it will make the new gauge look wrong
until it lands.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. This moves a widget between two panes
that are both held to the highest bar, so pose both and look:
`TASTE_PROBE_CHECK=1 TASTE_PROBE_VIEW=orchestrator` with `TASTE_PROBE_CHAT=busy`,
and `TASTE_PROBE_VIEW=backlog` for the header it left. Run
`build-aux/headless/near-miss.py` on the dump — this adds an edge to a header
row, which is exactly the defect that tool exists for — and `TASTE_MEASURE_MIN=1`
to confirm a second gauge has not raised the chat pane's minimum width. Oxford
commas in everything written. Commit per verified batch; never push.
