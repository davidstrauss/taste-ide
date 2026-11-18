# Lock order

Every std lock in the workspace is `tracing_mutex::stdsync`'s `Mutex` or
`RwLock` (`clippy.toml` refuses std's own). In a debug build each one
takes part in one graph: whenever a thread takes a lock while holding
another, that is an edge from the held lock to the new one, and a lock
whose edge would close a cycle is refused where it is taken. A cycle in
that graph is every deadlock two or more locks can make, so the check
finds one **without the deadlock having to happen**: the two orders may
run minutes apart, on different threads, in different tests of one
binary. A lock taken again by the thread already holding it is the
shortest cycle there is, and is refused the same way. A release build
gets std's own types and pays nothing.

## Why

On 2026-11-17 two functions of `ide_exec`'s job registry took the
registry's lock and a job's in opposite orders, and two of an agent's own
polls of one command each took one (80fe82e6). The test suite had run
both functions for weeks. It never failed, because a deadlock needs the
two orders to interleave, and in a test they never did. Ordering is what
a test can check reliably, so the check is on ordering.

The first run found one more such bug. `ExecContext` resolved an agent's
command while holding a read of its target, and then read the target
again inside that. std's `RwLock` on Linux makes a second read wait behind
a writer that is already waiting. So a retarget (every reload makes one)
that came between the two reads blocked the resolve forever, and the
retarget with it. The agent's commands, its terminals, and its tasks all
resolve through there. A resolve now reads the target once and works from
the copy (`ExecContext::target`).

## Where it bites

- **Tests** panic at the lock that closes the cycle, so `cargo test` is
  the gate. `crates/taste-core/tests/lock_order.rs` takes the registry and
  the job in the two orders from before 80fe82e6, one after the other so
  that nothing can deadlock, and requires the second to be refused. A test
  build that stopped tracing fails there instead of passing everything
  unchecked.
- **The debug IDE**, the build that is run day to day, logs a cycle
  rather than panicking (`taste-app`'s `lock_order.rs`). A cycle is a
  deadlock that has not happened yet, and a panic on a GTK callback aborts
  the process now. The log line is `lock order:`, with the stack that
  closed the cycle, at most eight a run.
- To see where each edge of a cycle was first made, and not only where it
  closed, run with `RUST_LIB_BACKTRACE=1`. The check then keeps a
  backtrace for every edge the first time it is made.

## What it does not see

- **Orders that never run.** It checks what was executed, by a test or by
  the debug IDE in use. It does not prove anything about code that
  neither has run.
- **Instances, not kinds.** A lock is a node of its own. Two jobs' locks
  taken in opposite orders are four nodes, not a cycle. The registry
  bug was caught because it was one job's lock both times.
- **tokio's locks** (`tokio::sync::Mutex` and `RwLock`, a dozen uses).
  Holding one across an await while waiting on another is a deadlock of
  tasks, and nothing traces those.
- **A cycle on a thread nobody joins.** A detached thread or a tokio task
  whose handle is dropped panics alone. Its test usually fails anyway,
  because the work it was doing never finished, but not always. Run with
  `--nocapture` and grep for `Found cycle` when a test passes but its log
  is in doubt.
- **Other crates' locks**: GLib's, tokio's internals, and those of any
  dependency.

`docs/MCP-CALLS.md` is the other half. This check is meant to prevent a
call from wedging in the first place; that page covers what a wedged call
does to the rest of its connection, and keeps it bounded.
