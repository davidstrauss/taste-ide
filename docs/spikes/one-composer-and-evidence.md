# Spike: one composer, marked-up attachments, and evidence of done

Design answer to David's questions of 2026-09-04: where the UI is doing
one job with several looks; whether the chat's prompt drafting and the
backlog's issue filing can share a design; how an attachment gets marked
up (redaction, rectangles, freehand, crop, arrows — "or is there a more
semantic way?"); and how environments attach evidence of finished work
and review it themselves. **Proposal, not yet approved.** No product code
changed for this document; every claim about today's UI cites the code.

## Conclusion up front

1. **One composer, three homes.** The chat prompt, the issue composer and
   the commit box become one widget with one anatomy — attachment chips,
   a text field, a left `+`, a right primary pill — differing only in
   what the pill says and where the text goes. The issue composer's two
   fields collapse into one by the commit-message convention the third
   home already lives by: **the first line is the title.**
2. **Attachments are content the issue owns**, stored beside it in
   `refs/taste/issues` (`issues/<id>/attachments/`), rendered in the
   backlog and in the agent's view of the issue, and readable by agents
   through one new MCP tool. The chat's existing attachment path (chips,
   drop target, `+` menu, image and text blocks) is the model; the issue
   gets the same chips because it gets the same composer.
3. **Mark-up is vector annotations over the image, flattened on send.**
   Five tools are asked for; the design ships four as one gesture family
   — drag a rectangle, then say what it is (callout, redaction, crop) —
   plus arrows, and defers freehand. The semantic layer is the
   annotation list itself: numbered callouts with notes travel as
   structured data beside the flattened image, and on a screenshot of the
   IDE a callout snaps to the widget under it and carries its name.
4. **Evidence is a filmstrip, not a video.** An environment attaches
   ordered frames with captions to the issue it claimed; `publish(ready)`
   refuses until the agent has looked at each frame and written a verdict
   through a tool that shows it the frame. Screencast files are accepted
   for humans and never shown to a model; the frames are what a model can
   judge.

## 1. Where one job has several looks

A component catalogue of `crates/taste-app/src` (by kind of job, not by
pane) turned up the following. Ranked by how visible the difference is to
a user who is not looking for it.

1. **The "shared composer" is not shared.** `composer.rs` says the chat
   prompt and the commit box "are the SAME widget"; today only the commit
   box (`filetree.rs:373`) calls `Composer::new`. The chat prompt
   (`chat.rs:1167`) hand-builds a vertical card — chips, a
   `sourceview5::View`, a toolbar row — that reuses the `.prompt-entry`
   class and nothing else. The two most-used inputs in the app have two
   anatomies, with a comment insisting otherwise. Section 2 is the fix.
2. **Five ad hoc field-plus-button rows.** Ignore expression
   (`filetree.rs:3357`), stash message (`:3279`), environment rename
   (`console.rs:2732`), reject comment (`:1935`), new-branch name
   (`filetree.rs:2678`): each a bare `gtk::Entry` beside its own button,
   with the button styled three different ways (`suggested-action`,
   `destructive-action`, none). These are `Composer::new(Job::…)` with a
   one-line field.
3. **The tool card is the one transcript card without `.card`.** User,
   plan and permission cards wear the theme's card surface; the tool card
   (`chat.rs:4245`) is a bare `GtkFrame` border, so mid-transcript it
   reads as a different kind of surface than its neighbours.
4. **One menu that is not a menu.** The file tree's row `⋮`
   (`filetree.rs:2427`) is flat buttons in a plain popover; every other
   dropdown is a `gio::Menu` in a `PopoverMenu`, with the keyboard
   navigation and theming that come with it.
5. **Two ways to build a row in a `navigation-sidebar` list.** Backlog,
   environments and services hand-roll a `ListBoxRow` + `Box`; branches,
   quick-open and the shortcut list use `adw::ActionRow`. Same list
   chrome, different internals, and the hand-rolled three reinvent
   suffix slots and activation each time.
