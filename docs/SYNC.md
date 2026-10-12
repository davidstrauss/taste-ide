# The folder and Personal: what the sync must do

The primary environment's checkout lives in a VM, and the folder the user
opened is its peer: each mirrors the other's working copy, both ways
(ENVIRONMENTS → "The topology", `taste_git::mirror`,
`taste_devcontainer::peer`). This document is the sync's specification: the
model it works in, the properties it must hold under every interleaving of
edits on both sides, and the design measured against them. A change to the
sync is checked against it, and a test of the sync tests one of its
properties by name.

It follows the formal specification of Unison (Balasubramaniam and Pierce,
*What is a File Synchronizer?*, 1998; Pierce and Vouillon, *What's in
Unison?*, 2004), whose state-based design — two replicas and an archive of
what they last agreed on — is the one this sync already had, and which says
precisely what "never loses work" means for it. Written 2026-10-12, after a
day of sync bugs that were each a property below being broken in a way
nobody had written down (David: "Is there any comprehensive/rigorous test
suite or set of theoretical models we could consider to make sync truly
robust for all scenarios?").

## The model

**Two replicas.** F is the folder on this host; C is the checkout in the VM.
Each replica's *working copy* is a map from paths to entries, where an entry
is a file (its bytes and its executable bit) or a symbolic link (its
target). A path absent from the map is absent. Directories are not entries:
a directory exists because something is beneath it, as git has it.

**Scope.** A path is *in scope* on a side unless that side's rules ignore it
and do not sync it (`.gitignore`, and the `taste-sync` attribute that brings
an ignored path back, `taste_git::sync_ignored`). Out of scope is out of the
sync entirely: never read as a change, never written, never removed.

**The archive.** A records, per path, the entry both sides last agreed on
(absent included), and the position — branch and commit — they last agreed
on. It is the base each side's changes are measured from, and the only
memory the sync has.

**Observation.** A pass *observes* both replicas: it reads each one's
working copy as it stands. F is read directly. C is read in the VM, as a
snapshot (`taste_git::snapshot`), which is C at the instant it was taken.

**Changes.** For a path p, a side X has *changed* p when X(p) ≠ A(p).

**The decision.** For each path in scope:

| F changed | C changed | F(p) = C(p) | The pass                                     |
|-----------|-----------|-------------|----------------------------------------------|
| no        | no        | —           | nothing                                      |
| yes       | no        | —           | C takes F(p)                                 |
| no        | yes       | —           | F takes C(p)                                 |
| yes       | yes       | yes         | nothing written; A takes the agreed entry    |
| yes       | yes       | no          | **conflict**: nothing written; the user asked |

"Takes" covers removal: a side that deleted p, against an unchanged other
side, deletes it there. The answer to a conflict is a side, and once
answered the path is decided as if only that side had changed it.

**The position.** Branch and HEAD follow the same table, with the position
as the one value compared: the side that moved since A is followed by the
other, and both having moved, differently, is a conflict, said rather than
settled (`peer::sync_primary_peer_with`). Working copies are synced only in
a pass that finds the positions agreeing.

**A pass** observes, decides every path, applies the decisions, and records
in A what each side now agrees on. Passes run when either side changes, and
again after a pass that wrote, until one writes nothing.

**Concurrency.** Either side may change at any moment — the user saving,
the agent editing, a program writing — including in the middle of a pass.
The sync is correct only if every property below holds for every
interleaving of those changes with every step of every pass.

## The properties

**P1 — Safety: the sync destroys no version that exists nowhere else.** It
writes or removes p on a side only while that side holds A(p) there, and it
checks that at the moment of the write, not at the moment of the decision. A
side that changed p after the pass observed it keeps its change, which the
next pass finds. This is the property every other one serves; Unison calls
it "never overwrites a change".

**P2 — Conflicts are asked, never guessed.** A path both sides changed,
differently, is written on neither side until the user answers, and the
answer writes only the paths it was about.

**P3 — Propagation.** A path changed on exactly one side reaches the other
within a bounded number of passes once both sides are quiet, unless a
conflict over that path is pending. Unison calls it maximality: everything
that can propagate does.

**P4 — Convergence and quiescence.** With both sides quiet and no conflict
pending, passes reach a fixed point: F and C agree on every path in scope,
A records that, and a further pass writes nothing and starts no other. The
sync's own writes never start an endless round of passes.

**P5 — No resurrection.** A path one side removed, which the other side has
not changed since A, never comes back on either side. (P1 and P3 imply it;
it is named because it is the failure a person sees, and because one
happened: a file renamed in the folder while it was on its way to the
checkout came back under its old name, f0ae3a2.)

**P6 — No echo.** A side's copy of what the sync itself wrote there is never
taken for that side's change. What the folder sent the checkout, observed
in the checkout, is agreement.

**P7 — Scope.** Nothing out of scope on a side is read as a change, written,
or removed there. A path entering scope — an ignore rule removed, sync set
on an ignored path — is decided by the table with A(p) absent: the same
content on both sides is agreement, different content a conflict, one side
only propagated. A path leaving scope on one side is left as it is there.

**P8 — Containment.** Every write lands beneath its working tree: never
through a link, never into `.git`, never at a path that escapes the root
(`taste_git::beneath`).

**P9 — Whole files.** A file is replaced whole — written beside itself and
renamed into place — so no reader sees part of one, and an interrupted
write leaves the old file or the new one.

**P10 — Crash tolerance.** A pass stopped at any step — the process killed,
the VM gone, a write failing — leaves a state from which later passes hold
P1 through P9. A records an agreement only for a path whose write
completed.

**P11 — Directories.** A directory the sync emptied, on either side, is
removed, up to the working tree's root; a directory holding anything, in or
out of scope, is never removed.

**P12 — Types.** A path changing kind — file, link, directory of files —
is an ordinary change, and a file and a directory never meet at one path
during a write.

## The design as built, against the model

**Where it fell short (2026-10-12, before the planner).** The archive was
two refs — `refs/taste/mirror/primary`, recorded whole when a pass wrote
the folder, and `refs/taste/mirror/sent`, what the folder had sent since —
and the rename resurrection (P5) was the gap between them. A pass planned
against the checkout's newest snapshot, taken by whatever last asked —
the VM's watcher two seconds after a change, a chat turn's end, a commit
— so an agent's edit since was not in the decision, and the folder's
change to the same path was sent over it (P1). Neither side's writes were
checked at the write: `write_tree_over` wrote the checkout's tree over the
folder as the pass had read it, and `send_change` wrote the folder's entry
over whatever the checkout held by then (P1). Directories the folder's
removals emptied stayed in the checkout until 096b680 (P11).

**How it is now.**

- **One archive, per path.** `refs/taste/mirror/primary` is A: its tree is
  the agreed working copy, its message the agreed position. A pass records
  it per path — each write that landed, each agreement, each path let go
  out of scope — and nothing for a write that did not land (P10). The
  second ref is retired, and deleted where found.
- **The pass observes the checkout itself.** Every pass begins by
  snapshotting the checkout, so it decides on C as it stands, not as it
  stood when something last asked (`peer::sync_primary_peer_with`).
- **Every write is a compare-and-swap.** Each carries the entry its side
  held when the pass observed it, and lands only if that is still there —
  checked once before anything is done and again after the new content is
  staged beside its place, just before the rename. In the folder,
  `beneath::write_file_if` and `write_link_if`; in the checkout,
  `peer::APPLY_SCRIPT`, which also refuses a path a link on the way would
  carry out of the checkout. A write that does not land is counted
  (`Pass::missed`) and another pass runs at once to decide it again.
- **The decision is `taste_git::plan`**, a pure function over A, F, and C,
  with the table above, the user's answer to a conflict limited to the
  paths they were asked about, and a write that would leave a file and a
  directory at one path made a conflict instead (P12).
- **Both directions in one pass.** The folder's changes no longer go out
  first in a pass of their own, with the checkout's waiting for the next.

**What remains.** Between the second check and the rename is a window of
a system call or two; a write landing in it is lost. Unison has the same
window, and closing it needs a filesystem that can rename only if a file
is unchanged, which neither side has. The checkout's side hashes each path
it writes with `git hash-object`, twice, which is a process per write: fine
for what people and agents change by hand, slow for a pass that writes
thousands of files at once (a first sync of a large synced directory).

## Making it hold: four layers

In order, each building on the one before.

1. **This specification.**
2. **A planner that is a pure function, tested exhaustively.** The decision
   moves out of `GitWorkspace::mirror_at` into a function of three maps —
   A, F, and C — returning per-path actions, with no I/O. Over a small
   universe — a few paths, each absent, a file of one of two contents, or a
   directory, in each of A, F, and C — every combination is enumerated and
   the plan checked against P1 to P7, P11, and P12. Most synchronizer bugs
   need only two or three paths to show, which is what makes a small
   universe enough (the small-scope hypothesis Alloy is built on). Around
   it, the design changes the gaps above call for: one archive, updated per
   path as each write lands; C observed by the pass itself, at its start;
   and every write a compare-and-swap against what the pass observed, on
   both sides.
3. **Seeded simulation of whole passes.** The real sync, both ways, over
   local repositories with a stand-in for the VM, driven by a random
   schedule of edits on both sides — writes, removals, renames of files and
   directories, kind changes, rule changes — interleaved at every step of a
   pass, with faults injected: a write failing, a file changing while it is
   read, a pass stopped between steps. After every step P1 is checked
   against a ledger of every version each side has held, and after the
   schedule ends the sides are left quiet and P4 is checked. A failure
   prints its seed, and the seed replays it. This is Dropbox's practice for
   its sync engine (*Testing sync at Dropbox*, 2020: CanopyCheck for the
   planner, Trinity for the whole engine).
4. **A model of the protocol, model-checked.** A TLA+ specification of the
   pass — observations, decisions, writes, archive updates — with
   concurrent edits on both sides at every step, checked with TLC for every
   interleaving over a small universe rather than a sample of them. It
   proves the design rather than the code; the simulation ties the code to
   it.
