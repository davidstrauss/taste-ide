-------------------------------- MODULE Sync --------------------------------
(***************************************************************************)
(* The folder-and-Personal sync's pass, as a protocol (docs/SYNC.md,       *)
(* layer 4): two working copies, an archive of what they last agreed on,  *)
(* and a pass that observes both, decides every path by the table, writes *)
(* each decision to its side, and records what landed — with the actors   *)
(* editing either side at every step, and a pass that may stop partway.   *)
(*                                                                         *)
(* What it leaves out is what the planner's exhaustive test covers (kinds *)
(* of entry, scope): here a path holds a value, every value an actor      *)
(* writes is new, and a removal is one more new value. What it is for is  *)
(* the interleavings, which no test samples completely and TLC visits    *)
(* every one of.                                                          *)
(*                                                                         *)
(* Two switches put the sync as it was before 2026-10-12 into the same    *)
(* model, so the checker can show what each part of the redesign buys:    *)
(* CAS, whether a write checks its side at the write; and Fresh, whether  *)
(* a pass observes the checkout itself rather than through a snapshot     *)
(* some other moment took.                                                *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Paths,      \* the paths, a small set
          MaxValue,   \* the last value an actor may write: bounds the edits
          MaxCrashes, \* how many passes may stop partway
          CAS,        \* writes check their side at the write
          Fresh       \* a pass observes the checkout itself

VARIABLES F,          \* the folder's working copy: Paths -> value
          C,          \* the checkout's
          A,          \* the archive: what the two last agreed on
          Snap,       \* the checkout's newest snapshot, which a pass reads if not Fresh
          pc,         \* "idle" or "apply"
          todo,       \* the pass's writes not yet attempted
          landed,     \* the pass's writes that landed
          agreed,     \* the pass's agreements: <<path, value>>
          next,       \* the next value an actor writes
          crashes,    \* passes stopped so far
          written,    \* every <<path, value>> an actor wrote
          replaced    \* every <<path, new, old>>: an actor's value and what it replaced

vars == <<F, C, A, Snap, pc, todo, landed, agreed, next, crashes, written, replaced>>

Side == {"F", "C"}

Init == /\ F = [p \in Paths |-> 1]
        /\ C = [p \in Paths |-> 1]
        /\ A = [p \in Paths |-> 1]
        /\ Snap = [p \in Paths |-> 1]
        /\ pc = "idle"
        /\ todo = {}
        /\ landed = {}
        /\ agreed = {}
        /\ next = 2
        /\ crashes = 0
        /\ written = {}
        /\ replaced = {}

(***************************************************************************)
(* The actors.                                                             *)
(***************************************************************************)

EditF(p) == /\ next <= MaxValue
            /\ replaced' = replaced \cup {<<p, next, F[p]>>}
            /\ written' = written \cup {<<p, next>>}
            /\ F' = [F EXCEPT ![p] = next]
            /\ next' = next + 1
            /\ UNCHANGED <<C, A, Snap, pc, todo, landed, agreed, crashes>>

EditC(p) == /\ next <= MaxValue
            /\ replaced' = replaced \cup {<<p, next, C[p]>>}
            /\ written' = written \cup {<<p, next>>}
            /\ C' = [C EXCEPT ![p] = next]
            /\ next' = next + 1
            /\ UNCHANGED <<F, A, Snap, pc, todo, landed, agreed, crashes>>

\* Something other than the pass snapshots the checkout: the VM's watcher,
\* a chat turn's end, a commit.
TakeSnapshot == /\ Snap # C
                /\ Snap' = C
                /\ UNCHANGED <<F, C, A, pc, todo, landed, agreed, next, crashes, written, replaced>>

(***************************************************************************)
(* The pass.                                                               *)
(***************************************************************************)

\* The decision table (docs/SYNC.md), for observed copies f and c.
Writes(f, c) ==
    { [side |-> "C", p |-> p, val |-> f[p], expect |-> c[p]]
        : p \in {q \in Paths : f[q] # A[q] /\ c[q] = A[q]} }
    \cup
    { [side |-> "F", p |-> p, val |-> c[p], expect |-> f[p]]
        : p \in {q \in Paths : f[q] = A[q] /\ c[q] # A[q]} }

Agreements(f, c) ==
    { <<p, f[p]>> : p \in {q \in Paths : f[q] # A[q] /\ f[q] = c[q]} }

Observe == /\ pc = "idle"
           /\ LET c == IF Fresh THEN C ELSE Snap
              IN /\ todo' = Writes(F, c)
                 /\ agreed' = Agreements(F, c)
           /\ landed' = {}
           /\ pc' = "apply"
           /\ UNCHANGED <<F, C, A, Snap, next, crashes, written, replaced>>

\* One write: lands if its side still holds what the pass observed — or,
\* without CAS, regardless.
Apply(w) ==
    /\ pc = "apply"
    /\ w \in todo
    /\ todo' = todo \ {w}
    /\ LET holds == IF w.side = "F" THEN F[w.p] ELSE C[w.p]
           lands == ~CAS \/ holds = w.expect
       IN /\ landed' = IF lands THEN landed \cup {w} ELSE landed
          /\ F' = IF lands /\ w.side = "F" THEN [F EXCEPT ![w.p] = w.val] ELSE F
          /\ C' = IF lands /\ w.side = "C" THEN [C EXCEPT ![w.p] = w.val] ELSE C
    /\ UNCHANGED <<A, Snap, pc, agreed, next, crashes, written, replaced>>

\* The archive, per path: each write that landed, each agreement.
Finish == /\ pc = "apply"
          /\ todo = {}
          /\ A' = [p \in Paths |->
                     IF \E w \in landed : w.p = p
                     THEN (CHOOSE w \in landed : w.p = p).val
                     ELSE IF \E g \in agreed : g[1] = p
                          THEN (CHOOSE g \in agreed : g[1] = p)[2]
                          ELSE A[p]]
          /\ pc' = "idle"
          /\ todo' = {}
          /\ landed' = {}
          /\ agreed' = {}
          /\ UNCHANGED <<F, C, Snap, next, crashes, written, replaced>>

\* A pass stopped partway: what landed stays, the archive records nothing.
Crash == /\ pc = "apply"
         /\ crashes < MaxCrashes
         /\ crashes' = crashes + 1
         /\ pc' = "idle"
         /\ todo' = {}
         /\ landed' = {}
         /\ agreed' = {}
         /\ UNCHANGED <<F, C, A, Snap, next, written, replaced>>

ApplyAny == \E w \in todo : Apply(w)

Next == \/ \E p \in Paths : EditF(p) \/ EditC(p)
        \/ TakeSnapshot
        \/ Observe
        \/ ApplyAny
        \/ Finish
        \/ Crash

\* The sync keeps running, and something keeps snapshotting the checkout;
\* an actor need never act, and a pass need never stop partway.
Fairness == /\ WF_vars(Observe)
            /\ WF_vars(ApplyAny)
            /\ WF_vars(Finish)
            /\ WF_vars(TakeSnapshot)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

TypeOK == /\ F \in [Paths -> 1..MaxValue]
          /\ C \in [Paths -> 1..MaxValue]
          /\ A \in [Paths -> 1..MaxValue]
          /\ pc \in {"idle", "apply"}

\* A value is live at p if a side holds it, or an actor replaced it with a
\* value that is live: so every value an actor wrote is either still held
\* or was knowingly replaced.
RECURSIVE Live(_, _)
Live(p, v) == \/ F[p] = v
              \/ C[p] = v
              \/ \E r \in replaced : r[1] = p /\ r[3] = v /\ r[2] > v /\ Live(p, r[2])

\* P1: the sync destroys no value that exists nowhere else.
Safety == \A w \in written : Live(w[1], w[2])

\* P4: once the actors are done and no pass is under way, every path the
\* two do not agree on is one both changed since the archive — a conflict
\* — and the pass that finds it so writes nothing.
Conflict(p) == F[p] # A[p] /\ C[p] # A[p] /\ F[p] # C[p]
Settled == pc = "idle" /\ \A p \in Paths : F[p] = C[p] \/ Conflict(p)
Converges == (next > MaxValue) ~> Settled

\* P4: a pass over settled copies plans no write — the sync's own writes
\* never start another round (P6: what it wrote is agreement).
QuietIsQuiet == (Fresh /\ Settled) => Writes(F, C) = {}

=============================================================================