6. **Two copies of the same bottom panel and the same confirm dialog.**
   `open_intervention`/`close_intervention` and `confirm_destructive`
   exist once in `filetree.rs` and once, near byte-identical, in
   `console.rs`. Pixel-identical today; two places to drift tomorrow.
7. **Three banner shapes.** `adw::Banner` (review band, editor
   conflicts), a hand-rolled `.taste-banner` box (devcontainer — the
   comment says why: the action has to sit beside the text), and a
   `Revealer` + button (jump banner). Low visibility; two of the three
   are argued for in place.
8. **Two tab mechanisms**, the chat's three flat toggles and the editor's
   `AdwTabBar`. Argued for in code (three fixed faces vs. N files) and
   never on screen together. Left alone.

What is already uniform, for the record: toasts (one overlay), transcript
meta rows, status labels, tooltips, and the two panel headers, which share
`.env-panel-header`.

**Recommendation, in order of payoff:** items 1 and 2 through the one
composer below (one change, seven surfaces); item 3 as a one-line class
(`.card` on the frame, border off — the same look the plan card has);
item 6 by moving the two helpers into a shared module; item 4 by building
the row menu from a `gio::Menu`; item 5 last and only if the hand-rolled
rows need a feature `ActionRow` has.

## 2. One composer

### What exists

Three surfaces take more than a word of text and act on it:

| Surface | Widget | Fields | Primary action | Attachments |
| --- | --- | --- | --- | --- |
| Chat prompt (`composer.rs`, `chat.rs`) | `Composer`: `[+] [TextView in .prompt-entry card] [Stop/Send pill]` | one | Send / Queue pill, Enter | chips above the card; drop target; `+` menu (selection, file, image) |
| Issue composer (`backlog.rs:451`) | `.card.backlog-composer`: heading, `Entry` title, `TextView` body, Cancel + File | two | "File" suggested-action button | none |
| Commit box (`filetree.rs`) | `Composer` with different icons | one | commit | none |

`composer.rs`'s own doc says the commit box and the chat prompt "are the
SAME widget with different icons and effects — one look, defined once".
The issue composer is the one that is not, and its own comment explains
why it has two fields: "they ask for two halves of one issue". The commit
box already answers that with a convention every git user knows — the
first line is the summary, the rest is the body — and nothing about an
issue is different: `Issue::render` writes front-matter and a title
followed by a markdown body.

### The design

**One `Composer`, parametrised by a `Job`:**

```
Composer::new(Job::Prompt)  → "+" · text · [Stop | Send/Queue]
Composer::new(Job::Issue)   → "+" · text · [Cancel | File / Save]
Composer::new(Job::Commit)  → "✦" · text · [Commit]
```

- **Anatomy is fixed:** an optional heading line (the issue's "New issue"
  / "Editing i-0007"), the chip row (hidden until there is a chip), the
  text field, and the action row — chips and the field standing on the
  same column (`PANE_BAR_INSET` in the chat; the panel's 12 in the
  flank), the pill at the end.
- **First line is the title** for `Job::Issue`. The placeholder says so
  ("Title, then details"), the first line renders in the heading weight
  while typing (a `TextTag`, the way the commit box could show its
  summary line), and `Issue::parse` already splits title from body.
  Editing an existing issue loads `title\n\nbody` back into the one
  field.
- **Cancel** is the same gesture everywhere: Escape, and the flat button
  the issue composer already has; the chat prompt's Escape already means
  Stop, and an empty prompt has nothing to cancel, so the chat shows no
  Cancel — the pill row differs by job, the field does not.
- **Attachments** come with the composer: the chip row, the drop target,
  and the `+` menu (`attach_from_disk`, the paste handler) move from
  `ChatPane` into `Composer`, and the pane becomes a consumer of
  `composer.take_attachments()`. The issue composer gets them for free,
  and the commit box gets a `+` that offers nothing — better than a
  third anatomy.

### What the issue does with an attachment

