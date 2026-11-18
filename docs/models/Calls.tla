------------------------------- MODULE Calls -------------------------------
(***************************************************************************)
(* How the IDE's MCP server answers one connection's tool calls            *)
(* (crates/taste-mcp/src/server.rs, `handle_connection`), and the promise  *)
(* it makes: every call the agent makes gets an answer, or an error,       *)
(* within its deadline, whatever the calls beside it do (docs/MCP-CALLS.md *)
(* says why, and what each switch below was).                              *)
(*                                                                         *)
(* A call is sent, read, waits for one of the connection's permits, and    *)
(* runs its work. Work may finish, may never finish while still giving     *)
(* its thread back at every await (a channel nobody answers), or may       *)
(* block the thread it runs on outright (a std lock that two threads hold  *)
(* in opposite orders, as `ide_exec`'s registry did on 2026-11-17). Time   *)
(* is a clock of ticks; reading a call, handing it a free permit, and      *)
(* firing a due deadline are urgent, so time stands still while any of     *)
(* them can happen, and passes a deadline only where nothing can answer.   *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Calls,          \* the calls one connection sends
    Permits,        \* how many calls' work may run at once (MAX_IN_FLIGHT)
    Workers,        \* the runtime's worker threads
    Deadline,       \* a call's bound, in ticks (TOOL_WATCHDOG)
    MaxTime,        \* where the clock stops, to keep the model finite
    Answerer,       \* where a call's deadline is kept, and so who answers
                    \* at it: "work", inside the work's own future (as it
                    \* was); "beside", a task of its own on the workers the
                    \* work runs on too; "apart", on the workers while the
                    \* work runs on threads of its own
    FromRead,       \* whether the deadline counts from the read (TRUE) or
                    \* only from the permit (FALSE, as it was)
    SlotWithWork    \* whether a call's permit stays with its work after the
                    \* call is answered, until the work is gone

ASSUME Answerer \in {"work", "beside", "apart"}
ASSUME FromRead \in BOOLEAN /\ SlotWithWork \in BOOLEAN

VARIABLES
    now,    \* the clock
    phase,  \* each call: "unsent", "sent", "waiting", "running", "answered"
    work,   \* each call's work: "none", "going", "blocked", "done", "dropped"
    sent,   \* when each call was sent: the agent's clock starts there
    armed,  \* whether a deadline is kept for the call yet
    since,  \* when its deadline started counting
    slot    \* whether it holds a permit

vars == <<now, phase, work, sent, armed, since, slot>>

Blocked == {c \in Calls : work[c] = "blocked"}
Held == {c \in Calls : slot[c]}

\* A worker is free for the runtime's tasks: the read loop, the timers,
\* and — but where they run apart — the calls' work. Work blocked on its own
\* threads takes none.
WorkerFree == Answerer = "apart" \/ Cardinality(Blocked) < Workers

\* The work can take a step: on its own thread apart, on a worker otherwise.
WorkRuns == Answerer = "apart" \/ Cardinality(Blocked) < Workers

TypeOK ==
    /\ now \in 0..MaxTime
    /\ phase \in [Calls -> {"unsent", "sent", "waiting", "running", "answered"}]
    /\ work \in [Calls -> {"none", "going", "blocked", "done", "dropped"}]
    /\ sent \in [Calls -> 0..MaxTime]
    /\ armed \in [Calls -> BOOLEAN]
    /\ since \in [Calls -> 0..MaxTime]
    /\ slot \in [Calls -> BOOLEAN]

Init ==
    /\ now = 0
    /\ phase = [c \in Calls |-> "unsent"]
    /\ work = [c \in Calls |-> "none"]
    /\ sent = [c \in Calls |-> 0]
    /\ armed = [c \in Calls |-> FALSE]
    /\ since = [c \in Calls |-> 0]
    /\ slot = [c \in Calls |-> FALSE]

\* The agent sends a call, whatever the IDE is doing.
Send(c) ==
    /\ phase[c] = "unsent"
    /\ phase' = [phase EXCEPT ![c] = "sent"]
    /\ sent' = [sent EXCEPT ![c] = now]
    /\ UNCHANGED <<now, work, armed, since, slot>>

\* The read loop, a task on the workers, reads it.
CanRead(c) == phase[c] = "sent" /\ WorkerFree
Read(c) ==
    /\ CanRead(c)
    /\ phase' = [phase EXCEPT ![c] = "waiting"]
    /\ armed' = [armed EXCEPT ![c] = FromRead]
    /\ since' = [since EXCEPT ![c] = now]
    /\ UNCHANGED <<now, work, sent, slot>>

\* It takes a permit, and its work starts: at once when one is free, as a
\* semaphore hands it over.
CanTake(c) == phase[c] = "waiting" /\ WorkerFree /\ Cardinality(Held) < Permits
Take(c) ==
    /\ CanTake(c)
    /\ phase' = [phase EXCEPT ![c] = "running"]
    /\ work' = [work EXCEPT ![c] = "going"]
    /\ slot' = [slot EXCEPT ![c] = TRUE]
    /\ armed' = [armed EXCEPT ![c] = TRUE]
    /\ since' = IF FromRead THEN since ELSE [since EXCEPT ![c] = now]
    /\ UNCHANGED <<now, sent>>

\* The work finishes: the call is answered with what it did, if nothing
\* answered it first, and its permit comes back.
Finish(c) ==
    /\ work[c] = "going"
    /\ WorkRuns
    /\ work' = [work EXCEPT ![c] = "done"]
    /\ phase' = IF phase[c] = "running"
                THEN [phase EXCEPT ![c] = "answered"] ELSE phase
    /\ slot' = [slot EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<now, sent, armed, since>>

\* The work blocks the thread it is on, for good.
Block(c) ==
    /\ work[c] = "going"
    /\ WorkRuns
    /\ work' = [work EXCEPT ![c] = "blocked"]
    /\ UNCHANGED <<now, phase, sent, armed, since, slot>>

\* The call's deadline is due, and whatever keeps it can run: inside the
\* work, only while the work is not blocked and has a worker to run on;
\* beside it, while a worker is free; apart, always.
CanTime(c) ==
    /\ phase[c] \in {"waiting", "running"}
    /\ armed[c]
    /\ now - since[c] >= Deadline
    /\ CASE Answerer = "work"   -> work[c] /= "blocked" /\ WorkerFree
         [] Answerer = "beside" -> WorkerFree
         [] Answerer = "apart"  -> TRUE

\* At the deadline the call is answered with an error. Work that is going
\* is dropped where it stands, and gives its permit back with it; work that
\* is blocked cannot be dropped, and keeps its permit or not as the design
\* has it.
Time(c) ==
    /\ CanTime(c)
    /\ phase' = [phase EXCEPT ![c] = "answered"]
    /\ work' = IF work[c] = "going"
               THEN [work EXCEPT ![c] = "dropped"] ELSE work
    /\ slot' = IF work[c] = "blocked" /\ SlotWithWork
               THEN slot ELSE [slot EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<now, sent, armed, since>>

\* Time passes, but not over a read, a permit's handover, or a deadline
\* that can happen now.
Tick ==
    /\ now < MaxTime
    /\ \A c \in Calls : ~CanRead(c) /\ ~CanTake(c) /\ ~CanTime(c)
    /\ now' = now + 1
    /\ UNCHANGED <<phase, work, sent, armed, since, slot>>

Next ==
    \/ Tick
    \/ \E c \in Calls :
        Send(c) \/ Read(c) \/ Take(c) \/ Finish(c) \/ Block(c) \/ Time(c)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* What is checked.                                                        *)

\* Every call the agent sent is answered, or still within its deadline as
\* the agent counts it: from the send.
AnsweredInTime ==
    \A c \in Calls :
        phase[c] \in {"sent", "waiting", "running"} => now - sent[c] <= Deadline

\* What one connection can take from the IDE is bounded: no more of its
\* work blocked than it has permits, whatever is answered meanwhile.
Contained == Cardinality(Blocked) <= Permits

Symmetry == Permutations(Calls)
=============================================================================
