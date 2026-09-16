---
title: Safe mode cannot be a recovery console for the default agent: the baseline binds the checkout read-only and Claude Code's writes never reach the IDE
state: open
reporter: i-0027
created: 2026-09-16T03:33:12Z
updated: 2026-09-16T03:33:12Z
labels: bug, agents, acp, safe-mode
---

Found while working i-0027 (2026-09-16), and it is the same fault seen from the safe-mode side: i-0036's `fs/write_text_file` "EROFS while `ide_write_policy` says writable".

## The story as written

`ide_write_policy` in safe mode says: "Focus on authoring or fixing the devcontainer configuration … then devcontainer_reload to build and start it." CLAUDE.md: "the agent authors and the USER applies." ENVIRONMENTS.md § Two modes: the baseline binds the checkout **read-only** on both binds "while writes remain IDE-mediated through `write_allowed`'s safe-mode scope".

## Why it does not hold for the default agent

The pinned Claude Code adapter (`@agentclientprotocol/claude-agent-acp` 0.73.0) never calls the ACP client's `fs/write_text_file` (proved in i-0027 with `build-aux/acp/fs-probe.mjs`; ROADMAP.md § "one assumption under it is false"). Its `Write`/`Edit` act on the filesystem where its process runs. In safe mode that process is relocated into the **baseline** container, where the checkout is bound read-only, so every write — `.devcontainer/` included, the one scope the policy allows — fails with EROFS. The "IDE-mediated" write path exists and enforces the safe-mode scope correctly; the default agent just never takes it. So the agent that safe mode exists to let repair the devcontainer config is the one agent that cannot write it.

`ide_write_policy` now reports the agent's topology (i-0027), but for a relocated baseline agent the topology is "beside-files" and the mount is still read-only: the policy's `writable` list is honest about the IDE's rule and wrong about what that agent can do.

## Options

- Bind the checkout read-write in the baseline for exactly `write_allowed`'s safe-mode scope (`.devcontainer/`, the dotfiles) — a mount that agrees with the policy rather than being "strictly more restrictive" than it.
- Or route the default agent's writes through the IDE (ROADMAP option A, MCP file tools), which fixes this and i-0027's outside rung at once.
- Or make `ide_write_policy` say, in safe mode with this agent, that nothing is writable through its tools and the user must edit `.devcontainer/` themselves.

## Acceptance

- In safe mode, an agent asked to fix `.devcontainer/` can either do it, or is told by `ide_write_policy` (before trying) that it cannot and why.
- No path answers "writable" for a file the asking agent's own tools cannot write.
