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
it "never overwrites a change". "Nowhere else" counts the two working
copies, every commit either repository holds, and one more place: when a
commit reaches the checkout over uncommitted work that does not apply on
top of it, that work is kept in the checkout's `taste-ide-sync` stash, said
in the sync's note and offered by the file tree's Unstash
(`peer::push_ahead_into_checkout`). A commit made over a pending conflict
is the committer's choice, as git has it: they committed their working
copy over what HEAD held there, and what HEAD held is in history.

**P2 — Conflicts are asked, never guessed.** A path both sides changed,
differently, is written on neither side until the user answers, and the
answer writes only the paths it was about. An answer decides its paths
whichever side changed them — a file one side made where the other holds a
directory is a conflict of kinds, and answered as surely — with one
exception: an answer whose write would meet a file the folder ignores is
held, since the sync never removes an ignored file (P7), and the user moves
it.

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
or removed there. Each side's scope is its own rules — what it ignores, and
what syncs anyway — and a path is synced only where both sides have it in
scope; while their rules disagree, a path one side leaves out is left as it
is on both. A path entering scope — an ignore rule removed, sync set on an
ignored path, on both sides — is decided by the table with A(p) absent: the
same content on both sides is agreement, different content a conflict, one
side only propagated. A rule's change reaches its paths a pass after the
rule itself has reached the other side.

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
  stood when something last asked (`peer::sync_primary_peer_with`). This is
  a safety requirement, not a convenience: a snapshot from before a pass's
  own writes reads them as the checkout changing the paths back, and the
  write carrying that "change" into the folder passes its check, since the
  folder holds exactly what the pass expected — so the folder's edit is
  gone from both sides a pass later. The check at the write guards the
  side written; only a fresh look guards the side read from. The archive
  records the snapshot each pass decided on and whether it wrote into the
  checkout, and a pass offered that same snapshot again is `Stale`.
  (The model checker found this; see below.) The next snapshot is told
  that one and is never it (`snapshot::script_avoiding`): an unchanged
  working copy otherwise keeps its old snapshot commit, and a checkout put
  back exactly as it was before the pass wrote — the file sent there moved
  away again — was that commit, refused on every pass after.
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
- **A pass reads what changed.** "Decides every path" is what a pass
  computes, not what it has to read: a path the archive, the folder, and
  the checkout hold alike, in scope by the rules it was recorded under, is
  "nothing" in the table and nothing to let go of, and no write can meet it
  as file against directory, since neither side's tree holds both. So while
  the rules hold — no `.gitignore` or `.gitattributes` changed in either
  tree since the last pass in this process, the folder's `info/exclude` and
  `info/attributes` as they were, and the archive the one that pass
  recorded — the planner is given only the paths git's tree diff finds
  changed against the archive, and the archive is updated at those paths
  in place (`mirror::RulesSeen`). Anything else decides every path, as
  before. The checkout's snapshot keeps its index between runs for the
  same reason (`snapshot::script`): what a pass reads on php-src's 28,601
  files went from every one of them, several times over, to the ones that
  changed (2026-10-26). The one input not watched is the user's global
  excludes file: a path it newly ignores that both sides hold unchanged
  stays in the archive until either side changes it. The writes come out
  the same but in one case: a side that edits such a path in the very pass
  it comes back into scope has its edit propagated, where P7's "decided
  with A(p) absent" would ask about it as a conflict.
- **After a clean shutdown, the folder wins.** A VM brought back up while
  shut off and still marked disposable — its window's close found nothing
  on it this machine lacked, and the re-check just before it went down
  found nothing moved since (`disposable`) — cannot have changed since the
  two last agreed, so whatever differs was done on the folder's side: a
  history rewritten, a fresh clone, a branch switched, files edited. Before
  the first pass, Personal takes the folder whole — its branches (the
  IDE's `agents/*` aside, and a branch the folder lacks deleted), HEAD and
  its commit, and the working copy less what is ignored — with its old tip
  kept at `refs/taste/kept/before-folder-*`
  (`peer::give_checkout_the_folder`). The first pass then finds the two in
  step. Without it, a folder cloned afresh had no archive to measure from,
  and its rewritten history stood as a divergence for the person to settle
  (David, 2026-10-22: "any changes since the VM's clean shutdown should
  clearly win"). A VM that was still running, or went down without the
  re-check, keeps the rules above.

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
3. **Seeded simulation of whole passes.** (Built: `taste_devcontainer`'s
   `sync_sim`, described below.) The real sync, both ways, over
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
4. **A model of the protocol, model-checked.** (Built: `docs/models`,
   described below.) A TLA+ specification of the
   pass — observations, decisions, writes, archive updates — with
   concurrent edits on both sides at every step, checked with TLC for every
   interleaving over a small universe rather than a sample of them. It
   proves the design rather than the code; the simulation ties the code to
   it.

## The simulation, as built

`crates/taste-devcontainer/src/sync_sim.rs` runs the real sync, both ways,
between two repositories on one machine — the VM's end stood in for by an
ssh that runs git's far end locally — under a seeded schedule. The actors
write, remove, and rename files; rename a directory whole; make a file a
directory and back; make and replace symbolic links; flip the executable
bit; write and commit inside a submodule (a pair of its own); change the
rules — `.gitignore` taking a path in and out, and `taste-sync` bringing an
ignored directory back — on either side, so the two disagree for a while;
switch among three branches on either side, so each can move somewhere the
other did not; and commit on either side, both at once included. They act
at every step a pass reports — before it observes the checkout, during the
fetch, while it moves Personal's branch, before each write it sends — and a
pass may be stopped at any of those steps, or have a directory in the
folder refuse its writes.

Then the two sides are left quiet, and the simulation's user does what a
person reading the sync's notes would: answers every conflict (a side the
seed picks); where the two have diverged or stand on different branches,
commits their own work, moves to Personal's branch, and merges Personal's
commits, keeping their own side where the two clash — in the submodule
first, then the parent; and where Personal will not take the folder's
commits because a file of its own that git does not track is in the way,
moves that file aside. Then the specification is checked against what the
actors did:

- **P1 by a ledger.** Every version an actor writes records what it
  replaced on its side, and every version ever written must be held — in a
  working copy, in a commit a branch reaches on either side or in either
  submodule, or in a stash — or replaced, through that chain, by one that
  is. Only commits a branch reaches: the sync's own snapshots and archive
  are commits too, and a version only they hold is one the user cannot
  find. The ledger knows a version by its bytes, which are unique to each
  write, wherever they are: git merges one side's new bytes with the
  other's new mode cleanly, and carries a change along a rename (a
  modification of `d/x` stashed over a commit that renamed `d` comes back
  at `d2/x`), into versions no actor wrote whole or at that path. The mode
  is held by P4.
