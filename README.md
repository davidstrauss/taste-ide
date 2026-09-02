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
repository with git status markers and Dirty/Staged filters, an
Environments panel pinned at its foot listing Yours, brisk-3, calm-1,
spry-2 and wry-4 — each with a green, amber or red status dot and a
sparkline of its recent activity, and wry-4 marked with an accent rail and
an eye because it is done and waiting for review — a Backlog panel folded
open below it listing four issues with the environments that claimed them,
a Rust source file open in the editor with a minimap, the console
below detailing the environment you are in and its live shell roster, and an
agent chat on the right mid-turn — streamed prose, a shell tool card, a diff
card, and a permission card asking to rebuild an
environment.](docs/screenshots/hero.png)

## What it looks like

A workspace is a fleet, not a session: any number of named environments,
each a git clone with its own devcontainer, one chat, disk footprint and
token spend. The panel at the foot of the file tree is the whole fleet,
always — one row each, a traffic light and a live activity sparkline apiece
— and it is the app's only top-level control, because every other pane shows
the selected environment's world. The console beside it is the *one*
environment you are in, in the depth a sidebar row has no width for: a
header naming it — mode and container state, branch and dirty counts, the
chat working there, token spend, and the actions that start, rebuild or
destroy it — over a flat strip of tabs for its build log, shell roster,
podman resources, services and terminals. Nothing is listed twice, and
nothing is a tab set inside a tab.

![The Environments panel at the foot of the file tree, under a header
reading "Environments" with an amber subscription gauge reading 68% and a +
for a new one: five rows — "Yours" (selected and bold, amber, with an amber
attention dot), brisk-3 (amber), calm-1 (green, with a blue
unpublished-work dot), spry-2 (amber, with both dots) and wry-4 (red,
nothing running) — each of the live ones trailing an activity
sparkline.](docs/screenshots/envstrip.png)

Select an environment and every pane becomes its: its files, its git state,
its editor tabs, its console, its chat. Non-primary environments are
read-only to you — watch the agent work without racing it. The panel names
where you are and tints itself while you are away from your own checkout;
every tree row carries a lock, and files open as read-only tabs badged with
the environment's name.

![The taste-ide window watching calm-1: every file tree row padlocked, the
Environments panel at its foot tinted purple with "calm-1" selected and
carrying a lock, the editor tab labelled "filetree.rs · calm-1", the console
detailing calm-1 — its branch, dirty files, footprint and token spend, and
its agent's running terminal — and the chat headed "Claude Code ·
calm-1".](docs/screenshots/watching.png)

The whole fleet spends out of your own subscription — the same five-hour
and weekly windows your own Claude use draws on — so the panel header
carries what is left of it, and each chat's Utilization tab breaks it
down. Nothing is ever asked of the API to produce those numbers: the IDE
holds the credential, so it is the last hop of every request the agents
make, and it reads the account's own rate-limit headers off responses it
was already carrying. That means the figures are as of the last turn, and
every one of them says so.

![The chat pane's Utilization tab, in two sections. "This conversation":
context window 42% of 200k, session tokens, cached, thinking and cost.
"Subscription · as of 4 min ago": session window 68% used resetting in 1 h
19 min, weekly window 41% used, two per-minute API limits, "Spent through
this IDE — 777.0k total · calm-1 433.4k · spry-2 198.0k · wry-4 101.6k",
and a row saying where the figures came from.](docs/screenshots/utilization.png)

Agents publish branches; they never push. **An environment is the unit of
review**: it has exactly one branch, publishing is a checkpoint it makes as
often as it likes, and saying "I am done" is a separate sentence that flags
it and stops its container. Flagged environments are marked where you
already look — the Environments panel — and the console leads with the
decision: the branch, how far ahead of your own it is, whether it is
already in, and Open Review, Merge and Reject.

![The console's environment detail for wry-4: "wry-4 says it is done",
"agents/wry-4 → main · 6 commits ahead", and the buttons Open Review, Merge
and Reject, above a header line reading "working on i-0002 — Decide what a
stopped environment costs". Its shell roster reads "Nothing running here",
because flagging stopped the container; the file-tree flank shows the
review Open Review aimed there — "agents/wry-4 → main" and the two files
the branch changed.](docs/screenshots/review.png)

Open Review lists the branch's changed files, and clicking one diffs **the
branch**, not your working copy: the merge target's blob against the
branch's, read out of the repository. Those tabs are read-only and say what
they are comparing — they are not files on disk, and they close when you
leave the review.

![An editor tab titled "fleet.rs · agents/wry-4" with a bar above the diff
reading "agents/wry-4 vs main" and a lock at its right edge, showing one
removed line and a block of added ones; the file-tree flank beside it lists
Close Review "agents/wry-4 → main" and the changed files fleet.rs (M) and
disk.rs (A).](docs/screenshots/review-diff.png)

Issues are a git ref in your own checkout, so every environment can read
them and they ride along on your push. The queue is a **backlog** — its
order is yours to author — and it sits under the Environments panel, so
the environment that claimed something is a row away from the issue it
claimed.

![The Backlog panel: four issues in the order the user put them in, each
with a state checkbox and the environment holding it beside its traffic
dot, and one row showing its six hover actions — move to top, up, down, to
bottom, edit, delete.](docs/screenshots/backlog.png)

Narrow the window and the layout consolidates rather than rearranging: the
chat column and the console stop being panes and become tabs at the end of
the editor's strip — the same widgets moved, not new ones built — so the
window has one tab strip and whichever tab you are reading gets the whole
width. Nothing else shifts.

![A window at half-screen width: the file-tree flank still on the left with
the Environments panel and Backlog in it, and one tab strip carrying a file
tab, the chat tab it is posed on, and the Usage and Agent tabs beside it,
with a button at the strip's left edge reading 10 for the tabs that do not
fit.](docs/screenshots/consolidated.png)

![The same window posed on the console's half of that strip: Shells
selected, with the environment's header above it — traffic dot, name, the
chat working there and its state line — and Resources and Services beside
it in the same strip.](docs/screenshots/consolidated-console.png)

Shrink it further and the panes give way entirely: the window becomes the
two panels that were already answering the question. Same widgets, moved —
not a second rendering of them.

![A narrow window titled "taste-ide / fleet monitor": the Environments
panel with its subscription gauge at 68% and five environment rows, and the
Backlog panel below it with four issues.](docs/screenshots/gadget.png)

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
