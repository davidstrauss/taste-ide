------------------------------- MODULE SyncGit -------------------------------
(***************************************************************************)
(* The sync with git in it (docs/SYNC.md, layer 4): Sync.tla's pass, and  *)
(* around it what the sync asks of git — commits flowing between the two  *)
(* sides, Personal carried onto the folder's commits through a stash,     *)
(* each side following the other's branch switch — with git's own rules   *)
(* for what a fast-forward, a switch, a stash, and a stash's pop do to a  *)
(* working tree: a tracked file, an untracked one, and one the side's     *)
(* rules ignore, which git's defaults count expendable.                   *)
(*                                                                         *)
(* The actors — the user in the folder, the agent in Personal — edit      *)
(* files, commit, switch branches as git does, and change what their side *)
(* ignores, at every step of every pass; a pass may stop at any step.     *)
(* Every value an actor writes is new, so the ledger can tell whether     *)
(* any was destroyed. A commit, once made, is kept (nothing here rewrites *)
(* history), and a stash entry while it is listed: both are places a      *)
(* version is held.                                                        *)
(*                                                                         *)
(* Two switches put the behaviour git and the sync's scripts once had     *)
(* into the same model, so the checker shows what each fix buys:          *)
(* ProtectIgnored — a fast-forward or a switch refuses rather than        *)
(* overwrite an ignored file — and Cleanup — what a stash that would not  *)
(* pop leaves: "old", reset --hard and git clean; "hardreset", reset      *)
(* --hard and only the untracked files the pop restored; "precise", only  *)
(* the paths the stash touched, and only where they hold what the pop     *)
(* wrote.                                                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS Paths, Fresh, Branches, MaxValue, MaxCommits, MaxSwitches,
          MaxToggles, MaxCrashes, ProtectIgnored, Cleanup
          \* Fresh: paths no commit has yet, absent on both sides at first

Side == {"F", "C"}
NoChange == -1
Marker == -2  \* a conflict's markers: no actor's value

VARIABLES wt,        \* each side's working tree: Paths -> value (0 absent)
          head,      \* each side's branch
          refs,      \* each side's branches: Branches -> commit (0 none)
          Tree,      \* each commit's tree
          Parent,    \* each commit's parent (0 none)
          nc,        \* the next commit's number
          ignored,   \* the paths each side's rules ignore
          stash,     \* Personal's stash: a sequence of [base, chg]
          A,         \* the archive: the working copy the two last agreed on
          P,         \* the branch the two last agreed on
          pc,        \* the pass: "idle", "push", "position", "observe", "apply"
          pa,        \* the push's step: "none", "start", "stashed", "merged"
          target,    \* the commit the push carries Personal to
          stashed,   \* whether the push stashed
          todo, landed, agreed,  \* the mirror's writes, as in Sync.tla
          said,      \* the pass ended on something said to the user
          next, crashes, switches, toggles, done,
          written, replaced

vars == <<wt, head, refs, Tree, Parent, nc, ignored, stash, A, P, pc, pa, target,
          stashed, todo, landed, agreed, said, next, crashes, switches, toggles,
          done, written, replaced>>

Commits == 1..MaxCommits

Init == /\ wt = [s \in Side |-> [p \in Paths |-> IF p \in Fresh THEN 0 ELSE 1]]
        /\ head = [s \in Side |-> "main"]
        /\ refs = [s \in Side |-> [b \in Branches |-> IF b = "main" THEN 1 ELSE 0]]
        /\ Tree = [c \in Commits |-> [p \in Paths |-> IF c = 1 /\ p \notin Fresh THEN 1 ELSE 0]]
        /\ Parent = [c \in Commits |-> 0]
        /\ nc = 2
        /\ ignored = [s \in Side |-> {}]
        /\ stash = <<>>
        /\ A = [p \in Paths |-> IF p \in Fresh THEN 0 ELSE 1]
        /\ P = "main"
        /\ pc = "idle"
        /\ pa = "none"
        /\ target = 0
        /\ stashed = FALSE
        /\ todo = {}
        /\ landed = {}
        /\ agreed = {}
        /\ said = FALSE
        /\ next = 2
        /\ crashes = 0
        /\ switches = 0
        /\ toggles = 0
        /\ done = FALSE
        /\ written = {}
        /\ replaced = {}

(***************************************************************************)
(* Git, as it treats a working tree.                                       *)
(***************************************************************************)

H(s) == refs[s][head[s]]
Tracked(s, p) == Tree[H(s)][p] # 0
Modified(s, p) == Tracked(s, p) /\ wt[s][p] # Tree[H(s)][p]
Untracked(s, p) == ~Tracked(s, p) /\ wt[s][p] # 0 /\ p \notin ignored[s]
IgnoredFile(s, p) == ~Tracked(s, p) /\ wt[s][p] # 0 /\ p \in ignored[s]

RECURSIVE Ancestor(_, _)
Ancestor(a, b) == a = b \/ (Parent[b] # 0 /\ Ancestor(a, Parent[b]))

\* Whether git moves `s`'s working tree from commit f to commit t: no path
\* that changes between them has a change of the side's own, or an untracked
\* file in the way — or an ignored one, when told to protect those.
CanMove(s, f, t, protect) ==
    \A p \in Paths : Tree[f][p] # Tree[t][p] =>
        /\ ~(Tree[f][p] # 0 /\ wt[s][p] # Tree[f][p])
        /\ ~(Tree[f][p] = 0 /\ wt[s][p] # 0 /\ (p \notin ignored[s] \/ protect))

Moved(s, f, t) == [p \in Paths |-> IF Tree[f][p] # Tree[t][p] THEN Tree[t][p] ELSE wt[s][p]]

(***************************************************************************)
(* The actors.                                                             *)
(***************************************************************************)

Edit(s, p) ==
    /\ ~done
    /\ next <= MaxValue
    /\ replaced' = replaced \cup {<<p, next, wt[s][p]>>}
    /\ written' = written \cup {<<p, next>>}
    /\ wt' = [wt EXCEPT ![s][p] = next]
    /\ next' = next + 1
    /\ UNCHANGED <<head, refs, Tree, Parent, nc, ignored, stash, A, P, pc, pa, target,
                   stashed, todo, landed, agreed, said, crashes, switches, toggles, done>>

\* git add -A and git commit: what is tracked, and what is untracked and not
\* ignored, as the working tree has it.
Commit(s) ==
    /\ ~done
    /\ nc <= MaxCommits
    /\ LET t == [p \in Paths |-> IF ~Tracked(s, p) /\ p \in ignored[s] THEN 0 ELSE wt[s][p]]
       IN /\ t # Tree[H(s)]
          /\ Tree' = [Tree EXCEPT ![nc] = t]
          /\ Parent' = [Parent EXCEPT ![nc] = H(s)]
          /\ refs' = [refs EXCEPT ![s][head[s]] = nc]
          /\ nc' = nc + 1
    /\ UNCHANGED <<wt, head, ignored, stash, A, P, pc, pa, target, stashed, todo, landed,
                   agreed, said, next, crashes, switches, toggles, done, written, replaced>>

\* git switch, as an actor runs it: git's default, which overwrites an
\* ignored file in the way — the actor's own act, recorded as theirs.
Switch(s, b) ==
    /\ ~done
    /\ switches < MaxSwitches
    /\ b # head[s]
    /\ LET t == IF refs[s][b] # 0 THEN refs[s][b] ELSE H(s)
       IN /\ CanMove(s, H(s), t, FALSE)
          /\ replaced' = replaced \cup
                {<<p, Tree[t][p], wt[s][p]>> : p \in {q \in Paths : IgnoredFile(s, q) /\ Tree[t][q] # 0}}
          /\ written' = written \cup
                {<<p, Tree[t][p]>> : p \in {q \in Paths : IgnoredFile(s, q) /\ Tree[t][q] # 0}}
          /\ wt' = [wt EXCEPT ![s] = Moved(s, H(s), t)]
          /\ refs' = [refs EXCEPT ![s][b] = t]
          /\ head' = [head EXCEPT ![s] = b]
    /\ switches' = switches + 1
    /\ UNCHANGED <<Tree, Parent, nc, ignored, stash, A, P, pc, pa, target, stashed, todo,
                   landed, agreed, said, next, crashes, toggles, done>>

\* A side's rules change: a path ignored there, or no longer.
Toggle(s, p) ==
    /\ ~done
    /\ toggles < MaxToggles
    /\ ignored' = [ignored EXCEPT ![s] = IF p \in ignored[s] THEN ignored[s] \ {p} ELSE ignored[s] \cup {p}]
    /\ toggles' = toggles + 1
    /\ UNCHANGED <<wt, head, refs, Tree, Parent, nc, stash, A, P, pc, pa, target, stashed,
                   todo, landed, agreed, said, next, crashes, switches, done, written, replaced>>

\* The actors stop, for the convergence property.
Stop == /\ ~done
        /\ done' = TRUE
        /\ UNCHANGED <<wt, head, refs, Tree, Parent, nc, ignored, stash, A, P, pc, pa, target,
                       stashed, todo, landed, agreed, said, next, crashes, switches, toggles,
                       written, replaced>>

(***************************************************************************)
(* The sync's commit protocol (peer::sync_primary_peer_with).              *)
(***************************************************************************)

\* The checked-out branch, both sides on it: the side behind follows. The
\* folder behind follows at once (the mirror moves its branch; its files
\* are the mirror's to decide). The folder ahead is carried into Personal
\* by the push below, or — Personal's working tree the folder's already —
\* by moving the branch alone. Diverged is said.
CommitsStep ==
    /\ pc = "idle"
    /\ IF head["F"] # head["C"]
       THEN /\ pc' = "position" /\ said' = FALSE
            /\ UNCHANGED <<refs, pa, target>>
       ELSE LET b == head["F"]
                f == refs["F"][b]
                c == refs["C"][b]
            IN CASE f = c ->
                      /\ pc' = "position" /\ said' = FALSE
                      /\ UNCHANGED <<refs, pa, target>>
                 [] Ancestor(f, c) ->
                      /\ refs' = [refs EXCEPT !["F"][b] = c]
                      /\ pc' = "position" /\ said' = FALSE /\ UNCHANGED <<pa, target>>
                 [] Ancestor(c, f) /\ wt["C"] = wt["F"] ->
                      /\ refs' = [refs EXCEPT !["C"][b] = f]
                      /\ pc' = "idle" /\ said' = FALSE /\ UNCHANGED <<pa, target>>
                 [] Ancestor(c, f) ->
                      /\ pc' = "push" /\ pa' = "start" /\ target' = f /\ said' = FALSE
                      /\ UNCHANGED refs
                 [] OTHER ->
                      \* Diverged: said, and left to the user.
                      /\ pc' = "idle" /\ said' = TRUE /\ UNCHANGED <<refs, pa, target>>
    /\ UNCHANGED <<wt, head, Tree, Parent, nc, ignored, stash, A, P, stashed, todo, landed,
                   agreed, next, crashes, switches, toggles, done, written, replaced>>

\* git stash push --include-untracked: tracked changes and untracked files
\* that are not ignored; an ignored file stays where it is.
PushStash ==
    /\ pc = "push" /\ pa = "start"
    /\ LET dirty == \E p \in Paths : Modified("C", p) \/ Untracked("C", p)
           e == [base |-> H("C"),
                 chg |-> [p \in Paths |-> IF Modified("C", p) \/ Untracked("C", p)
                                           THEN wt["C"][p] ELSE NoChange]]
       IN IF dirty
          THEN /\ stash' = Append(stash, e)
               /\ wt' = [wt EXCEPT !["C"] = [p \in Paths |->
                           IF Modified("C", p) THEN Tree[H("C")][p]
                           ELSE IF Untracked("C", p) THEN 0 ELSE wt["C"][p]]]
               /\ stashed' = TRUE
          ELSE /\ stashed' = FALSE /\ UNCHANGED <<stash, wt>>
    /\ pa' = "stashed"
    /\ UNCHANGED <<head, refs, Tree, Parent, nc, ignored, A, P, pc, target, todo, landed,
                   agreed, said, next, crashes, switches, toggles, done, written, replaced>>

\* git stash pop of the newest entry onto Personal's HEAD now. Refused
\* outright where a path it touches has a change of Personal's own;
\* otherwise each path applies, or — changed upstream as well — conflicts,
\* and a conflict fails the pop with the entry kept. What a failed pop
\* leaves is the script's to clean (Cleanup).
PopResult(e) ==
    LET t == H("C")
        local == \E p \in Paths : e.chg[p] # NoChange /\ wt["C"][p] # Tree[t][p]
        clash(p) == e.chg[p] # NoChange /\ Tree[t][p] # Tree[e.base][p]
        applied == [p \in Paths |-> IF e.chg[p] = NoChange THEN wt["C"][p]
                                    ELSE IF clash(p) THEN Marker ELSE e.chg[p]]
        ok == ~local /\ \A p \in Paths : ~clash(p)
        after == IF local THEN wt["C"] ELSE applied
        cleaned ==
          CASE Cleanup = "old" ->
                 \* reset --hard, then git clean -fd
                 [p \in Paths |-> IF Tree[t][p] # 0 THEN Tree[t][p]
                                  ELSE IF p \notin ignored["C"] THEN 0 ELSE after[p]]
            [] Cleanup = "hardreset" ->
                 \* reset --hard, then only untracked files the pop restored
                 [p \in Paths |-> IF Tree[t][p] # 0 THEN Tree[t][p]
                                  ELSE IF e.chg[p] # NoChange /\ after[p] = e.chg[p] THEN 0
                                  ELSE after[p]]
            [] OTHER ->
                 \* only what the stash touched, where it holds the pop's own
                 [p \in Paths |-> IF e.chg[p] # NoChange /\ (after[p] = e.chg[p] \/ after[p] = Marker)
                                  THEN Tree[t][p] ELSE after[p]]
    IN IF ok THEN <<TRUE, applied>> ELSE <<FALSE, cleaned>>

\* git merge --ff-only (--no-overwrite-ignore when ProtectIgnored) to the
\* folder's commit; refused, the stash is popped back where it was.
PushMerge ==
    /\ pc = "push" /\ pa = "stashed"
    /\ LET b == head["C"]
       IN IF H("C") # target /\ Ancestor(H("C"), target) /\ CanMove("C", H("C"), target, ProtectIgnored)
          THEN /\ wt' = [wt EXCEPT !["C"] = Moved("C", H("C"), target)]
               /\ refs' = [refs EXCEPT !["C"][b] = target]
               /\ pa' = "merged" /\ UNCHANGED <<pc, stash, said>>
          ELSE \* Refused: the stash is popped back where it came from, and a
               \* pop that fails there is left as git leaves it — the script
               \* runs no cleanup on this path (`git stash pop || true`).
               /\ IF stashed
                  THEN LET r == PopResult(stash[Len(stash)])
                       IN IF r[1]
                          THEN /\ wt' = [wt EXCEPT !["C"] = r[2]]
                               /\ stash' = SubSeq(stash, 1, Len(stash) - 1)
                          ELSE UNCHANGED <<wt, stash>>
                  ELSE UNCHANGED <<wt, stash>>
               /\ pa' = "none" /\ pc' = "idle" /\ said' = TRUE
               /\ UNCHANGED refs
    /\ UNCHANGED <<head, Tree, Parent, nc, ignored, A, P, target, stashed, todo, landed,
                   agreed, next, crashes, switches, toggles, done, written, replaced>>

PushPop ==
    /\ pc = "push" /\ pa = "merged"
    /\ IF stashed
       THEN LET r == PopResult(stash[Len(stash)])
            IN /\ wt' = [wt EXCEPT !["C"] = r[2]]
               /\ stash' = IF r[1] THEN SubSeq(stash, 1, Len(stash) - 1) ELSE stash
       ELSE UNCHANGED <<wt, stash>>
    /\ pa' = "none" /\ pc' = "idle"
    /\ UNCHANGED <<head, refs, Tree, Parent, nc, ignored, A, P, target, stashed, todo, landed,
                   agreed, said, next, crashes, switches, toggles, done, written, replaced>>

(***************************************************************************)
(* The position: each side follows the other's switch.                     *)
(***************************************************************************)

PositionStep ==
    /\ pc = "position"
    /\ CASE head["F"] = head["C"] ->
              /\ pc' = "observe" /\ UNCHANGED <<wt, head, refs, P, said>>
         [] head["F"] # P /\ head["C"] = P ->
              \* The folder switched: Personal follows, refused over a
              \* change of its own to a tracked file (git diff), a branch of
              \* its own the folder's does not contain, or a file in the way.
              LET b == head["F"]
                  t == refs["F"][b]
                  mine == refs["C"][b]
              IN IF /\ \A p \in Paths : ~Modified("C", p)
                    /\ (mine = 0 \/ Ancestor(mine, t))
                    /\ CanMove("C", H("C"), t, ProtectIgnored)
                 THEN /\ wt' = [wt EXCEPT !["C"] = Moved("C", H("C"), t)]
                      /\ refs' = [refs EXCEPT !["C"][b] = t]
                      /\ head' = [head EXCEPT !["C"] = b]
                      /\ P' = b
                      /\ pc' = "idle" /\ UNCHANGED said
                 ELSE /\ pc' = "idle" /\ said' = TRUE
                      /\ UNCHANGED <<wt, head, refs, P>>
         [] head["C"] # P /\ head["F"] = P ->
              \* Personal switched: the folder follows — its branch and
              \* HEAD, its files the mirror's — unless its copy of the branch
              \* holds commits Personal's lacks.
              LET b == head["C"]
                  t == refs["C"][b]
                  mine == refs["F"][b]
              IN IF mine = 0 \/ Ancestor(mine, t)
                 THEN /\ refs' = [refs EXCEPT !["F"][b] = t]
                      /\ head' = [head EXCEPT !["F"] = b]
                      /\ P' = b
                      /\ pc' = "observe" /\ UNCHANGED <<wt, said>>
                 ELSE /\ pc' = "idle" /\ said' = TRUE
                      /\ UNCHANGED <<wt, head, refs, P>>
         [] OTHER ->
              \* Both moved, differently: said.
              /\ pc' = "idle" /\ said' = TRUE /\ UNCHANGED <<wt, head, refs, P>>
    /\ UNCHANGED <<Tree, Parent, nc, ignored, stash, A, pa, target, stashed, todo, landed,
                   agreed, next, crashes, switches, toggles, done, written, replaced>>

(***************************************************************************)
(* The mirror (Sync.tla), on one branch at one commit: each side's scope   *)
(* its own rules, a path in it where neither ignores it.                    *)
(***************************************************************************)

InScope(p) == p \notin ignored["F"] /\ p \notin ignored["C"]

Writes(f, c) ==
    { [side |-> "C", p |-> p, val |-> f[p], expect |-> c[p]]
        : p \in {q \in Paths : InScope(q) /\ f[q] # A[q] /\ c[q] = A[q]} }
    \cup
    { [side |-> "F", p |-> p, val |-> c[p], expect |-> f[p]]
        : p \in {q \in Paths : InScope(q) /\ f[q] = A[q] /\ c[q] # A[q]} }

Observe ==
    /\ pc = "observe"
    /\ IF H("F") = H("C") /\ head["F"] = head["C"]
       THEN /\ todo' = Writes(wt["F"], wt["C"])
            /\ agreed' = { <<p, wt["F"][p]>> : p \in {q \in Paths :
                             InScope(q) /\ wt["F"][q] # A[q] /\ wt["F"][q] = wt["C"][q]} }
            /\ pc' = "apply"
       ELSE /\ pc' = "idle" /\ UNCHANGED <<todo, agreed>>
    /\ landed' = {}
    /\ UNCHANGED <<wt, head, refs, Tree, Parent, nc, ignored, stash, A, P, pa, target, stashed,
                   said, next, crashes, switches, toggles, done, written, replaced>>

Apply(w) ==
    /\ pc = "apply"
    /\ w \in todo
    /\ todo' = todo \ {w}
    /\ IF wt[w.side][w.p] = w.expect
       THEN /\ wt' = [wt EXCEPT ![w.side][w.p] = w.val]
            /\ landed' = landed \cup {w}
       ELSE UNCHANGED <<wt, landed>>
    /\ UNCHANGED <<head, refs, Tree, Parent, nc, ignored, stash, A, P, pc, pa, target, stashed,
                   agreed, said, next, crashes, switches, toggles, done, written, replaced>>

ApplyAny == \E w \in todo : Apply(w)

Finish ==
    /\ pc = "apply" /\ todo = {}
    /\ A' = [p \in Paths |->
               IF \E w \in landed : w.p = p THEN (CHOOSE w \in landed : w.p = p).val
               ELSE IF \E g \in agreed : g[1] = p THEN (CHOOSE g \in agreed : g[1] = p)[2]
               ELSE A[p]]
    /\ pc' = "idle" /\ landed' = {} /\ agreed' = {}
    /\ UNCHANGED <<wt, head, refs, Tree, Parent, nc, ignored, stash, P, pa, target, stashed,
                   todo, said, next, crashes, switches, toggles, done, written, replaced>>

\* A pass stopped at any step: what it did stays; a stash it made stays
\* listed; the archive records nothing.
Crash ==
    /\ pc # "idle"
    /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ pc' = "idle" /\ pa' = "none" /\ todo' = {} /\ landed' = {} /\ agreed' = {}
    /\ UNCHANGED <<wt, head, refs, Tree, Parent, nc, ignored, stash, A, P, target, stashed,
                   said, next, switches, toggles, done, written, replaced>>

Next == \/ \E s \in Side, p \in Paths : Edit(s, p) \/ Toggle(s, p)
        \/ \E s \in Side : Commit(s)
        \/ \E s \in Side, b \in Branches : Switch(s, b)
        \/ Stop
        \/ CommitsStep \/ PushStash \/ PushMerge \/ PushPop \/ PositionStep
        \/ Observe \/ ApplyAny \/ Finish \/ Crash

Fairness == /\ WF_vars(CommitsStep) /\ WF_vars(PushStash) /\ WF_vars(PushMerge)
            /\ WF_vars(PushPop) /\ WF_vars(PositionStep) /\ WF_vars(Observe)
            /\ WF_vars(ApplyAny) /\ WF_vars(Finish)

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Where a version is held: a working tree, a commit, a listed stash entry.
Held(p, v) == \/ \E s \in Side : wt[s][p] = v
              \/ \E c \in 1..(nc - 1) : Tree[c][p] = v
              \/ \E i \in 1..Len(stash) : stash[i].chg[p] = v

RECURSIVE Live(_, _)
Live(p, v) == \/ Held(p, v)
              \/ \E r \in replaced : r[1] = p /\ r[3] = v /\ r[2] # v /\ r[2] > v /\ Live(p, r[2])

\* P1: no version an actor wrote is destroyed by the sync.
Safety == \A w \in written : Live(w[1], w[2])

\* Nothing either side holds is a conflict's markers.
NoMarkers == \A s \in Side, p \in Paths : wt[s][p] # Marker

\* P4: once the actors stop, the sync comes to rest either agreed — one
\* branch, one commit, and every path in scope the same or a conflict — or
\* having said why not.
Conflict(p) == wt["F"][p] # A[p] /\ wt["C"][p] # A[p] /\ wt["F"][p] # wt["C"][p]
Agreed == /\ head["F"] = head["C"]
          /\ H("F") = H("C")
          /\ \A p \in Paths : ~InScope(p) \/ wt["F"][p] = wt["C"][p] \/ Conflict(p)
Settled == pc = "idle" /\ (Agreed \/ said)
Converges == done ~> Settled

=============================================================================
