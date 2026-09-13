---
title: A popup outlives the window at quit: "Tried to unmap the parent of a popup"
state: open
reporter: primary
created: 2026-09-13T18:58:07Z
updated: 2026-09-13T18:58:07Z
labels: bug, ui, gtk, shutdown
---

David, 2026-09-13, and it happens on **every** shutdown:

```
(taste-ide:259056): Gdk-WARNING **: 11:40:53.672: Tried to unmap the parent of a popup
```

GDK emits this when a toplevel is hidden while a popup surface is still mapped under it. The effect today is one line at exit, so this is cosmetic in itself; the reason to fix it is that a popup outliving its parent is a teardown-order defect, and the codebase already knows how that ends — `backlog.rs:2563`'s `close_context_menu` pops a menu down *and* unparents it "now, not on the closed signal's idle", because "a row finalized with a popover still parented to it is a crash (GTK says so, then segfaults)". The same shape at window teardown is the same bug with a luckier outcome.

## What is established

**It is the dying process, not the starting one.** The warning is stamped 18:40:53 UTC and the replacement process's first log line is 18:40:57 — the IDE had been up since 01:13 local. `ide_app_log` is per-process, so the old process's own log went with it and the message quoted above is all that survives of it.

**It is not the slash-command completion popup.** `sh build-aux/headless/typing.sh` exits 0 with zero warnings both with the popup closed at quit (the default burst, which ends in `/cozz` so nothing matches) and with it deliberately left open (`sh build-aux/headless/typing.sh 'The clone comes up. /co'`). Sourceview pops its completion down on unmap correctly. Do not re-litigate this one.

**Nothing headless covers the quit path at all.** `window.rs:3622-3645` installs `connect_close_request` only when `!probe_mode`, so every probe renders, gets shot, and exits without ever taking the real close path. That is why the typing gate is clean, and it is a gap in its own right: there is currently no automated run in which this warning *could* have appeared.

**Neither the backlog nor the history has it.** `git log -S'unmap' --all` names four commits, none about this.

## What is left

The popup is something mapped at essentially every quit, which is a much narrower claim than "a popover was open". The candidates, in the order they are worth checking:

- **GTK's own tooltips.** `hover.rs` installs `full_text_on_hover` on every ellipsized label in the app, and a tooltip needs only a resting pointer — including a pointer resting on the header's close button, which is where it is when the window closes. This is the only candidate that explains "every time" without further assumptions.
- **The `popdown()` sites**, none of which run on window close: `backlog.rs:2568`, `console.rs:3492`, `:3500`, `:3508`, `:3519`, `:3529`, `editor.rs:1791` and `:1865` (`mode_popover`), `filetree.rs:3190`, `:3839`, `:4063`, `:4088` (`branch_popover`), and `pages_menu.rs:125`.

## A caveat on the method

`TASTE_LOG_BACKTRACE=1` is the tool that names which popup, since the message never does and there is no gdb in the devcontainer. It did **not** yield a backtrace for this warning. Find out why before trusting it on the next teardown bug: a warning emitted during GTK teardown may arrive after the log writer is torn down, or on a path the writer does not see, and if so that is a second defect worth its own fix — the flag was merged (i-0023) precisely so shutdown-time GTK complaints could be attributed.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus:

- a regression test that exercises the **real** close path with the offending popup up, since no probe does today;
- whatever closes the gap in the harness, so a GTK warning at quit is an exit code rather than a line the user reads to us — `typing.sh` is the precedent, and `G_DEBUG=fatal-warnings` on a shutdown run is the obvious shape.

Note that `build-aux/headless/typing.sh` is in `main` as mode 100644, so invoke it as `sh build-aux/headless/typing.sh` unless that has been fixed by the time you read this. Oxford commas in everything written. Commit per verified batch; never push.
