# Roadmap & UX ledger

Candidate work, ordered by how much it advances "superlean but
hyperfunctional." Each item must clear the bar in ARCHITECTURE.md: no
extension points, conventions over configuration, agents through ACP.

## UX-convention ledger (2026-08 audit)

Done in this pass:
- Primary menu (hamburger) with About and Keyboard Shortcuts — the HIG
  staple the window was missing.
- Ctrl+P quick-open over the existing search index; Ctrl+Q graceful quit.
- Markup escaping for file names in row titles.

Standing conventions to keep honoring:
- Disable, never hide; no modal dialogs for dirty-file workflows (bottom
  intervention panel); toasts for outcomes; StatusPage for empty states;
  destructive styling + confirmation only for destructive actions.
- Known debt: the file pane header is crowded (new-file, three git
  filters, ignored toggle); consider a split into a toolbar row once one
  more control appears. The chat "Auto-approve" switch overlaps
  conceptually with the agent's Auto mode — consolidate when the ACP
  permission story settles. One stray `GtkGizmo snapshot without
  allocation` warning remains unexplained (cosmetic; watch it).

## Near-term features

0. **Multi-chat tabs** (user-requested, designed, next up). The Chat tab
   strip becomes an AdwTabView: a + button opens a new chat (fresh ACP
   session, same agent), tabs close (ending their session; the wire and
   UI teardown all exist on ChatPane already). Each tab IS a ChatPane —
   session, transcript, composer, and per-session settings travel
   together, which the mode/model semantics already assume. Window-level
   routing (state persistence, sign-in completion, destroy-session
   toast, commit-message suggestions) addresses the SELECTED page's
   pane. Persist the session-id LIST in WorkspaceState (open_chats:
   Vec<...>, additive-compatible with the existing single field).
   Also queued from review: commit box appears when staged > 0 (any
   view), accent on the non-zero Staged count, auto-switch to Staged
   after a bulk Stage.

1. **Fork / rewind from a transcript point** (user-requested). The adapter
   advertises `sessionCapabilities: fork`. Plan: per-prompt-card menu —
   "Fork conversation from here" creates a forked session via ACP and
   switches the pane to it; "rewind code" maps to Claude Code checkpoints
   when the adapter exposes them over ACP (probe first; the schema side is
   unstable). Transcript cards already track their prompt index.
2. **Diagnostics via LSP, curated in-tree.** One bundled language server
   per first-class language (rust-analyzer ships in the devcontainer
   already), inline squiggles + a problems row in the console. No plugin
   surface: language support is a curation decision.
3. **Richer subagent visibility**: nested tool cards per Task invocation.
4. **Chat polish parity**: syntax highlighting inside diff cards, styled
   terminal-output sections on tool cards, thought-duration labels.
5. **Commit flow completion**: push button surfacing sync state more
   prominently after commit; changed-files funnel counts in the filter
   toggles ("Dirty (7)").

## 2026 bets (superlean, hyperfunctional)

- **The working set is the review unit.** Deepen the Dirty/Staged/Stashed
  filters into a first-class "review before commit" flow: keyboard-driven
  next-change/prev-change walking the changes faces of dirty files.
- **Agent-native affordances over IDE chrome.** Prefer MCP tools +
  transcript cards over new panes: e.g. "explain this diff", "test this
  hunk" as chat slash-commands operating on the working set, not buttons.
- **Session portability.** Sessions already live with the agent; surface
  "continue on another machine" by syncing only ids + the repo (state file
  is already industry-idiomatic JSON).
- **Zero-config service topology.** Lean into systemd + socket activation
  (Services tab) as *the* way projects run daemons; devcontainer templates
  that pair `foo.service`/`foo.socket` ghosts next to `.editorconfig`.
- **Explicitly out**: plugins, themes beyond light/dark, per-project IDE
  scripting, embedded browsers, telemetry.
