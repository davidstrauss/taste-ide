---
title: Pressing ESC from dispatch shouldn't stop the chat agent
state: open
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: sonnet
created: 2026-09-08T16:17:27Z
updated: 2026-09-10T06:07:48Z
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

## What must not be lost

`composer_key` holds a rule Dispatch does not have, and it is the one thing in
that block worth keeping:

```rust
if state.preedit {
    return ComposerKey::Passthrough;
}
```

While an input method is composing — every CJK user, every time they type —
Enter commits the composition; it does not end the sentence. Dispatch's
`Key::Return | Key::KP_Enter` arm at `compose.rs:436` has no such guard, and
`grep preedit crates/taste-app/src/compose.rs` finds nothing, so a message typed
into the box that is actually on screen is sent truncated mid-word with no way
to get it back. Deleting `composer_key` would delete the codebase's only
statement of the rule.

So carry it over as part of this removal: Dispatch's entry tracks preedit the
way `chat.rs:2019` does, and its Return arm returns `Propagation::Proceed` while
a preedit is live. Cover it with a test.

## Tests

`escape_stops_only_while_streaming` (`chat.rs:9032`) and
`escape_denies_the_permission_card_before_it_stops_the_turn` (`chat.rs:9051`)
describe behavior that is going away, and they come out with it.
`enter_belongs_to_the_input_method_mid_preedit` (`chat.rs:9002`) describes
behavior that is *moving*, so move it to `compose.rs` against Dispatch rather
than deleting it.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. The chat pane and the prompt box are
held to the highest bar in the app, so pose the result and look at it:
`TASTE_PROBE_CHECK=1 TASTE_PROBE_VIEW=orchestrator`. Oxford commas in
everything written. Commit per verified batch; never push.