- **Nothing the actors did not write** is in either working copy.
- **P4**: on one branch at one commit, the submodule at one commit, the same
  files, modes, and links at every path in scope under the rules both sides
  end with, and a further pass that writes nothing.
- **P7**: an always-ignored file on each side is that side's own.

`cargo test -p taste-devcontainer sync_sim` runs three seeds;
`cargo test -p taste-devcontainer sim_ -- --ignored` runs
`TASTE_SYNC_SIM_SEEDS` (default 50) from `TASTE_SYNC_SIM_FROM` (default
1000) and prints how often each case came up; `TASTE_SYNC_SIM_SEED=<n>`
replays one, printing its schedule on failure, and `TASTE_SYNC_SIM_KEEP=1`
leaves its repositories behind to be looked at.

What it found, and what was fixed (each with a test of its own):

- The folder's snapshot failed every pass once the checkout made a file a
  directory (`snapshot::worktree_tree` cleared only the other direction).
- An answered conflict of kinds was asked about again forever (`plan`
  applied an answer only where both sides had changed the path).
- The checkout's snapshot failed every pass while a `taste-sync` rule
  matched no file (its kept index was never written, then moved).
- Scope was one pooled set of rules. A path one side ignores and the
  other does not was taken by the side that ignores it for removed, and the
  write that would have carried that over was refused at the checkout
  forever; and a sync rule only the folder had took a path the checkout
  ignores for in scope, which the checkout's snapshot, by its own rules,
  left out — so the stale-snapshot guard stood the sync still (P4). Each
  side's scope is now its own rules, the checkout's read from its
  snapshot, and a path is synced where both have it in scope (P7).
- Git counts an ignored file expendable: a fast-forward of the folder's
  commits into Personal overwrote, a rewrite's `reset --hard` would have
  deleted, and a switch of Personal's branch would have overwritten, an
  ignored file of Personal's own where the commit tracks one — and a stash
  never holds an ignored file, so the version was gone (P1). Each now
  refuses (`--no-overwrite-ignore`, `read-tree -m -u`), naming the file,
  and the user moves it. The folder's own merge and fast-forward refused
  likewise, before anything moves (`GitWorkspace::merge_with`).
- After a stash would not apply, `git clean` deleted every untracked file,
  among them one the stash never held: ignored when the stash was made, and
  no longer once the commits it moved to changed the rules (P1). Only what
  the failed pop itself put back, byte for byte, is removed now.
- A directory left empty on one side — by that cleanup, or by anyone's
  removal — is nothing to git and so to the plan, which sent the other
  side's file of that name as onto an empty path; the write found a
  directory and missed, on every pass (P4). An empty directory now reads
  as nothing at the write and gives way to it, and the cleanup takes the
  directories it empties with it.
- The stale-snapshot guard refused a fresh snapshot with the stale one's
  very commit, above, on every pass (P4).
- A pass that pushes the folder's commits in mirrors nothing, and leaves
  the working copies to the next pass; a submodule's push was not reported
  up, so its working copy waited for something else to start one (P4).
  It is reported now, and the IDE runs that pass at once, as it does for a
  write that missed.

