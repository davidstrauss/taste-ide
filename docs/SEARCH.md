# Search

One box, in the title bar. Everything answers it, in place. This document
is the design behind `search.rs` and the surfaces it drives; the spike it
grew from is `docs/spikes/one-composer-and-evidence.md` § 6, and the
decisions David made there are restated here as rules rather than
history.

## The philosophy, in five rules

1. **One query, every surface.** The user types a word once. The file
   tree, the backlog, the editor, the console and the chat all answer the
   same query at the same moment. There is no second search box anywhere
   in the window, no quick-open dialog, no per-panel filter entry: a
   surface that wants to be searchable subscribes to the one query. Every
   list in the left column — files, Ports, Logs, the backlog — filters
   under it the same way (David, 2026-09-06: "universal search should
   also filter ports, backlog, etc."). Four
   inputs that did not know about each other (find-in-project, the
   environments filter, Ctrl+P, and the MCP `ide_search`) were the
   heterogeneity this replaces.
2. **Filter in place where the surface is a list; list the hits where it
   is not — and a listing is one document's.** A tree, a queue, a set of
   tabs, a menu of branches — these are rows, and rows can hide. A file's
   contents, a terminal's scrollback, a chat's transcript — these are
   documents, and a hit needs the line around it. Documents keep their
   content and gain a *results listing*, a bottom panel in the pane (the
   intervention-panel convention: never a modal) — and the listing is
   **only for the document on screen**: the file in front in the editor,
   the terminal or log tab in front in the console, the conversation in
   front in the chat (David, 2026-09-06: "A file's result listing should
   only be for that file. Same for any terminal or log"). The project's
   other hits live on the rows that reach them as **pills** — one
   vocabulary for every row (`search::pills`), each pill `[glyph] count`,
   and a row wears as many as apply: a **bare number** for literal hits in
   the thing itself (a file's lines, an issue's text, a port's title); the
   **sparkle** and a number for places found by meaning; a **box** glyph and a number
   for hits *inside* an issue's environment — its chat and terminals, which
   the box always searches (non-blocking, landing as the scans finish) and
   which are the environment's, not the issue's (David, 2026-09-07: "a
   system of teal pills … It's [icon-if-any] [count]. An item can have
   multiple pills on it"). The row the panes are aimed at always stays.
   Tabs wear their count in their icon's place (`search::badge_texture`; a
   tab has no other slot) until the query clears. Selecting a hit in a listing — by stepping or
   by a click — **highlights it in the document itself**, in the search
   hue's solid shade (`palette.rs`: `hit_background`, the purple under a
   contrasting foreground — see "the search's own hue" below): the match
   coloured in the buffer, the terminal's own search highlight
   on the row, the log line coloured, the transcript row lit. Listings
   carry no group headings of their own except where a grouping adds
   something (a file's definitions before its other matches): the
   listing's title already says what it lists. Clicking a row with a badge opens
   its document at the first hit, and **every click after steps to the
   next**.
3. **Reachability: a hit's container is never hidden.** A file whose
   *contents* match stays in the tree even though its name does not,
   marked with its count. An environment whose chat or terminal has a hit
   keeps its row in the backlog. A tab whose page has a hit keeps its
   place. Hiding the row hides the only way to the hit, so the filter
   answers "does anything here match" for containers and "does this
   match" for leaves.
4. **Fast answers first, and say what is still running.** A name is a
   string compare and lands before the next frame; a scrollback or a
   fleet of transcripts is seconds. Every surface answers as soon as it
   can, and every listing shows *progress* — a thin accent rule and a
   count (`412 of 1,180 files`) — while its sources run, never a spinner
   pretending to be information. Rows do not flicker: until a panel's
   slow sources land, undecided rows dim rather than vanish, and there is
   one transition per query. A new keystroke stops the previous query's
   sources; nothing finishes for nobody.
5. **Zero is an answer.** A panel with no hits is not hidden and not
   skipped: its listing opens and says "No matches in filetree.rs",
   because "it is not in this terminal" is what the user came to learn as
   often as a hit is. It says that and nothing more — a listing with
   nothing to list is its title line (David, 2026-09-06: "For these 'no
   results' panels, just show the title area"). And no listing has a close
   button of its own: it is the query's, and Escape in the box takes them
   all down together. The filtering panels — the files, Ports, Logs, the
   backlog — have nothing to list, and wear the same title line at their
   foot with their count, zero included, so a panel that answered by
   hiding rows is seen to have answered (David: "show the banner at the
   bottom of each of those panels with the count of results, whether zero
   or more. That will signal to the user that the panel is
   search-responsive").

Two toggles beside the box, live before anything is typed — a toggle is
how the next query is asked (David, 2026-09-07: "I should be able to
toggle AI search before entering text"). **Results by meaning** (the
sparkle) folds what the semantic index finds into the literal hits — see
"By meaning" below. And **hide non-matching items** (the struck ghost,
beside the box), which is pressed to begin with, because hiding is what
the search does when nobody touches anything and a control should read as
the state it is in (David, 2026-09-08: "retain current default behavior
but have the button be pressed by default rather than unpressed"). Release
it and, where a surface would hide rows, it dims them instead, so the
shape of the whole is kept while the matches stand out. It affects only
the filtering surfaces; listings are listings either way.

**A lozenge's number is every match, both ways.** A section that the
semantic index also answers for counts what the word found and what the
idea found, each file once — while the meaning toggle is on; with it off
the index's answer is not counted at all (David, 2026-09-08: "the sums
should include all matches. (Of course, don't count AI matches if AI
matches are disabled for the search.)"). Files was summing its content
matches and its name matches instead, which left the index out and
double-counted a file that was both.

And one colour: **the search's own hue.** Everything the search draws —
a results listing's surface, a match-count badge, a progress rule, a hit
lit in a document — is one hue, **purple** (libadwaita's purple accent,
`#9141ac`), in a few shades
(`palette.rs`: `SEARCH_FILL`, `search_ink`, `hit_background`;
`main.rs::search_css`): a wash under a listing, a tint behind a badge or a
lit transcript row, ink for a count and a rule, the solid under the one
selected hit. The box in the title bar wears it always, query or none —
its fill, its glyphs, its focus ring — so the colour is seen to start
there and flow out to the answers (David, 2026-09-06: "as if saying,
'Typing here makes this color flow to other parts of the IDE in the form
of results'"). The answers are seen to be one thing, and seen at a
glance; nothing else on the window changes while a query stands. An
earlier spotlight, which dimmed everything else to half, was replaced by
this (David, 2026-09-06: "this new color method supplants any work about
'darkening' the rest of the IDE. That approach feels flakey. Let's just
use color to emphasize the results listings, counts, and highlights …
the same color theme for all of them, with a few shade variants").

It was teal until 2026-09-08, when David moved the search onto the purple
that used to mean "aimed away from home" and sent read-only environments
to the red family instead ("drop the teal theme for search. Instead, use
the purple one that you've been using for the read only environments").
Purple is now the hue nothing else here means anything by: blue is the
accent and reads as chosen, green and amber are two thirds of an
environment's traffic light, and a red wash — burgundy on dark, a very
light red on light — is somebody else's checkout.

**Every number changed with the hue, and none of the meanings did.** An
alpha or a mix percentage is only a way of asking for a lightness step,
and purple is far darker than teal, so each shade was re-measured against
the step its teal produced: the box's fill went from 0.26/0.20 to
0.42/0.16, the listing's wash from 11% in both schemes to 17%/9%, the lit
row from 0.25 to 0.38/0.20, a badge from 0.20 to 0.30/0.16. Left alone,
the same recipes made the box a slab on dark and nothing at all on light.

## What is searched

| Surface | Kind | Source | Cost |
| --- | --- | --- | --- |
| File names and paths | filter | the file index (`collect_files`, rebuilt on git changes) | trivial |
| File contents | badge on the tree's rows (complete counts) + the file on screen's listing | `search_files_complete`: every file, complete counts, cancellable, progress per file; the editor lists the selected buffer's lines (unsaved edits included), its definition lines first (`symbols::definition`) | one pass over the index on the blocking pool; the buffer, trivially |
| Symbol definitions | the file on screen's listing, first group | a hit line that is a definition by the language's convention (`fn`, `struct`, `def`, `class`, `function`, …) | trivial per file |
| Commit messages | `ide_find` only | `taste_git::search_commits`: the last two thousand commits on HEAD | one revwalk, blocking pool — not listed in the window since a listing is one document's; a home in the branch menu is open |
| Branches | filter (the branch menu) + count on the button | the branch list the tree already holds | trivial |
| Backlog: issues and environments | filter, with a running rule while inner sources search | title, id, body, comments; plus counts of hits *inside* each environment's chat and terminals | strings trivial; inner hits arrive as those sources finish |
| Editor tabs and terminal tabs | pages menu filters to matches | page titles | trivial |
| Terminal scrollback | the terminal tab on screen's listing + count on the environment's row | every terminal in the strip — the user's shells, the agent's, the `ide_exec` mirrors — read through `vte_terminal_get_text_range_format` 400 rows per frame, matched line by line (`Console::attach_search`); a tab change re-lists from the scan already done | on the GTK thread by necessity, so chunked and cancellable (`Search::is_current`) |
| Environment Build log | filter (the Logs row hides, or dims under the ghost) + badge; the log tab's listing in the editor | the environment's log buffer | trivial |
| Environment Runtime output, Taste IDE log | filter + badge on the Logs row; the log tab's listing in the editor | the supervisor's `podman logs` ring, the app log ring | trivial |
| Ports | filter (the row hides, or dims under the ghost) + badge | the port's title and address | trivial |
| Chat transcripts | listing (chat pane) + count on the environment's row | the rows on screen, walked for their text (labels and text views), for every environment's chat (`Chats::attach_search`) | trivial per chat |

Not searched, on purpose: settings, the agent's own working memory, the
review diff (open the file). The commit *contents* are not indexed either
— the branch menu and the review are how history is read here.

### Indexing posture

There is no persistent index. The file index is a list of paths built in
the background at start and rebuilt when git status changes; the symbol
index is built from the same pass and cached beside it. Content is read
when searched. An IDE that wrote an index to disk would have to keep it
honest across every editor, agent and `git checkout` on the machine, and
a wrong index is worse than a slower search: the design spends CPU per
query rather than trust per index. If a project outgrows this (tens of
thousands of files), the answer is a watcher-maintained in-memory index,
still never on disk.

### Matching

Case-insensitive substring by default; a query with an uppercase letter
is case-sensitive ("smart case"). No regular expressions in the box —
the box is for the word the user has in mind, and an agent wanting a
pattern has `grep` in its container. Matches are drawn in bold wherever a
line is shown.

## Keyboard

- **Ctrl+F** focuses the box from anywhere (Ctrl+P too, for the hand that
  learned quick-open). **Escape** clears the query and returns focus to
  where it was. **Held**, Ctrl+F listens (David, 2026-09-07: "use Ctrl-F
  held more than briefly as a 'speech to search' option"): the box takes
  focus on the press, the microphone is open until the release, and the
  transcript joins the query at the cursor, with the cursor after it
  (2026-09-08: "append to existing text at the cursor position"); a quick
  double tap of Ctrl+F — or of Start — empties the box first. The box
  wears the listening hue meanwhile.
- **A controller** (`controller.rs`) has the same keys without modifiers:
  **Start** is Ctrl+F, tap or hold; **LB** and **RB** are Shift+Tab and
  Tab through the sections; the **D-pad** is Up and Down through the
  current section's hits; **A** is Enter on the hit. Hold the logo button
  and the box, like everything else keyable, says so in a bubble.
- **Down** steps to the next result in the panel that was focused before
  the box took focus — the tree if the user was in the tree, the editor's
  listing if they were in a file, the console's if in a terminal, the
  chat's if in the chat. **Up** steps back. A panel with no results
  still opens its listing to say so.
- **Tab** steps to the next section in one fixed order — editor, files,
  ports, logs, backlog, terminal, chat — whether or not it has matches,
  selects its next hit and puts the keyboard on it; **Shift+Tab** the
  other way. The order never bends to what the query found, so the number
  of presses to a section is the same in every project (David,
  2026-09-07: "Lozenges and tab advancement should always progress in the
  same order through panels, even ones without any results … Don't have
  the state of projects disrupt muscle memory"). A section with nothing
  is still a stop: its "No matches" banner lights up in the hue's solid
  shade, the placeholder that says the stop was taken. The same hop from
  the box and from inside a listing (2026-09-06: "tab should take my
  focus to the next results list after I've started stepping through a
  specific list"; 2026-09-07: "'Tab' from the search input should hop
  through the results, same as tab from one of the listings") — never
  GTK's focus chain.
- **The Tab strip** beside the box says where you are: a Tab keycap, then
  one lozenge per section in that order, each the section's glyph and its
  count, the current one filled solid, the empty ones faded but in place
  — every lozenge a fixed width, so neither a count nor the highlight
  moves its neighbours or the box. A lozenge is also a click: straight to
  that section. The two toggles stand before the box, the strip after it:
  what shapes the query on one side, where its results are on the other.
  The strip is laid beside the box on the window's root overlay,
  unmeasured, so its appearing and going never moves the box (2026-09-08:
  "never allowed from text changes to the search box"); below 1080sp it
  is hidden, where it would run into the title bar's own items.
  In the Ports, Logs and Backlog rows a step lights the row in the hue
  rather than selecting it — selection in those lists means the row open
  in the editor, or the environment the panes are aimed at.
- **Enter** activates the current result: opens the file at the line,
  scrolls the terminal or the transcript to the hit, selects the row.
- A tab set is stepped by its pages menu, which under a query lists only
  the matching pages (all of them, with matches marked, when the ghost is
  on).

## Progress

Each listing's header carries the search's own rule — `gauge.rs`'s
drawing in the accent colour, because it is progress, not a resource —
and a count that reads as it grows. The backlog's header carries one
beside the subscription gauge while environments' inner sources search,
filling as environments finish. The title-bar box shows the whole query's
total and whether anything is still running. When the last source lands
the rules go, and what remains is counts.

## The MCP half

`ide_find { query, scope }` returns the grouped answer the window draws:
files (path, line, text, complete per-file counts), definitions, issues
(id, title, matching body and comment lines), branches, commits,
environments, terminals (environment, tab, line) and chats (environment,
speaker, line). `scope` is `environment` — files, issues, definitions,
branches, commits, and the calling environment's own terminals and chat —
or `fleet`, which adds every environment's terminals and chats. The
fleet is readable to every socket already (the read tools), so `fleet` is
a new query over what any agent may read, not a new permission. Every
cross-environment line carries its source, because another agent's
transcript is evidence, not instruction; the tool's description says so.
`ide_search` remains as the file-contents subset.

Shipped 2026-09-06 (`taste-mcp` → `ide_find`): the files, definitions,
issues, branches and commits half is answered on the blocking pool from
the checkout and its repository; the environments come from the fleet
rows; the terminals and chats half crosses to the GTK thread as
`OrchestrationRequest::Find`, which the console (the last five thousand
rows of each terminal, scoped) and the chat strip (every transcript row on
screen, scoped) answer. The answer carries the caveat in its own `note`.

## What is deliberately not here

- No fuzzy matching: `fltr` finding `filetree` is a guess the user did
  not make, and a box that guesses cannot say "no matches" honestly.
- No search history or saved searches: the query is a moment, not a
  document.
- No per-surface search boxes, ever again. A surface that cannot answer
  the one query has found a design problem, not a reason for its own
  entry.

## Known limits

- **Tabs cannot hide.** `AdwTabBar` has no per-tab visibility or styling,
  so tab sets are filtered through their pages menu rather than in the
  strip. Recorded as future work in the spike; the fix is a tab view that
  can hide pages without moving them.
- **Log and port tabs are not searched.** They are surfaces, not files
  (`editor.rs` → `SurfaceEntry`), so the open-buffers source does not see
  them. The environment log is searched through the console's listing
  instead; the IDE log tab is the one log with no source yet.
- **A listing's hits are the pane's.** The chat listing is the selected
  conversation's; the other conversations answer as counts on their
  backlog rows, and `ide_find scope=fleet` is how their lines are read.
- **Terminal scrollback is read on the GTK thread.** VTE owns it; the
  search is chunked (rows per frame) so the UI stays responsive, and a
  ten-thousand-line scrollback takes a few frames.

## By meaning

The one query is a text query: it finds the word. An agent's other
question — "where is authentication handled?", "what decides whether a
write is allowed?" — has no word to find, and VS Code answers it with
semantic search over a workspace index
(`#codebase`). Here that is `taste-semantic` and the MCP tool
`ide_semantic_search` (docs/spikes/agent-workspace-context.md):

- **Local.** One pinned embedding model (nomic-embed-text v1.5, the
  maintainers' own GGUF, `taste_semantic::EMBEDDING`, fetched once by
  `taste-models` with its digest checked) runs through llama.cpp on the
  CPU, in the `taste-embed` helper process beside the IDE. Nothing about
  the code or the question leaves the machine, which is the same rule as
  voice.
- **One store, a manifest per checkout, kept current.** The workspace's
  vectors live once, content-addressed by the hash of each chunk's text
  (`semantic/vectors.bin` under the workspace's state directory); each
  checkout — the primary, every environment's clone — has only a manifest
  of its files and their chunk hashes (`semantic/manifests/`). Environments
  are clones of one repository, so indexing one costs hashing and chunking
  plus embedding the chunks no checkout has seen: its own diff, and
  nothing else; and every environment answers from its own tree, never
  the primary's answer for a file its agent changed (David, 2026-09-07:
  "Are efficient derivatives for each env possible, or should the various
  envs just query the base index?"). The primary is indexed when the
  window opens and re-indexed, debounced, when git says the tree changed
  (`taste-app/src/semantic.rs`); an environment's clone the first time an
  agent asks. Chunks are cut on content — a blank line or a definition
  starts one, once the current one has eight lines, and none runs past
  forty — so an edit disturbs the chunk it lands in and that chunk alone
  is embedded again ("Can the index be incrementally freshened?"). Text
  files only, `.gitignore` honoured, binaries and files over 256 KB
  skipped; a chunk no manifest refers to leaves the store; the format is
  rebuilt, never migrated, when it changes.
- **For agents.** `ide_semantic_search { query, limit }` returns the best
  chunks — path, line range, text, score — and says "indexing" or
  "unavailable" honestly when it cannot answer yet, so the agent falls back
  to `ide_find`. The instructions tell every agent what it is for.
- **For the person at the box, too** (David, 2026-09-07: "I'd like to be
  able to use this for results for myself, too … amend the current,
  literal hits with the ML/AI ones"). A quarter second after the last
  keystroke the window asks the index once, off the main thread, and what
  comes back at or above `MEANING_FLOOR` joins the literal answer: in the
  tree, a file the word is not in but the idea is stays visible with a
  sparkle pill (the same hue's pill, the sparkle saying how it was found)
  and opens at its
  first such place, and the files banner adds "· N files by meaning" so
  what the badges add is counted in words; a query typed while the index
  was still building is asked again when it lands; in the editor's
  listing, the file on screen's chunks
  appear under **By meaning**, after the literal lines, each with its lines
  and how alike it is. A toggle beside the ghost (`taste-meaning-symbolic`,
  the sparkle) includes or excludes them, on by default. Nothing else
  changes: the counts a badge shows are still literal counts, and Ports,
  Logs, the backlog, terminals and transcripts are not in the index.
- **What it costs.** This repository — 190 files, 5,343 content-cut
  chunks — takes about twelve minutes to embed the first time on twelve
  threads (728 s), then under a second for the chunk an edit landed in; a
  question takes 76 ms. The helper holds the model, some 250 MB resident
  while it lives; a second helper for questions doubles that while both
  are up.
- **While it builds**, the meaning button says so: disabled, with a grey
  pill of the whole minutes left (`Nm`, rounded up, an ellipsis until the
  plan pass has counted what there is to embed), and the chunk count in
  its tooltip (David, 2026-09-07: "show that instead as a disabled AI
  button with a gray pill showing simply 'Nm' … The tool tip can provide
  chunk progress details"). The pill going is the whole announcement —
  no toast when it finishes. A gauge beside the box did this first, and
  was the first thing the narrow rungs had to hide.
