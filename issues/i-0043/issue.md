---
title: Move private-model setup from README into the chat configuration
state: open
reporter: primary
created: 2026-09-16T04:11:40Z
updated: 2026-09-16T04:11:40Z
labels: enhancement, agents, ui, docs
---

**What is wanted.** The private-model setup currently requires users to derive a workspace state directory from a `<folder-name>-*.json` manifest, create it manually, and write `private-model.json` from a README snippet. Expose this configuration near the chat's model controls instead, so a user can enter the private model's base URL, API key, model name, and context limit without shell commands or restart-dependent setup.

**Where.** The README section that documents the `private-model.json` shell snippet, the chat's model configuration UI, and the workspace-state persistence that supplies the private model.

**Done looks like.** The in-app configuration persists the private model in workspace state, makes the configured model selectable for a chat, and replaces or removes the manual README setup step. Any remaining prerequisite, such as starting a compatible server, is documented in the in-app path or README as appropriate.

**Constraints.** Preserve the existing project-scoped credential and private-model behavior. Follow `docs/ARCHITECTURE.md`'s IDE-mediated agent and reload-without-restart commitments. Verify the configuration is durable and usable from the chat. Anything discovered outside this outcome is filed as a separate issue rather than widening this one.