Not in its schedule: a detached HEAD, a link where a directory is (an
actor's later write would travel through it, which is the actor's doing,
not the sync's), and `info/exclude` or a global excludes file differing
between the sides.

## The model, as built

`docs/models/Sync.tla` is the pass as a protocol: two working copies, the
archive, a pass that observes both, decides every path by the table,
writes each decision to its side, and records what landed — with the
actors editing either side at every step, a snapshot of the checkout taken
whenever something asks, and a pass that may stop partway. Kinds and scope
are left to the planner's exhaustive test; here a path holds a value, every
value an actor writes is new, and a removal is one more new value. It
checks, for every interleaving over three paths, four edits, and two
stopped passes (672,714 distinct states, under a minute):

- **Safety (P1)**: every value an actor wrote is held by a side, or was
  replaced by an actor with one that is.
- **QuietIsQuiet (P4, P6)**: a pass over settled copies plans no write.
- **Converges (P4)**: once the actors are done, the sync reaches a state
  where the two agree on every path that is not a conflict — given that it
  keeps running.

Two switches put the sync as it was into the same model: `CAS`, whether a
write checks its side at the write, and `Fresh`, whether a pass observes
the checkout itself. `build-aux/check-sync-model.sh` runs TLC (the
release's jar, digest checked, in a JRE image pinned by digest, neither
installed on the host) on all four combinations and holds each to its
outcome: the sync as it is holds every property; each of the other three
loses an edit, which is the model showing it can see the bug rather than
passing because it sees nothing.

It found one thing the simulation had not, because the simulation never
runs a pass on an old snapshot: checked writes alone are not safe. The
configuration with them and a lagging snapshot was expected to hold, and
TLC returned an eleven-step trace in which it loses an edit — the case
under "The pass observes the checkout itself" above, which the sync now
guards against as well as avoiding.

### The model with git in it

`docs/models/SyncGit.tla` puts the pass inside what the sync asks of git.
Each side has branches, a HEAD, and a working tree in which a path is
tracked, untracked, or ignored by that side's own rules; commits are
shared, each a tree and a parent. The actors edit, commit, switch branches
as git does — overwriting an ignored file in the way, which is theirs to
do — and change what their side ignores. The sync's protocol runs a step at
a time, the actors free to act between any two steps: the folder behind
follows Personal's commits; the folder ahead carries Personal onto its
commits, moving the branch alone where the two trees already agree, and
otherwise stashing Personal's uncommitted work, fast-forwarding, and
popping it back, with a pop that will not apply cleaned up; either side
follows the other's switch, Personal refusing over a change of its own
(an edit inside a submodule is the submodule's pair's, not the parent's,
and does not hold the switch);
divergence and both sides moving are said and left; and then the mirror of
`Sync.tla`. Git's rules are modelled as git has them: a fast-forward or a
switch refuses over a local change to a path it changes and over an
untracked file in the way, and overwrites an ignored one unless told not
to; a stash takes tracked changes and untracked files but never an ignored
one; a pop refuses over a local change to a path it touches, and conflicts
where both changed it.

TLC checks safety — every value an actor wrote held by a working tree, a
commit, or a listed stash entry, or knowingly replaced — that no conflict's
markers are left in either tree, and that once the actors stop the sync
comes to rest agreed or having said why not: 18.2 million distinct states
over two paths, one of them new, two branches, three edits, two commits
beyond the first, a switch, a rule change, and a stopped pass, in about
twenty minutes. Two switches restore what git and the scripts once did, and
each loses an edit within nine steps: `ProtectIgnored` false (a switch
Personal follows overwrites an ignored file of its own) and `Cleanup` —
`"old"`, `reset --hard` and `git clean`, and `"hardreset"`, which is what
the script still did on 2026-10-12. That one TLC found: Personal's own
change stashed, the folder's commit fast-forwarded in, and the agent
editing another tracked file before the pop failed on a conflict; `reset
--hard` reverted the agent's edit, which no stash or commit held. The
script now reverts only the paths the stash touched, and of those only one
holding the stash's version or git's conflict markers (`peer::SYNC_SCRIPT`,
with a test that reaches the window through a `post-merge` hook), and the
older cleanup of a stash's conflict leftovers does likewise.

What it leaves out: rebases and other history rewrites (history here only
grows), merges of divergent branches (said, and left to the user), more
than one parent, a detached HEAD, submodules, and directories (Sync.tla's
planner test covers kinds). `build-aux/check-sync-model.sh` runs all eight
configurations; `--quick` leaves out the long one.

## A damaged repository is repaired from its other copy

The folder and Personal's checkout each hold the whole history, so an
object one of them can no longer read is almost always one the other
still has (`taste_devcontainer::repair`). On 2026-10-19 a page of a pack
went bad in a VM's memory — the bytes on its disk were sound — and every
sync after it failed in `git fetch` with zlib's words, until it was
repaired by hand. A pass that fails on a damaged object now repairs it
and goes again, once: `git fsck` each copy; move a damaged pack aside and
unpack what is still readable in it (git will not read past a damaged
object in a pack to a sound copy elsewhere); copy each object still
missing from the other copy, checked against its id on the way in; then
repack and check again. The title bar's history says what was repaired,
in a sentence, and an object neither copy can read is named as lost
rather than failing every pass in git's words. git's own output goes to
the IDE log, never the title bar, and a failure said again is one entry
rather than one per pass.
