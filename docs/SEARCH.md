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
   surface that wants to be searchable subscribes to the one query. Four
   inputs that did not know about each other (find-in-project, the
   environments filter, Ctrl+P, and the MCP `ide_search`) were the
   heterogeneity this replaces.
2. **Filter in place where the surface is a list; list the hits where it
   is not.** A tree, a queue, a set of tabs, a menu of branches — these
   are rows, and rows can hide. A file's contents, a terminal's
   scrollback, a chat's transcript — these are documents, and a hit needs
   the line around it. Documents keep their content and gain a *results
   listing*, a bottom panel in the pane whose content it enumerates (the
   intervention-panel convention: never a modal). Activating a hit goes
   there.
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
   often as a hit is.

And one toggle: **highlight without filtering** (the ghost, beside the
box). Where a surface would hide rows it dims them instead, so the shape
of the whole is kept while the matches stand out. It affects only the
filtering surfaces; listings are listings either way.

## What is searched

| Surface | Kind | Source | Cost |
| --- | --- | --- | --- |
| File names and paths | filter | the file index (`collect_files`, rebuilt on git changes) | trivial |
| File contents | listing (editor pane) + reachability count in the tree | `search_files_complete`: every file, complete counts, lines capped per file, cancellable, progress per file | one pass over the index; runs on the blocking pool |
| Open buffers | listing (editor pane) | the buffer text of every open page, so unsaved edits are searched | trivial |
| Symbol definitions | listing (editor pane, first) | `taste_core::search::symbols`: definitions by language convention (`fn`, `struct`, `def`, `class`, `function`, …), indexed with the file index | one pass, cached with the index |
| Commit messages | listing (editor pane, last) | `taste_git::search_commits`: the last two thousand commits on HEAD | one revwalk, blocking pool |
| Branches | filter (the branch menu) + count on the button | the branch list the tree already holds | trivial |
| Backlog: issues and environments | filter, with a running rule while inner sources search | title, id, body, comments; plus counts of hits *inside* each environment's chat and terminals | strings trivial; inner hits arrive as those sources finish |
| Editor tabs and terminal tabs | pages menu filters to matches | page titles | trivial |
| Terminal scrollback | listing (console pane) | the text of every shell's scrollback, read row by row, matched line by line, in bounded chunks per frame | on the GTK thread by necessity, so chunked and cancellable |
| Environment log | listing (console pane) | the devcontainer log buffer | trivial |
| Chat transcripts | listing (chat pane) + count on the environment's row | the rows on screen, walked for their text, for every environment's chat | trivial per chat |

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
  where it was.
- **Down** steps to the next result in the panel that was focused before
  the box took focus — the tree if the user was in the tree, the editor's
  listing if they were in a file, the console's if in a terminal, the
  chat's if in the chat. **Up** steps back. A panel with no results
  still opens its listing to say so.
- **Tab** moves the stepping to the next panel that has results, in
  reading order (tree, editor, console, chat); **Shift+Tab** the other
  way. The panel being stepped shows it as a focused list does.
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
  them; the environment log listing this table promises for the console
  pane has not been built yet either. When it is, the log tab's buffer is
  the natural source.
- **Terminal scrollback is read on the GTK thread.** VTE owns it; the
  search is chunked (rows per frame) so the UI stays responsive, and a
  ten-thousand-line scrollback takes a few frames.
