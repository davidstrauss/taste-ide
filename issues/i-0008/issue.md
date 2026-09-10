---
title: Pressing ESC from dispatch shouldn't stop the chat agent
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-08T16:17:27Z
updated: 2026-09-10T07:25:24Z
---

Escape pressed in Dispatch does nothing to the chat. It does not stop a
streaming turn, and it does not deny a permission card. Nothing leaves the
composer box.

Dispatch is the universal composer — Chat, Backlog, and Commit share the one
field — and it is the only composer on screen; the chat pane has no box of its
own. A key pressed in it should not reach across into whatever pane happens to
be selected. Stop and Deny stay where they are visible: their buttons.

## What comes out

1. Compose's Escape hook: the `Key::Escape` arm at `compose.rs:440`, the
   `on_escape` field at `compose.rs:311`, and `set_on_escape` at
   `compose.rs:494`. Escape gets no arm at all — it propagates, so the
   completion popup and the window keep whatever they already do with it.
2. The wiring at `window.rs:800-804`, which is the hook's only caller.
3. `ChatPane::escape()` at `chat.rs:5484`. `window.rs:803` is its only caller;
   with the hook gone it is dead.
4. The chat-specific composer and its keymap: `composer_key`
   (`chat.rs:976-1011`), `ComposerKey`, `ComposerState`, the preedit cell at
   `chat.rs:507` and `chat.rs:1818`, the `connect_preedit_changed` at
   `chat.rs:2019`, and the call site at `chat.rs:2042`. There should not be a
   second composer at this point, and there should not be a second keymap
   describing one. Confirm it is genuinely unreachable first — if some
   responsive rung or view still packs it, stop and say so rather than tearing
   it out.

The doc comment at `compose.rs:421-425` still describes the old rule; rewrite
it to the new one.

## Item 4 waits on i-0012

`composer_key` holds a preedit guard that Dispatch does not have, and deleting
the block would take the codebase's only statement of that rule with it. i-0012
moves the rule to where it belongs, in Dispatch. **Land i-0012 first, then
remove item 4.** Items 1 through 3 and the test work below do not depend on it
and can go in their own batch now.

## Tests

`escape_stops_only_while_streaming` (`chat.rs:9032`) and
`escape_denies_the_permission_card_before_it_stops_the_turn` (`chat.rs:9051`)
describe behavior that is going away, and they come out with it.
`enter_belongs_to_the_input_method_mid_preedit` (`chat.rs:9002`) belongs to
i-0012 — leave it alone here; it goes when item 4 goes.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. The chat pane and the prompt box are
held to the highest bar in the app, so pose the result and look at it:
`TASTE_PROBE_CHECK=1 TASTE_PROBE_VIEW=orchestrator`. Oxford commas in
everything written. Commit per verified batch; never push.