`refs/taste/issues` is a tree; a blob is a blob. Layout:

```
issues/i-0007/issue.md
issues/i-0007/comments/0001.md
issues/i-0007/attachments/0001-composer-clipped.png
issues/i-0007/attachments/0001-composer-clipped.marks.json   (when marked up)
issues/i-0007/attachments/0002-walkthrough-3.png
```

- Numbered like comments, for the same reason: disjoint paths under
  concurrent writers, and the number is the callout the prose can refer
  to ("see 1").
- `issue.md` references them as markdown images in the body, so the
  issue reads the same in the backlog row's tooltip, in an agent's
  `issue_list` result, and in any other markdown reader.
- The transaction is `issue_update` with a new `attach` change; the
  compare-and-swap loop (`issue_transaction`) already handles a lost race
  by re-numbering.
- **Agents read them through `issue_attachment(id, n)`**, which returns
  the image as an image content block the way `ide_screenshot` does
  (`server.rs` → "image content block, not JSON-as-text"), plus the
  `.marks.json` as text when there is one. `issue_list` gains a count and
  the captions, not the bytes.
- Size: the same caps the chat applies (5 MB per image, 256 KB per text
  attachment); a screencast is accepted up to a larger cap as an opaque
  blob, listed but never returned to a model.

## 3. Marking up an image

### Why not a raster editor

The five requested tools are what every screenshot tool ships, and a
`DrawingArea` + cairo implementation of them is a few hundred lines. But
a raster result throws away the one thing the receiver of the mark-up
most wants: **which** thing, and **what about it**. A red rectangle is a
pointer with no referent; the model infers the referent from pixels, and
the human reviewing the issue later gets a picture and a paragraph that
have to be matched by eye.

### Vector marks, flattened on send

A mark-up is a list, edited over the image and stored beside it:

```json
{ "image": "0001-composer-clipped.png", "size": [640, 900],
  "marks": [
    { "n": 1, "kind": "callout", "rect": [12, 4, 120, 34],
      "note": "two labels both ellipsized to two letters",
      "widget": "chat > GtkBox.top > GtkLabel.caption" },
    { "n": 2, "kind": "arrow", "from": [300, 600], "to": [215, 580],
      "note": "gauge should sit here" },
    { "kind": "redact", "rect": [40, 700, 200, 24] },
    { "kind": "crop", "rect": [0, 0, 640, 480] }
  ] }
```

- **Callout** = rectangle + number + optional note. This is the tool the
  request's "draw a rectangle" is really for, and it is the one that
  turns pixels into a numbered list the prose can cite.
- **Arrow** = from/to + optional note; numbered like a callout when it
  has one.
- **Redact** = rectangle, filled in the flattened image, and the
  original is **not kept** — a redaction that could be undone by reading
  a sidecar is not a redaction. The flattened PNG is what is stored; the
  sidecar keeps only the non-destructive marks.
- **Crop** = one rectangle; everything else is clipped to it on
  flatten, and mark coordinates are stored in the original frame so a
  crop can be changed before the first send.
- **Freehand** is deferred. It is the least semantic tool (a scribble has
  no referent and no note), and a callout with a note says the same thing
  legibly. Add it later as `{"kind":"stroke","points":[…]}` if the other
  four leave a gap.

**What is sent.** The flattened PNG (marks drawn, redactions burned,
crop applied) as the image block, and the marks as a numbered list in the
text: "1. two labels both ellipsized … 2. gauge should sit here". The
model gets the picture a human would draw and the words a human would
type; the human reviewing the issue gets both too.

**The semantic layer for the IDE's own screenshots.** A frame taken by
the probe (`ide_screenshot`, or the docs recipe) has a geometry dump to
go with it. When such a frame is marked up, a callout's rectangle is
resolved to the deepest widget whose bounds contain it, and the mark
carries that widget's dotted name (`chat.composer`) and type path — the
same names `ide_widget_geometry` answers to. For this project that is
the difference between "the thing in the top right is cut off" and
"`chat > GtkBox.top > GtkLabel.caption` at x=144 is ellipsized to 9px"; a
model can act on the second without a second look.

