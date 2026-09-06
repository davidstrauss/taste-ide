# Spike: systemd services and journals in the devcontainer (shelved)

Shipped early (`git log -- crates/taste-devcontainer/src/services.rs`
for the history), **removed 2026-09-06**. David: "Drop the systemd
integration for now for services and logs. I just can't exercise it much
yet." This is what the implementation knew, written down so the feature
can be designed well rather than rediscovered if it comes back — and it
is expected to: the roadmap still wants socket-activated systemd units as
*the* way a project runs daemons, and the devcontainer conventions the
IDE hands agents (`ide_conventions`, the `.devcontainer` ghost template)
still say so. Only the UI and the plumbing behind it went; the container
side — `--systemd=always` allowed through `taste-devcontainer::security`,
the Docker-only run args ignored with a note — stays.

## What it was

A pinned, icon-only **Services** tab in the console strip, beside
Environment and Resources: a unit list on the left (name, description,
state, a "socket-activated · foo.socket" caption), and on the right the
selected unit's journal with Start / Stop / Restart / Reload, a "unit
files" viewer, and a Follow toggle. The tab's icon carried the summary —
on/off/warn/none glyphs (`data/icons/…/taste-services-*.svg`, also
removed), `needs-attention` when a unit had failed, the count in the
title. Two crates split the work:

- `taste-devcontainer::services` — the systemd side, no GTK:
  `list_services`, `service_action`, `journal_snapshot`, `unit_files`,
  and `tail_journal` (a `journalctl -f` on the tokio runtime, lines over
  an `async_channel`, killed on drop). Unit-tested against sample
  `systemctl --output=json` output.
- `taste-app::services` — the pane. Refresh on every environment data
  refresh; a disabled-not-hidden pane when there was no container or no
  systemd; the journal view loaded per selection with a generation
  counter so a stale tail could not write into a newer view.

## What it learned (keep these)

1. **Run as container-root through the exec context, never on the host.**
   `systemctl` and `journalctl` need root *inside* the container. Under
   rootless podman container-root is the user's own uid seen through the
   user namespace, so `podman exec --user root` grants nothing on the
   host — and the spawn still went through `ExecContext`, which refuses
   the no-container case. `ExecContext::resolve_root` existed only for
   this and was removed with it; bring it back as `resolve_as(Some("root"), …)`
   when needed, and keep the "host targets never sudo" rule.

2. **Unit names are argv.** They come back from systemctl, but they flow
   into later commands, so they were validated against systemd's own
   charset (`[A-Za-z0-9-_.@:\]`, no leading `-`, under 256 chars) before
   use. Defense in depth, and a one-liner.

3. **`systemctl list-units --type=service,socket --all --output=json`
   is one call for the whole picture.** Sockets fold into their service's
   `socket` field by stem (`web.socket` → `web.service`); the list sorts
   failed first, then running, then the rest, because failed is what the
   user is looking for. `systemctl show -p TriggeredBy` walks from a
   service to the socket that activates it, and `-p FragmentPath,DropInPaths`
   finds the unit files, drop-ins included.

4. **The pane's own width was the whole window's minimum twice over.** An
   unwrapped status label (1066px) blocked half-screen tiling; a
   `width_request(220)` on the unit-list sidebar pinned the console's
   minimum at 470 for a tab nobody was looking at, because an
   `AdwTabView` measures every page. Both are general lessons the rest of
   the UI now applies (`TASTE_MEASURE_MIN=1` attributes them): wrapping
   labels get `WordChar` and `max_width_chars`, and a comfortable width is
   `max_content_width`, never a floor.

5. **Journal tailing wants coalescing.** `journalctl -f` bursts; the pane
   drained the channel up to 256 lines per wakeup into one buffer edit
   and capped the view at 2000 lines, trimming from the top. A tail is
   tied to a *view* (unit + follow state) by a generation counter; a
   change of either bumps it, stops the old process, and lets late lines
   from the old tail fall on the floor.

6. **"No systemd" is not an error and must not be red.** The tab
   distinguished a running container without systemd (warn glyph, with a
   one-line recipe: a systemd-capable image, `--systemd=always` in
   `runArgs`, `"overrideCommand": false`) from no container at all
   (neutral glyph). Red stayed reserved for a unit that had failed.

7. **Unit files that live in the workspace open in the editor.** The
   viewer mapped container paths back to the checkout (`/workspaces/<name>/…`
   → the workspace root, or the identical path in the self-hosting case)
   and enabled Open in Editor only for files that exist there. A unit
   file outside the workspace is the image's business, and read-only.

8. **Disable, never hide.** When the container was down the pane kept its
   full shape and greyed out with the reason in one line — a pane that
   vanishes and reappears teaches nothing about where things are.

## What was wrong with it, and what a return should do differently

- **It was a fourth fixture in a strip that wanted two.** By the time it
  went, the console's identity had narrowed to "the selected environment's
  processes": terminals plus the podman Resources tab. Services and their
  journals are *logs and lifecycle*, and the direction the design is
  taking (the logs proposal of 2026-09-06) is that logs are opened from
  the file tree like files and read in the console strip as closable
  tabs. A unit's journal fits that shape exactly: **a journal is a log the
  tree lists under the environment, opened on demand, tailing by default**
  — not a permanent pane with its own sidebar, search box and toolbar
  (SEARCH.md rule: no per-surface search boxes, ever again; the pane had
  one).
- **Lifecycle actions belong with the environment's other actions**, in
  the backlog row's menu or the toolbar, not on a bar above a log.
- **The system journal as a pinned first row** was a good default view
  and should survive as "the environment's journal" beside its build log.
- **The MCP side never existed.** Agents could not list or read services;
  if it returns, `devcontainer_services` / `devcontainer_journal`
  read-only tools should come with it, mirroring `devcontainer_logs`, and
  lifecycle actions stay the user's (configuration authority is execution
  authority; starting a unit runs whatever the unit says).
- **Test it against a real systemd image before shipping the UI.** The
  reason it went was that nothing exercised it; the baseline environment
  does not run systemd, so a return needs a fixture image that does, and
  a probe view that shows a failed unit.
