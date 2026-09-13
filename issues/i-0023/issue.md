---
title: The Dispatch composer emits a gdk_popup_present CRITICAL on nearly every keystroke
state: open
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: claude-sonnet-5
created: 2026-09-13T03:25:13Z
updated: 2026-09-13T03:25:52Z
labels: bug, ui, compose, gtk
---

David, 2026-09-12, typing into Dispatch: `Gdk-CRITICAL **: gdk_popup_present: assertion 'width > 0' failed`, repeating — 15 in 30 seconds of one session's log, roughly one per typing burst, starting the moment the window came up.

The zero-width completion popup is already known: `crates/taste-app/src/command_completion.rs:105-125` has `populate` return `NotFound` instead of an empty model precisely to avoid this, and says so in a comment that names the assertion — *"a gdk_popup_present CRITICAL on every ordinary keystroke (interactive completion populates for ANY word, not just slash commands)"*. That guard is in the running build and the CRITICAL happens anyway, so the remaining path is elsewhere.

Two candidates, both to be checked rather than assumed:

1. **`refilter` takes the other route.** `command_completion.rs:131-138` handles zero matches with `set_proposals_for_provider(..., None)` rather than the error `populate` returns. That is the path a reader hits while typing — the second character onwards — rather than on the first.
2. **Dispatch shares the chat pane's provider object.** `compose.rs:520-532` adds the very same `CommandProvider` instance to Dispatch's `GtkSourceCompletion` while it is still registered with that chat's own entry (`chat.rs:1442`). One provider serving two completions, each with its own context calling `set_proposals_for_provider` on it. This would explain why it shows up in Dispatch specifically.

Not cosmetic: a zero-size popup surface is something a Wayland compositor may kill the whole app over, per the note already in that file.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus a reproduction that fails before and passes after — the app run with `G_DEBUG=fatal-criticals` while text is typed into Dispatch, so the assertion is a crash a test can catch rather than a line in a log nobody reads. Confirm afterwards that the slash-command list still opens, filters, and completes in both composers: the chat's own entry and Dispatch. Oxford commas in everything written. Commit per verified batch; never push.
