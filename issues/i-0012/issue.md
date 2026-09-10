---
title: Enter mid-composition sends a truncated message from Dispatch
state: declined
reporter: primary
created: 2026-09-10T06:08:56Z
updated: 2026-09-10T06:59:01Z
labels: bug, input, compose
---

While an input method is composing, Enter commits the composition; it does not
end the sentence. Dispatch does not know that, so it sends — and what goes out
is the message cut off mid-word, with the composition lost and no way to get it
back. Every CJK user hits this every time they type.

## Where it is

`compose.rs:436`:

```rust
Key::Return | Key::KP_Enter if !shift => {
    compose.dispatch(Destination::Chat);
    glib::Propagation::Stop
}
```

No preedit guard, and `grep preedit crates/taste-app/src/compose.rs` finds
nothing at all. Dispatch is the universal composer and the only composer on
screen, so this is the live path for Chat, Backlog, and Commit alike.

The rule is already written down, just not where it can help: `composer_key` in
`chat.rs:971-983` states it exactly, for a chat-side composer that is not on
screen.

```rust
/// The preedit rule is the subtle one. While an input method is composing —
/// … Enter COMMITS the composition; it does not end the sentence. Sending
/// there truncates the message mid-word and there is no way to get it back.
if state.preedit {
    return ComposerKey::Passthrough;
}
```

## The fix

Dispatch's entry tracks preedit the way `chat.rs:2018-2021` does —
`connect_preedit_changed`, an empty string meaning composition ended — and its
Return arm returns `glib::Propagation::Proceed` while a preedit is live, so the
input method gets the key.

Note that GtkTextView announces preedit changes and an empty string is the end
of one; `chat.rs:2014` has the comment explaining it. Read that before writing
the new one.

## Test

A unit test over the key decision, the way `enter_belongs_to_the_input_method_mid_preedit`
(`chat.rs:9002`) covers it today. Dispatch's Return handling is a closure inside
`Compose::new` rather than a pure function, so extracting the decision into one
the test can call is part of the work — do that rather than reaching for a
widget test.

## Relationship to i-0008

i-0008 removes the chat-specific composer and `composer_key` with it, which
deletes the codebase's only statement of this rule. This issue must land first;
i-0008 has been told to hold that removal until it does.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, and `cargo test --workspace`. Oxford commas in everything written.
Commit per verified batch; never push.
