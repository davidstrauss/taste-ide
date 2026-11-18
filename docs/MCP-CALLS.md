# How the IDE answers an agent's tool calls

The IDE's MCP server (`crates/taste-mcp/src/server.rs`, `handle_connection`)
makes one promise to every agent: **every call it makes gets an answer, or
an error, within its deadline, whatever the calls beside it do.** This page
describes the design that keeps it, and `docs/models/Calls.tla` checks it
with TLC (`build-aux/check-models.sh`).

## Why it is a promise

On 2026-11-17 remise's agent saw its shell commands hang, and then its file
edits. Two functions of `ide_exec`'s job registry took the same two locks
in opposite orders, and two of the agent's own polls of one command took
one each (`taste_mcp::exec`, fixed in 80fe82e6). That was a bug, and a
lock-order bug can come back in any std lock. What made it an outage was
the design around it:

- **The deadline was kept inside the work.** `TOOL_WATCHDOG` was a
  `tokio::time::timeout` around the call's own future. A future that blocks
  its thread outright is never polled again, so its timer never fired, and
  neither did the user's Stop, which was another branch of the same select.
- **The deadline counted from the permit.** A connection runs eight calls
  at once (`MAX_IN_FLIGHT`). A ninth waited for a permit with no bound. So
  once eight calls were wedged, every later call on the connection waited
  forever, the edits among them, even though nothing else was wrong.
- **The work ran on the runtime's workers**, the same threads that read the
  socket and fire every timer. Each blocked call took one of them.

## The design

- **The deadline counts from the read.** A call waits for a permit at most
  `QUEUE_WAIT` (25 s). If it gets none, it is refused, not started, and the
  refusal says nothing was done. That bound fits inside `TOOL_WATCHDOG`
  (150 s) by more than the longest any tool bounds itself (120 s, a test
  holds the sum), so a call that starts late still has the time its tool
  promises.
- **The deadline is kept apart from the work.** A call's own task waits on
  its answer with a timer, on the workers. The work runs on a thread of its
  own (`spawn_blocking`, driving the work's future with the runtime's
  handle). Work that blocks its thread takes only that thread, and the
  call is answered as overrunning at its deadline all the same. A Stop or a
  cancel answers it in the same way.
- **The permit stays with the work.** At the deadline, work that is still
  awaiting is dropped where it stands, and its permit comes back with it.
  Work that is blocked cannot be dropped, and keeps its permit until it
  ends. So what one connection can take from the IDE stays bounded:
  never more blocked threads than it has permits, however many of its calls
  are answered meanwhile. Calls made while all eight are held are refused
  after `QUEUE_WAIT`, each one promptly and honestly.

## The model

`docs/models/Calls.tla` models one connection. Calls are sent, read, wait
for a permit, and run. Their work may finish, may never finish while still
awaiting, or may block its thread for good. Time is a clock of ticks.
Reading a call, handing it a free permit, and firing a due deadline are
urgent, so time passes a deadline only where nothing is able to answer.
TLC checks two invariants:

- **AnsweredInTime**: every call the agent sent is answered, or is still
  within its deadline as the agent counts it, from the send.
- **Contained**: no more of the connection's work is blocked than it has
  permits.

Three switches restore what the design replaced, plus one alternative that
looks sufficient but is not. `build-aux/check-models.sh` holds each
configuration to its outcome:

| Configuration     | What it is                                                          | Outcome                |
|-------------------|---------------------------------------------------------------------|------------------------|
| `Calls`           | the design as built                                                 | holds                  |
| `CallsAsWas`      | the deadline inside the work, from the permit: the remise hang      | breaks AnsweredInTime  |
| `CallsFromPermit` | the deadline apart, but from the permit: queued behind slow work    | breaks AnsweredInTime  |
| `CallsBeside`     | the deadline a task beside the work, on the same workers            | breaks AnsweredInTime  |
| `CallsFreeSlot`   | the permit given back at the answer rather than with the work       | breaks Contained       |

`CallsBeside` is the one that looks sufficient. A separate task for the
timer is enough while fewer pieces of work are blocked than there are
workers. Nothing in the IDE keeps that true: each connection may block
eight, and there is a connection per agent. In the model, two blocked calls
on two workers leave nothing to read the next call or fire its timer.
`CallsFreeSlot` shows why the permit is not given back at the answer. The
connection would keep answering, but each answered call would leave its
blocked thread behind, until the workers were gone.

The checked universe is four calls, two permits, three workers (two for
`CallsBeside`), and a deadline of three ticks: about 125,000 distinct
states for `Calls`, in seconds, and fewer still before each of the others
finds its trace.

The model is tied to the code by
`every_call_is_answered_by_its_deadline_whatever_its_work_does` in
`server.rs`. It blocks every permit's thread outright on a runtime of two
workers. It then checks that the queued call is refused, that the blocked
calls are answered at their deadline rather than at their work's end, that
a call while the permits are held is refused, and that the permits come
back with the work. Run with the work on the workers instead, the test
fails as `CallsBeside` does.

## What it leaves out

- **One connection.** Connections share the runtime's workers and the
  blocking pool's threads (512), and the model has one. With the work
  apart, a connection's blocked work takes none of the workers, so others
  are affected only through the blocking pool. That needs 64 connections
  each wedged eightfold before it is exhausted.
- **The read loop's admission.** A connection reads at most
  `MAX_IN_FLIGHT * 2` calls ahead of their answers. Every admitted call is
  answered by its deadline, so admission frees up within one deadline. A
  call sent past that cap waits on the read, and its wait is not in the
  model.
- **A call that becomes an MCP task** (`call_as_task`, the stateless
  revision's tasks extension). It answers on time, because its promotion
  is bounded by `tasks::PROMOTE_AFTER`. But the work it promotes runs on
  as a task on the workers, so a blocked promoted task does take a worker.
- **Everything else on the runtime.** The design keeps a call's own work
  off the workers. A std lock that blocks in any other task on the runtime
  still takes a worker, and enough of those would stop every timer,
  including these. The model's promise covers what a call's work does,
  not the rest of the IDE. That rest is why a lock-order bug is still a bug.
