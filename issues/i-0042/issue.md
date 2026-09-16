---
title: Decide the outside rung: give Claude Code a workspace where no container exists (ROADMAP A or B)
state: open
reporter: i-0027
created: 2026-09-16T03:33:23Z
updated: 2026-09-16T03:34:16Z
labels: design, agents, acp
---

Left open by i-0027 (2026-09-16), which fixed the respawn topology (an agent no longer comes back outside a container it could be inside) and the honesty around it, but not the rung itself.

## What is known

The pinned Claude Code adapter never routes Read/Write/Edit through the ACP client (`build-aux/acp/fs-probe.mjs` shows zero `fs/read_text_file` with the capability advertised; ROADMAP.md § "one assumption under it is false"). Outside a container — the sibling agent container or bwrap, the "rung of last resort for a broken substrate" — its file tools see the read-only stand-in workspace and nothing of the project. After i-0027 the IDE reaches that rung only where no container will come: no podman, or a build that failed. There the agent can talk, search (`ide_list_files`, `ide_search`), file issues, and read `devcontainer_logs`, but cannot read or write a project file.

## The choice (ROADMAP → "Three ways out")

- **A. MCP file tools** (`ide_read_file` / `ide_write_file` / `ide_edit_file`) wrapping the IDE's existing, tested fs handlers — keeps every property mediation was for, including reads from unsaved editor buffers, and works in every topology. Costs: the model must prefer them over its native tools (the MCP instructions can say so), and the transcript's Edit/Write cards would come from a different tool.
- **B. Read-only checkout bind** into the outside-confined sandbox at the checkout's host path — native reads work (stale disk, no buffer awareness), writes fail EROFS and the IDE path stays for agents that use it. Cheap; makes the stand-in mostly dead code.
- Or accept the rung as blind, now that it says so.

## Acceptance

- A decision recorded in ROADMAP.md, and either A or B shipped, or the blind rung documented as accepted.
- If A: the default agent, prompted to read and edit a project file while on the stand-in, does so through the IDE (the INFO lines `fs/read_text_file for <env>: …` in the app log show it).
