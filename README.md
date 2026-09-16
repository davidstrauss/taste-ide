# Taste, an opinionated IDE for Bluefin and Silverblue

Taste is all you need.

An opinionated, AI-supported coding IDE: Rust, GTK4/libadwaita, Flatpak-first,
devcontainer-native via rootless Podman, with the
[Agent Client Protocol](https://agentclientprotocol.com) as the primary agent
abstraction. Files on the left, editor in the center, console on the bottom,
AI chat on the right — and no other arrangement. Convention over
configuration over code: projects behave uniformly because things live in
fixed places, not because each repo scripts its own behavior.

Built for Fedora's atomic desktops: [Bluefin](https://projectbluefin.io)
first, and the Silverblue it is made from. The host stays as it shipped —
podman from the base image is all Taste needs, every toolchain lives in a
devcontainer, and nothing is layered onto the OS.

The design and its non-negotiables: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

![The taste-ide window, dark: a file tree on the left showing the taste-ide
repository with git status markers and Dirty/Staged filters, and the
Backlog pinned at its foot — "Personal" first, then the issues: four of them
started, each with a status dot and a sparkline of its environment's
recent activity, one of those marked with an accent rail and an eye
because it is done and waiting for review; then a queued issue with an
empty checkbox, and a declined one struck through. A Rust source file is
open in the editor with a minimap, the console below is that environment's
machine room — its podman resources and whatever terminals you opened —
and an agent chat on the right is mid-turn: streamed prose, a diff card, a
command still running with the button that stops it, a refused push, and a
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
control, because every other pane shows the selected environment's world —
and it is where an environment is acted on: its light says what its
container is doing, its header starts, stops, rebuilds and deletes, its
row menu renames and nukes. The console below the editor is that
environment's **machine room**: podman's objects for it, and the shells
running in it. Nothing is listed twice, nothing is a tab set inside a tab,
and no pane says a thing another pane is already saying — the build log is
a document you open like a file, and the review's judgment sits beside the
diff it is a judgment on.

![The Backlog at the foot of the file tree, under a header reading
"Backlog 4 · 3 active" with Refresh at its right, over two-line rows that
each lead with the stage
their work has reached: "Personal" first (selected, a gear for work in
progress over "no environment · not configured", with a sparkline), then
"The composer loses a half-typed foll…" over "running · needs rebuild",
its gear badged blue at the upper right because the agent wants an answer
and amber at the lower right for the container; "Decide what a stopped
environment …" over "no environment · stopped" with an accent rail and an
eye, because it is waiting to be reviewed — and no health badge, since a
stopped environment is not an unwell one; "Serve the fleet over varlink"
over "no environment · building…" with the three dots of a starting
environment; "Terminal tabs should keep their outp…" dimmed under a
finished tick; "Sparklines should survive a fleet rebuild" over "queued ·
33m" behind the envelope of something newly filed; and "Add a per-project
settings file" struck through beside the circle-slash of a declined one.
The intervention bar at the foot is up because
Personal is selected — Start, Stop, Rebuild and Delete, greyed where they
do not apply to it — and it goes away entirely when nothing is selected or
checked.](docs/screenshots/backlog.png)

Select an environment and every pane becomes its: its files, its git state,
its editor tabs, its console, its chat. Non-primary environments are
read-only to you — watch the agent work without racing it. The panel names
where you are and tints itself while you are away from your own checkout;
every tree row carries a lock, and files open as read-only tabs badged with
the environment's name.

![The taste-ide window watching i-0007: every file tree row padlocked, the
Backlog at its foot tinted burgundy with "The composer loses a half-typed
foll…" selected over "running · needs rebuild" and carrying a lock, the
editor tab labelled "filetree.rs · i-0007", the console showing that
environment's resources, and the agent's chat on the right — its
`cargo clippy --workspace --all-targets` step still spinning, with the
stop button that kills it, over a question waiting to be
answered.](docs/screenshots/watching.png)

The whole fleet spends out of your own subscription — the same five-hour
and weekly windows your own Claude use draws on — so the chat's header
carries what is left of it, on a gauge beside the one for this
conversation's context window: two things that run out, side by side, the
same bar in the same traffic-light colours, and the account's marked
"Plan" so the pair cannot read as one measurement in two halves. Each
chat's Utilization tab breaks both of them down.

Nothing is ever asked of the API to produce those numbers: the IDE
holds the credential, so it is the last hop of every request the agents
make, and it reads the account's own rate-limit headers off responses it
was already carrying. That means the figures are as of the last turn, and
every one of them says so.

![The chat pane's Utilization tab, under the header's two gauges — this
conversation's context window bare, the account's marked "Plan", both
amber. In two sections. "This conversation":
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
look — their row in the Backlog, with an accent rail and an eye — and
**Open Review** on that row's menu aims the git views at the branch.

![The taste-ide window with the review aimed at i-0002: the file-tree
flank has become the review's file list — fleet.rs (M) and disk.rs (A),
each over the path it lives at — the Backlog below it shows
"Decide what a stopped environment …" with its accent rail and "no
environment · stopped", because flagging stopped the container, and the
chat on the right is asking to rebuild
it.](docs/screenshots/review.png)

Open Review lists the branch's changed files, and clicking one diffs **the
branch**, not your working copy: the merge target's blob against the
branch's, read out of the repository. Those tabs are read-only and say what
they are comparing — they are not files on disk, and they close when you
leave the review.

**The judgment is on that tab**, under the comparison: how far the branch
is from the merge target, and Merge and Reject. That is deliberate — it
means you cannot rule on the work until a file of it is open in front of
you. Merge is host-side libgit2, computed in the object database and
refused whole if it would conflict; Reject records the decision and asks
for a note to leave on the issue, so whoever picks it up next knows what
was already tried.

![An editor tab titled "fleet.rs · agents/i-0002" with a bar above the
diff reading "agents/i-0002 vs main" and a lock at its right edge, then a
second bar reading "agents/i-0002 → main · 6 commits ahead" with Merge and
Reject at its right, over one removed line and a block of added
ones.](docs/screenshots/review-diff.png)

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

New items are written in the **Dispatch** box — the one box under the chat
that everything you write leaves from (see "One composer, three
destinations" below) — filed with a held **F5**, or **B** held on a
controller; the list's last row is a ghost that points there, and F1
says the keys. The first
line is the title, the rest the body, and
**Enter** puts it on the queue. Select a row and the header answers for
it: **Start** a queued issue and it gets a world, **Stop** a running one,
**Rebuild** its container from the configuration on disk, **Delete** it.
Attach a screenshot, a selection or a file and it is kept beside the issue
in the ref, where an agent can read it back. Hold **Ctrl+D** to talk: the
words land in the field, transcribed on this machine by a model the IDE
fetches once, for you to read before anything acts on them. Reorder the
queue by dragging a row where you want it, or from the row's own menu.

That menu is what is pointed at *one* row, in three sections by what they
act on: where the issue sits, what the issue is (Edit, Decline, Delete),
and what its environment is — **Open Review**, **Rename** and **Nuke**,
which a row with no environment does not get at all. Within a section, an
action that is meaningless on this row is shown and disabled rather than
hidden, so the menu says where in the list you are and what has already
been decided.

![A backlog row's context menu: Move to Top, Move Up, Move Down and Move
to Bottom, all disabled because this row has an environment and its place
is its state's; then Edit, Decline and Delete; then Open Review, Rename
and Nuke in a section of their own.](docs/screenshots/backlog-menu.png)

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

![The same window posed on the console's half of that strip: the user's
own terminal selected — `[dev@ee6f679b0134 taste-ide]$` at its prompt —
and the icon-only Resources tab beside it in the same
strip.](docs/screenshots/consolidated-console.png)

Shrink it further and the panes give way entirely: the window becomes the
one panel that was already answering the question. Same widget, moved —
not a second rendering of it.

![A narrow window titled "taste-ide / fleet monitor": the Backlog,
"Personal" and four started issues with their dots and sparklines, then a
queued issue and a declined one.](docs/screenshots/gadget.png)

## One composer, three destinations

Everything you write leaves from one box, under the chat, called
**Dispatch**: a prompt to the agent, a new issue for the backlog, or the
message of a commit. Under the field sit three buttons, one per
destination, each a send glyph and the destination's own, each sending
directly: **Send to Chat** rightmost, where **Enter** and the
controller's **A** go, then **Backlog** and **Commit**. Those two are holds rather than taps — hold **F5** or **B** to
file the issue, hold **F6** or **Y** to commit what is staged with the
message — because a slip must not file or commit a prompt; a tap only
lights the button to say so. A button whose destination cannot take what
is in the box is disabled and says why on hover: a commit with nothing
staged, an image attached to a commit message, a chat whose environment
has no agent. The backlog's last row and the Staged view's panel both
point at this box rather than growing one of their own.

**Ctrl+D** focuses Dispatch the way **Ctrl+F** focuses Find, and holding
either one talks into it: the words are transcribed on this machine and
join the field at the cursor, spaced as you would have typed them, with
the cursor after them, for you to read before anything acts on them. A
quick double tap of either key — or of X and Start on the controller —
empties its box. **Ctrl+Shift+M** toggles dictation into Dispatch, for
the hand that learned it elsewhere.

An Xbox-layout controller drives the same things, read straight off
evdev: tap **X** to focus the composer and hold it to talk; **A** sends to
chat, **B** held files the issue, **Y** held commits what is staged;
**Start** is Ctrl+F, **LB** and **RB** step search's sections the way Tab
does, the D-pad steps results and **A** on one opens it. Hold **F1** and
every keyable thing
in the window wears a speech bubble with its keycaps; hold the
controller's logo button and the bubbles show the buttons instead. The
same bubble whatever the thing's size, so a small box is not a small
shortcut. Inside the Flatpak
the pad comes in through `--device=input`; the self-hosting run mounts
`/dev/input`.

![Hold F1: every keyable thing in the window wears a bubble naming its key
and its controller button.](docs/screenshots/reveal.png)

## From a stock Bluefin or Silverblue to self-hosting

Runs on an unmodified Bluefin — the standard image or Bluefin DX, which
adds nothing Taste needs — or on Fedora Silverblue: podman is already in
the base image, and nothing is ever installed on the host. Bluefin's own
additions (Homebrew, Distrobox, its developer tooling) go unused here;
Taste's toolchains live in devcontainers, not on the host.

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
already in the GNOME base that Bluefin and Silverblue share. Agents don't
need node on the host: they launch confined inside the devcontainer image
via podman.

Running the binary by hand works too — the flag just wraps:

```sh
./target/debug/taste-ide /path/to/some/project
```

## One host setting: the inotify budget

The one thing worth changing on the host. Linux caps the number of
inotify instances a user may hold (`fs.inotify.max_user_instances`,
128 by default), and the cap is per **uid**. Under rootless podman every
container in the fleet runs as your uid, so the desktop session, every
editor you have open, the IDE, and every agent in every environment all
spend from that one budget of 128. Claude Code alone holds several per
agent, and a fleet of half a dozen environments runs the budget out.

Running out looks like unrelated breakage rather than a limit: agents log
`EMFILE: too many open files` while setting up watchers, podman's network
helper falls back to polling, the IDE's file watchers refuse to start (the
watcher tests fail with the same message on a busy host and pass on a
quiet one), and `ulimit -n` — which the stock error text points at — is
not the limit that ran out. It compounds: an agent that dies and respawns
under this cap holds its instances until it is gone. The IDE names the
limit when its own watchers hit it, but it does not reconfigure your
machine.

Raise it once, and make it stick:

```sh
sudo sysctl fs.inotify.max_user_instances=1024
sudo tee /etc/sysctl.d/90-inotify.conf <<'EOF'
fs.inotify.max_user_instances = 1024
EOF
sudo sysctl --system
```

`/etc` is writable on Bluefin and Silverblue alike, so the drop-in
survives image updates and reboots. `fs.inotify.max_user_watches` is a
different limit, already in the hundreds of thousands on a stock install;
leave it alone. To see who is spending the budget:

```sh
find /proc/[0-9]*/fd -lname 'anon_inode:inotify' -user "$USER" 2>/dev/null | wc -l
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

## A private model on your own hardware

Run [llama.cpp](https://llama.app/)'s server on a machine of yours and
open a chat as **Claude Code (Private)**. It is the same Claude Code — the
same adapter, the same sign-in, the same home — handed a credential for
your server instead of your account, so its turns go there and draw on
none of the subscription. Plain **Claude Code** is unchanged and sits
beside it in the agent list, so one environment can hold a chat on each:
the real thing for the work that matters, the private one for what it is
good enough for. Written for Windows 11 with an RTX 3080 (10 GB).

The IDE side is a second upstream in the auth proxy, chosen by which
agent a chat was opened as. The private variant's settings shade shows
the server's configuration where the other's shows the model drop-down —
the server serves the one model it loaded whatever the request names, so
there is no model to pick (docs/ENVIRONMENTS.md → The auth proxy → A
private model).

1. In the NVIDIA control panel, under "Manage 3D settings", set "CUDA -
   Sysmem Fallback Policy" to "Prefer No Sysmem Fallback".
2. Install llama.cpp, in PowerShell. If it reports no CUDA Toolkit, run
   `winget install Nvidia.CUDA` and rerun it. Rerun it to update.

   ```powershell
   irm https://llama.app/install.ps1 | iex
   ```

3. Start the server with [gpt-oss-20b](https://huggingface.co/ggml-org/gpt-oss-20b-GGUF).
   Watch the VRAM figure in the load log: with more than about 1.5 GB
   free, narrow the `-ot` range to `(2[0-3])`; if allocation fails, widen
   it to `(1[2-9]|2[0-3])`. Keep `reasoning_effort` at `low` or `medium`;
   it goes through the environment variable because PowerShell mangles
   the quoted JSON on the command line.

   ```powershell
   $env:LLAMA_ARG_CHAT_TEMPLATE_KWARGS = '{"reasoning_effort":"low"}'
   llama serve -hf ggml-org/gpt-oss-20b-GGUF `
     -ngl 99 -ot "blk\.(1[6-9]|2[0-3])\.ffn_.*_exps\.=CPU" `
     -c 65536 -fa on --cache-type-k q8_0 --cache-type-v q8_0 `
     --jinja --host 0.0.0.0 --port 9931 --api-key <pick-one>
   ```

4. Open TCP 9931 in Windows Defender Firewall for the private network
   profile only.
5. From the machine running Taste, check the endpoint, then repeat
   against `/v1/messages/count_tokens`. Note the prompt and generation
   tokens per second the server logs for a request with a few thousand
   tokens of input.

   ```sh
   curl -s http://<windows-host>:9931/v1/messages \
     -H "x-api-key: <your-key>" -H "anthropic-version: 2023-06-01" \
     -H "content-type: application/json" \
     -d '{"model":"gpt-oss-20b","max_tokens":200,"messages":[{"role":"user","content":"Reply with one sentence."}]}'
   ```

6. In Taste, set a chat's **Agent** to **Claude Code (Private)**; its
   **Settings** then carry a **Private model** group with the endpoint,
   key header, API key, model name, and context window, the last two
   already set to this walk-through's values. Fill in the endpoint and
   key, then **Save and test connection**: the IDE stores the settings and
   sends the server one short request with them, and the line under the
   rows says what answered, or why nothing did. The private model is
   stored only in this project's IDE state, beside its credential, and
   every Claude Code (Private) chat in the project uses it. `x-api-key`
   is llama.cpp's usual header; select `Authorization: Bearer` only when
   the server expects it. Set the context window to the server's `-c`.

7. Prompt that chat. Note whether Claude Code asks for
   `/v1/messages/count_tokens` and what the server does with it, and
   whether the reasoning content renders in the chat. Both go on backlog
   issue i-0034.

## License

GPL-3.0-or-later.