### Where the editor lives

Not a modal — the files area's rule ("no modals in the files area") is
the app's rule. An image attachment opens **in the editor pane as a tab**,
the way any file does, with a small toolbar (callout, arrow, redact,
crop) and the marks list beside it; Save writes the flattened image and
the sidecar back to the chip. The center pane is the one place with the
width an image needs, and an image tab is already a kind of tab the
editor knows how to hold. The chip in the composer shows a small badge
with the mark count.

Implementation: a `gtk::Picture` under a `gtk::DrawingArea` overlay, one
`GestureDrag` for rectangles and arrows, cairo for the overlay and for
flattening (render marks into a `cairo::ImageSurface` of the original
and write PNG). No new dependency: cairo comes with gtk4-rs, and the
sparkline already draws with it.

## 4. Evidence of done

### The gate that exists

An environment says it is done by calling `publish` with `ready: true`,
which flags it for review and stops its container (`server.rs:971`). The
issue gate (`issue_update` to completed) asks whether the work is in the
target branch. ENVIRONMENTS.md already says the quiet part: "an
environment that claimed something but has never published is not
evidence of anything". Neither gate asks whether the work *works*.

### Evidence is frames with captions

An agent working on a UI can already see its work: `ide_screenshot`
shoots the IDE the agent runs inside, and a project with its own probe
(this one) shoots its own build headless. A web project shoots a
headless browser; a CLI project's evidence is a transcript. In every case
the honest artefact is **an ordered set of stills with a sentence each**
— a filmstrip — not a video:

