# Spike: the header's width budget, glyph legibility, and port traffic

What the pass of 2026-09-08 parked, and what the next attempt at each of
those things needs to know before it starts. Written because three
separate pieces of work stopped at a decision rather than at a bug, and
the reasoning that got them there is not recoverable from the diff.

Not a proposal. Every measurement below was taken with the probe against
the fixture repository at 1440x900, dark, and can be retaken the same way.

## Conclusion up front

1. **The title bar is out of room, and it will be out of room again.** The
   search box, seven Tab lozenges, and the end cluster do not fit at 1440
   with anything else beside them. Two things have already been removed to
   make space. The next thing that wants a place in the header has to take
   something out, or the strip has to stop being unmeasured.
2. **The Flatpak deploy gesture is gone, not broken.** Everything that
   reads the pipeline still exists and still works. What was removed is
   the one button that started a build. Whatever replaces it should not go
   back in the header.
3. **Port traffic sparklines are TCP-only, and that is a property of the
   kernel, not of the design.** UDP has no per-socket byte counters to
   read. Anything that claims to show UDP throughput is either measuring
   something else or has taken over the forwarding.
4. **A glyph is not done until it has been measured against the glyph next
   to it.** Ink area and bounding box, not eyeballing, and not a text
   render.

## 1. The header's width budget

At 1440 the header holds, left to right: the two search toggles, the
search box (roughly x 610–940), and the end cluster. The search's Tab
strip is laid over the header's right side by
`root_overlay.connect_get_child_position` (`window.rs`) — anchored to the
box's right edge, measured by nobody, clipped by nobody. That is
deliberate: a measured strip changes width as its counts change, and the
header bar would recentre its title, so **the search box would move
because of what was typed into it**, which is the one thing it must never
do (David, 2026-09-08).

The consequence is that the strip runs over whatever is under it. Measured:

| thing | extent at 1440 |
| --- | --- |
| search box | 610–940 |
| Tab strip (keycap + 7 lozenges) | 952–1312 |
| "F1 for shortcuts" (removed) | ~1090–1250 |
| Flatpak deploy button (removed) | ~1280–1305 |
| menu, maximise, close | 1322– |

Both collisions were fixed by deleting the thing being collided with.
That worked twice and will not work a third time — the end cluster is
next, and it is not removable.

**What the next attempt has to choose between.** Every option costs
something already asked for:

- *Give the strip real space in the header.* Pack it as a measured child
  with a fixed width sized for its widest form, so counts changing cannot
  move anything. Costs the search box roughly 360px of width, always,
  including when nobody is searching.
- *Right-anchor the strip* against the end cluster instead of the box.
  Moves the collision to the search box's right edge instead — there is
  not room for both at 1440 either way.
- *Hide the strip when it does not fit*, the way the breakpoints already
  hide it below `ROOMY_MIN_WIDTH_SP`. Honest, but the threshold cannot see
  what else is in the header, so it would have to be measured at
  allocation time rather than declared as a width.
- *Fewer lozenges.* Refused already: all seven show whether or not they
  have matches, so muscle memory is stable (David, 2026-09-07).

The cheapest correct answer is probably the third, done by measurement
rather than by a breakpoint constant.

## 2. The Flatpak deploy gesture

Removed: the header button, its click handler, its breakpoint setter, and
the two places that re-armed it (`window.rs`).

Still there, untouched, and still working:

- `taste-flatpak`'s `Packager`, including `build_install_launch`;
- the MCP server's `flatpak_status` and `flatpak_logs`, which are
  read-only by design;
- the console's Flatpak log tab, and the `FlatpakState` / `FlatpakLog`
  events that fill it and raise toasts.

So the instrumentation for whatever replaces the gesture already exists;
only the trigger is missing. **Do not put the replacement back in the
header** — see § 1. The natural homes are the menu (where it is a verb
among verbs) or a row in the environment panel (where it is one more thing
an environment can do).

One thing to re-decide when it comes back: the old button was visible only
when a manifest existed, and it called `packager.rediscover()` on every
`FileTreeChanged` so that a manifest appearing mid-session lit it up. That
was a filesystem question asked on every tree change; if the replacement
lives somewhere that is not always on screen, it does not need to ask.

## 3. Port traffic sparklines

The plan that was feasible: sample the forwarded ports on the host at 1 Hz,
new connections as requests, byte deltas in the tooltip, into the existing
`Sparkline`. That plan rests on one fact — `ss -tin` reports, per TCP
socket, cumulative `bytes_sent`, `bytes_acked`, `bytes_received`,
`segs_out` and `segs_in`, to an unprivileged reader. A per-second delta of
those is a rate, and that is the whole mechanism.

