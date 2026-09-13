---
title: issue_start has no opposite: environment destroy and issue delete on the MCP surface
state: open
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
created: 2026-09-13T03:25:00Z
updated: 2026-09-13T03:25:26Z
labels: mcp, environments, backlog, orchestration
---

The coordinator can create but not remove. `issue_create` files, `issue_start` clones a checkout and starts a container — and nothing on the MCP surface undoes either. Every reclaim is a hand-off: the coordinator certifies a list and the user clicks Destroy once per environment in the panel. That is backwards for the one participant that holds the disk budget, sees `disk.used_bytes` against `MAX_ORCHESTRATED_DISK_BYTES` on every `issue_list`, and is the reason those ceilings were written (i-0013).

David, 2026-09-13: *"Add environment/issue removal to the MCP surface."*

## What already exists

Both backends. `EnvironmentRegistry::destroy()` removes the clone, the container, and the environment's volumes, and hands back a report — `removed_volumes`, `unpublished`, `dirty_files`, `had_unsaved_work()`; `console.rs:2492-2552` is its only caller today. `GitWorkspace::issue_delete` removes an issue, its comments, and its place in the order, and errors on a second delete (`taste-git/src/issues.rs`, and the test at ~2244 that asserts no file of a deleted issue survives). Neither needs writing. What needs writing is the surface, and the two things below.

## The part that is not plumbing: who forgets

`run_destroy` does not just call the registry. It then makes the rest of the app forget: `console::forget_environment` (`console.rs:3598`), the review board's `forget`, `git_facts` / `claim_facts` / `review_facts` / `disk_facts`, the lifecycle sink, the selection falling back to `primary`, and — through their own paths — `chats::forget_environment` (`chats.rs:766`), `editor::forget_environment` (`editor.rs:1255`), and `taste_core::state::forget_environment` (`state.rs:222`). That fan-out is spelled out inline in one GTK function, and a second caller that reproduces it will drift from it.

So decide, and write down why: lift the fan-out into one thing both callers invoke, or have the MCP path publish an event the app consumes and keep the fan-out where the widgets are. The second is the shape the rest of this codebase uses (`EventBus`, GTK objects never leave the main thread) and it also answers what happens when the destroy is requested while the panel has that row selected.

## The part that is not plumbing: what replaces the dialog

The confirmation is not ceremony. `destroy_intervention` (`console.rs:2368-2490`) reads the clone *before* offering the button and says what is in it — unpublished branches with their commit counts and summaries, uncommitted file counts, which chat lives there and what it loses, and that volumes go too. An agent calling a tool sees none of that.

Proposed, to be argued with rather than implemented blindly:

- **Refuse by default when the clone holds work nobody else has** — unpublished commits or dirty files — and return the same enumeration the dialog shows, as data. `force: true` is how the caller says it read that and meant it. This mirrors `issue_update`'s completion gate, which already refuses on facts rather than taking the caller's word.
- **Never `primary`, and never the caller's own environment.** An agent destroying the clone it is running in is a live foot-gun, and the id is attached at accept time (ENVIRONMENTS.md → "MCP: the socket is the identity"), so the server can refuse it without asking anyone.
- **`issue_delete` refuses an issue whose environment still exists**, naming it. Two objects, two acts, in an order that cannot orphan a clone whose issue is gone.
- **Deleting is not how work gets closed.** `declined` exists precisely so a decision survives the thing decided against (README → the backlog). The tool description says so, the way the others do.
- Settle whether these route through the IDE's permission prompt. They are destructive and irreversible; `devcontainer_reload` already asks when config has drifted, and the same argument applies more strongly here.

## Gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Tests in `taste-mcp` for each refusal — unpublished work without `force`, `primary`, the caller's own environment, an issue whose environment is still there — and one that the forget fan-out happens exactly once however the destroy was requested. `docs/ENVIRONMENTS.md` gains the removal half beside the start half, and `issue_list`'s note, which currently explains that stopping frees nothing, gains the thing that does. Oxford commas in everything written. Commit per verified batch; never push.