- a model can judge a still and cannot judge a video;
- a still with a caption is a claim ("after the change, the gauge is
  amber at 66%") that a reviewer can check against the picture;
- stills are small, diff-able in the issue ref, and cost nothing to
  produce from the tools that exist.

Screencast recordings stay welcome as attachments a human drops in
(GNOME records with Ctrl+Shift+Alt+R; the file is a `.webm`), stored as
opaque blobs and listed with their size. They are for the human
reviewer. Recording from inside the IDE (the ScreenCast portal, pipewire,
an encoder) is real machinery for a thing a model cannot read; it is not
in this design.

### The self-review gate

Two new tools and one changed refusal:

- **`issue_attach(id, path | data, caption)`** — adds a frame (or any
  file) to the claimed issue, numbered, with a caption that is not
  optional. Returns the number.
- **`issue_evidence_review(id, verdicts: [{n, verdict: pass|fail, note}])`**
  — the tool **returns each frame as an image block** before it records
  anything, so the model that calls it has looked at what it is judging;
  a review that names a frame the caller was not shown is refused. The
  verdicts are written as a comment (`Evidence reviewed: 1 pass, 2 pass,
  3 fail — the gauge is still blue at 0.9`), which the backlog shows and
  the human reviewer reads first.
- **`publish(ready: true)` refuses** when the environment's claimed
  issue has no evidence, or evidence with no review, or a review with a
  `fail` in it — naming what is missing. A claim with no UI (a library
  change) attaches its test output as a text frame; "there is nothing to
  show" is a caption, not an exemption. Declining (`issue_decline`)
  needs none of this, for the reason the docs already give.

The human's review band (`Open Review · Merge · Reject`) gains the
filmstrip above the file list: the frames, their captions, and the
agent's own verdicts. That is where "review their own work completion
evidence for quality and completeness" becomes checkable — the reviewer
sees the frames and sees whether the agent's verdict was honest.

## 5. Voice

David, later the same day: "I also need you to add voice input that I can
use for the IDE. I would like to be able to mostly interface with the
chat since the chat itself should be able to add backlog issues and do
things on my behalf."

The second sentence is already true and is what makes the first cheap.
The chat's agent holds the IDE's tools — `issue_create`, `issue_update`,
`issue_claim`, the environment and orchestration tools on the designated
chat — so "file an issue about the gauge" said into the chat becomes an
`issue_create` call by the agent, in the backlog, under the agent's name.
Voice therefore does not need to reach the backlog composer, the commit
box, or a command palette: it needs to reach the **composer's field**, and
with one composer that is one place.

### Push to talk, into the field

- A microphone button in the composer's left action slot beside `+`
  (`Job::Prompt` only; the other homes get it for free later or never).
  **Hold to talk, release to transcribe** — no wake word, no
  always-listening, nothing captured while the button is up. The button
  shows a level while held so a dead microphone is visible before the
  silence is transcribed.
- The transcript lands **in the field, not in the transcript**: the user
  reads it, fixes a word, adds a chip, and sends — the composer is the
  confirmation step the design already has, and the agent never acts on
  words nobody read. A `Send` spoken at the end is a word like any other;
  auto-send is a setting to consider after the first week, not a default.
- While the agent works, speaking still writes to the field; the
  existing queue-on-Enter path (`Queue`) is what sending does mid-turn.

### Capture and transcription, on the IDE's side of the line

- **Capture** is PipeWire, the desktop's audio server: `pipewiresrc`
  through GStreamer, or `pw-record` as a child process writing 16 kHz
  mono PCM to a pipe. The IDE's own process does this — never an agent,
  never a container. Packaging: the Flatpak manifest adds
  `--socket=pulseaudio` (PipeWire's compatibility socket, the sanctioned
  Flatpak permission for microphones); the self-hosting bootstrap mounts
  the user's PipeWire socket into the IDE's container. Neither is present
  today.
- **Transcription is local.** `whisper.cpp` through the `whisper-rs`
  crate, on the CPU, with a model the IDE downloads once into
  `$XDG_DATA_HOME/taste-ide/models/` from a pinned URL with a pinned
  digest (the same rule as adapter versions: no `latest`). `base.en`
  (~150 MB) transcribes a ten-second utterance in about a second on this
  host's 24 cores; `small.en` is the upgrade if accuracy disappoints.
  Nothing leaves the machine: the proxy holds the Anthropic credential and
  Anthropic has no speech endpoint, so a cloud transcriber would be a new
  credential, a new destination for the user's voice, and a new hole in
  the line CLAUDE.md draws. Not worth what it buys.
- Transcription runs on the blocking pool; the field updates when the
  result lands, with the button disabled between release and result so a
  second recording cannot overlap the first.

### What it is not

Not a command grammar. "Open the review for calm-1" is a sentence the
agent can act on with the tools it has; teaching the IDE to parse it would
be a second interpreter beside the one that already understands English.
The one exception worth a hard-coded phrase is none: even "stop" goes
through the field, because the Stop button and Escape are already a
keypress away and a misheard "stop" mid-dictation would cancel a turn.

## Order of work

1. `Composer` grows chips, drop target and `+` menu; `ChatPane` uses it.
   No visible change. (Refactor; the probe frames prove it.)
2. The issue composer becomes `Composer::new(Job::Issue)` with the
   first-line title. Attachments stored in the ref; `issue_attachment`
   tool; backlog rows show a count. (Visible: one composer look.)
3. The image tab with callout / arrow / redact / crop, flatten on save,
   marks sent as text beside the image. (Visible: mark-up.)
4. `issue_attach`, `issue_evidence_review`, the `publish(ready)` refusal,
   the filmstrip in the review band. (Visible: evidence.)
5. Widget snapping for probe frames — the geometry dump is already
   there; this is the resolver and the field in the mark.
6. Push-to-talk: the packaging permission and the bootstrap mount first
   (they gate everything), then capture, then `whisper-rs` with the
   pinned model, then the button. The first frame is the button held with
   a level showing; the second is a transcript in the field.

Each step is a commit with a frame. Freehand, in-IDE recording, cloud
transcription and a voice command grammar are listed under "not in this
design" so that they are decisions, not gaps.