**UDP has no equivalent, checked rather than assumed.** On this host:

- `ss -uin` prints no info block at all for UDP sockets — the `-i`
  payload is TCP's. Only `Recv-Q`/`Send-Q` come back.
- `/proc/net/udp` gives, per socket, `tx_queue`/`rx_queue` — a queue depth
  at this instant, not a total, so a healthy fast reader reads flat zero
  and a quiet line means nothing — and a cumulative `drops` counter, which
  is real and would sparkline honestly but measures failure, not traffic.
- `nf_conntrack_acct` is `0` by default; turning it on needs root, and
  conntrack sees the host's netfilter rather than rootless podman's
  user-space forwarder anyway.
- nftables counters per port, or eBPF, need privileges the app does not
  have and should not want.

So: **ship it TCP-only.** For a UDP-forwarded port, draw nothing and say
why in the tooltip, or draw drops under a different mark. Do not put a
queue-depth line and a throughput line under the same glyph — they look
identical and mean different things, which is worse than an empty space.

The one route to real UDP byte counts is for the IDE to forward the ports
itself instead of podman, which would count every byte of both protocols
exactly. That is a design change, not a feature: it moves the forwarding
path into the app, and pasta and slirp4netns do not behave identically.

Also unsolved for both protocols: an SSH or cloud substrate forwards
somewhere else, so the sampling has to run there and come back over the
connection. Sampling the host is the local case, not the general one.

## 4. Glyphs at fourteen pixels

Three glyphs sit together in the tree's Logs section, so they are judged
against each other whether or not anyone means to. The stock play triangle
beside them measures **10 x 12 pixels, 77 ink pixels** in its 14px slot,
and that is the bar.

Two hand-drawn hammers failed before the third attempt was bought off the
shelf:

| attempt | measured | read as |
| --- | --- | --- |
| slab head, claw on the handle | 12 x 10, 50 ink | a mitten |
| narrow head, thin claw | 13 x 10, 67 ink | a stray line |
| Bootstrap Icons `hammer` (MIT) | 13 x 12, 51 ink | a hammer |

The lesson is not "draw better". It is that **at this size a familiar
silhouette beats a correct one**, and a well-made icon set has already
paid for that. Reach for one first; `data/icons/README.md` records the
provenance and the notice.

The carrot is the harder case, and it is still a compromise. It has to be
the app icon (David, twice), and the app icon is a real carrot's
proportions: 36 units wide against 80 tall. At the app icon's own -18
degree tilt that renders **7.4 pixels wide against the play triangle's
10** and reads as a sliver. Laid corner to corner at -45 degrees its
bounding box is square and it renders 12 x 11 — the frame is computed from
the *rotated* geometry, which is what the original crop got wrong. Hairline
gaps are cut between the leaves by a mask, because the full icon separates
them by hue and one colour cannot.

What is still true: at 48 ink pixels it is lighter than its neighbours,
because a tapered diagonal is. If it ever needs more presence, the lever
is the shape, not the frame — and that means departing from the app icon,
which is the thing that has been refused.

## 5. Measure the frame; do not squint at it

Two habits earned their keep this pass and should be the default.

**`first_line` in the geometry dump.** Every label and text view now
reports where its first line of text actually sits inside its own box
(`textline.rs`, dumped by `ui_probe.rs`). Aligning a dot, a bullet, or an
icon against text means aligning it with that line, and a widget's box is
not that line — a label's leading, a card's padding, and a text view's
`pixels_above_lines` each push the two apart, in three different systems,
none of them visible in the source. The transcript's dots were out by 2.5,
3.5, and 1.5 pixels on three different row types, which is exactly the
kind of thing that gets reported as "still misaligned on some rows" and
cannot be chased any other way. One constant cannot serve three row types;
measure the line.

**`near-miss.py` before judging a frame** — already the rule (CLAUDE.md),
and it caught the files pane's chrome sitting two pixels inside the rows it
titles. But note what it cannot know: it flags *edges*, and some edges are
content, not insets. Two were judged not defects and left alone:

- right 327 against 331 in the file tree: a tree label stopping short of
  its trailing slot, against a section row's own right edge (libadwaita's
  4px sidebar row margin). Two different lists' theme insets.
- right 247 / 251 / 256 / 257: two captions' text widths, the linked
  filter box, and the section rows' title column when a trailing widget is
  present. Content-driven, not stated anywhere.

A near-miss between two numbers *the code states* is a defect. A near-miss
between a number and a piece of text is arithmetic. The tool cannot tell
them apart; the reader has to.
