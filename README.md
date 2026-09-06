# Taste, an opinionated IDE for Silverblue enthusiasts

Taste is all you need.

An opinionated, AI-supported coding IDE: Rust, GTK4/libadwaita, Flatpak-first,
devcontainer-native via rootless Podman, with the
[Agent Client Protocol](https://agentclientprotocol.com) as the primary agent
abstraction. Files on the left, editor in the center, console on the bottom,
AI chat on the right — and no other arrangement. Convention over
configuration over code: projects behave uniformly because things live in
fixed places, not because each repo scripts its own behavior.

The design and its non-negotiables: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

![The taste-ide window, dark: a file tree on the left showing the taste-ide
repository with git status markers and Dirty/Staged filters, and the
Backlog pinned at its foot — "Yours" first, then the issues: four of them
started, each with a status dot and a sparkline of its environment's
recent activity, one of those marked with an accent rail and an eye
because it is done and waiting for review; then a queued issue with an
empty checkbox, and a declined one struck through. A Rust source file is
open in the editor with a minimap, the console below details the
environment you are in — its state and build log — and an agent chat on
the right is mid-turn: streamed prose, a diff card, a refused push, and a
permission card asking to rebuild the
environment.](docs/screenshots/hero.png)

## What it looks like

A workspace is a fleet, not a session — and **an environment is an issue
in progress.** Write an issue down and Start it, and it gets a world of
its own: a git clone with its own devcontainer, one chat given the issue
as its first prompt, a disk footprint and a token spend. The Backlog at
the foot of the file tree is therefore the whole fleet: your own checkout
first, then every issue, and the started ones carry their environment
right on the row — a traffic light, a live activity sparkline, an amber
mark when the agent is waiting on you. It is the app's only top-level
control, because every other pane shows the selected environment's world.
The console beside it is the *one* environment you are in, in the depth a
sidebar row has no width for — no header, because the row already named
it: a flat strip of tabs opens on the state in words, then its build log,
podman resources, services and terminals. Nothing is listed twice, and
nothing is a tab set inside a tab.

![The Backlog at the foot of the file tree, under a header reading
"Backlog · 4 · 3 active · 1 done · 1 declined" with an amber subscription
gauge two thirds full and a + for a new issue, and a Filter field: two-line
rows — "Yours" first (selected, "running" under it, a dot and a sparkline),
then four started issues — "The composer loses a half-typed follow-up on
switch" over "running · needs rebuild" with a blue unpublished-work dot
and a busy sparkline, "Decide what a stopped environment costs" over "no
environment · stopped" with an accent rail and an eye because it is done
and waiting for review, "Serve the fleet over varlink" over "building…",
and "Terminal tabs should keep their output…" dimmed as completed with an
attention dot — then "Sparklines should survive a fleet rebuild" over
"queued · 33m" with an empty checkbox, and "Add a per-project settings
file" struck through. Under the list, the composer: a field reading
"Title, then details" and a row of +, a microphone, File and a Start
pill.](docs/screenshots/backlog.png)

Select an environment and every pane becomes its: its files, its git state,
its editor tabs, its console, its chat. Non-primary environments are
read-only to you — watch the agent work without racing it. The panel names
where you are and tints itself while you are away from your own checkout;
every tree row carries a lock, and files open as read-only tabs badged with
the environment's name.

![The taste-ide window watching i-0007: every file tree row padlocked, the
Backlog at its foot tinted purple with "The composer loses a half-typed
follow-up on switch" selected and carrying a lock, the editor tab labelled
"filetree.rs · i-0007", the console detailing that environment — "running
· needs rebuild" with a Rebuild button, its orchestrator chat, what it is
working on, its publish ledger, and its agent's running terminal — and the
agent's chat on the right.](docs/screenshots/watching.png)

The whole fleet spends out of your own subscription — the same five-hour
and weekly windows your own Claude use draws on — so the panel header
carries what is left of it, and each chat's Utilization tab breaks it
down. Nothing is ever asked of the API to produce those numbers: the IDE
holds the credential, so it is the last hop of every request the agents
make, and it reads the account's own rate-limit headers off responses it
was already carrying. That means the figures are as of the last turn, and
every one of them says so.

![The chat pane's Utilization tab, in two sections. "This conversation":
context window 132.4k of 200.0k — 66% (filling up), session tokens 61.4k
in · 12.8k out, 992.0k cached, 6.1k thinking, 0.55 USD. "Subscription ·
as of 4 min ago": session window 68% used resetting in 1 h 19 min, weekly
window 41% used, two per-minute API limits, "Spent through this IDE —
777.0k total · i-0007 433.4k · i-0004 198.0k · i-0002 101.6k · 1 more",
and a row saying where the figures came from.](docs/screenshots/utilization.png)

Agents publish branches; they never push. **An environment is the unit of
review**: it has exactly one branch, publishing is a checkpoint it makes as
often as it likes, and saying "I am done" is a separate sentence that flags
it and stops its container. Flagged issues are marked where you already
look — their row in the Backlog — and the console leads with the decision:
the branch, how far ahead of your own it is, whether it is already in,
and Open Review, Merge and Reject.

![The console's environment detail for i-0002: a banner reading "Decide
what a stopped environment costs says it is done" with an Open Review
button, "agents/i-0002 → main · 6 commits ahead" with Merge and Reject
beneath it, and below that the state line "no environment · stopped" —
because flagging stopped the container — beside its chat, "working on
i-0002 — Decide what a stopped environment costs" and the publish ledger;
the file-tree flank shows the review aimed there — "agents/i-0002 → main"
and the two files the branch changed.](docs/screenshots/review.png)

Open Review lists the branch's changed files, and clicking one diffs **the
branch**, not your working copy: the merge target's blob against the
branch's, read out of the repository. Those tabs are read-only and say what
they are comparing — they are not files on disk, and they close when you
leave the review.

![An editor tab titled "fleet.rs · agents/i-0002" with a bar above the
diff reading "agents/i-0002 vs main" and a lock at its right edge, showing
one removed line and a block of added ones; the file-tree flank beside it
lists Close Review "agents/i-0002 → main" and the changed files fleet.rs
(M) and disk.rs (A).](docs/screenshots/review-diff.png)

Issues are a git ref in your own checkout, so every environment can read
them and they ride along on your push. The queue is a **backlog** — its
order is yours to author — and it is the same list as the fleet, because
starting an issue is what makes an environment and finishing one is what
ends it. Started rows sort to the top and carry their environment's marks;
the queue keeps your order below them; the resolved sink to the bottom.
One state per row, derived from both halves: queued, working, waiting on
you, failed, stopped, in review, completed, or declined — declined being
how you write down that something will not be done without deleting the
record of having decided it, and distinct from rejecting an attempt, which
hands the issue back to the queue.

The composer under the list is the chat's own — the same field, the same
attachment chips, the same microphone. Write an issue there: the first
line is the title, the rest the body, and the pill is **Start** — file it
and give it a world in one gesture, or File it for later. Select any row
and its issue is in the composer to edit; select a queued one and Start is
one press away. Attach a screenshot, a selection or a file and it is kept
beside the issue in the ref, where an agent can read it back. Hold the
microphone to talk: the words land in the field, transcribed on this
machine by a model the IDE fetches once, for you to read before anything
acts on them. Reorder the queue
by dragging a row where you want it, or from the row's own menu — which is
also where Edit, Decline and Delete live, and which a keyboard can summon
on the focused row. An action that is meaningless on a row is shown and
disabled rather than hidden, so the menu says where in the list you are
and what has already been decided.

![A backlog row's context menu: Move to Top, Move Up, Move Down, Move to
Bottom, then Edit, Decline and Delete in a section of their
own.](docs/screenshots/backlog-menu.png)

Narrow the window and the layout consolidates rather than rearranging: the
chat column and the console stop being panes and become tabs at the end of
the editor's strip — the same widgets moved, not new ones built — so the
window has one tab strip and whichever tab you are reading gets the whole
width. Nothing else shifts.

![A window at half-screen width: the file-tree flank still on the left with
the Backlog in it, and one tab strip carrying a file
tab, the chat tab it is posed on, and the Usage and Agent tabs beside it,
with a button at the strip's left edge reading 10 for the tabs that do not
fit.](docs/screenshots/consolidated.png)

![The same window posed on the console's half of that strip: the
Environment tab selected — no header above it, just the state in words
("running") beside its traffic dot and the follow, refresh and ⋮ actions
at the right edge — over the devcontainer's build log, and
the icon-only Resources and Services tabs beside it in the same
strip.](docs/screenshots/consolidated-console.png)

Shrink it further and the panes give way entirely: the window becomes the
one panel that was already answering the question. Same widget, moved —
not a second rendering of it.

![A narrow window titled "taste-ide / fleet monitor": the Backlog with its
amber subscription gauge, "Yours" and four started issues with their dots
and sparklines, then a queued issue and a declined
one.](docs/screenshots/gadget.png)

## From stock Silverblue to self-hosting

Runs on an unmodified Fedora Silverblue: podman is already in the base
image, and nothing is ever installed on the host.

```sh
git clone <this-repo> taste-ide && cd taste-ide
./bootstrap.sh
```

The script builds the project's devcontainer image, builds the IDE inside
it, and launches it against this repository — Wayland and GPU forwarded,
agent sign-ins persisted across runs. It is idempotent; run it again any
time (cached layers make subsequent runs go straight to launch). Every flag
is explained inline in [bootstrap.sh](bootstrap.sh).

That's the whole bootstrap. taste-ide opens this repository, recognizes it
is running *inside* its own devcontainer (full container mode — no safe-mode
locks), and from here on the work happens inside the IDE: terminals and
builds in the console, Claude Code in the chat pane, git in the file tree.

## Fast host runs

The quickest way to run Taste *on the host* (real portals, working
devcontainer supervision) without building a Flatpak:

```sh
./bootstrap.sh --host
```

It builds inside the devcontainer as usual, then runs the resulting
binary directly on the host — libgit2 is vendored into the binary and
everything else it links (GTK4, libadwaita, gtksourceview5, vte4) is
already in the Silverblue base. Agents don't need node on the host:
they launch confined inside the devcontainer image via podman.

Running the binary by hand works too — the flag just wraps:

```sh
./target/debug/taste-ide /path/to/some/project
```

## The production build

The self-hosting run lives inside the devcontainer, so it cannot itself
supervise devcontainers (podman does not nest). For real work on other
projects, run Taste as a proper Flatpak on the host:

```sh
./bootstrap.sh --flatpak
```

This builds the release Flatpak with `org.flatpak.Builder` (installed
per-user from Flathub, along with the GNOME runtime — a one-time,
multi-gigabyte download), installs it per-user, and launches it. Nothing
is installed on the host OS itself. The packaged app gets real portals
(browser links, dark-mode tracking) and full devcontainer supervision via
`flatpak-spawn` to host podman. Afterwards it lives in your app grid as
"Taste"; the in-IDE header-bar Flatpak button rebuilds and redeploys it
from inside the self-hosting run.

Packaging internals (manifest, offline cargo sources):
[build-aux/flatpak/README.md](build-aux/flatpak/README.md).

## License

GPL-3.0-or-later.
