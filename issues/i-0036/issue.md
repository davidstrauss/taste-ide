---
title: Credentials are the project's, not the machine's: the proxy reads a per-workspace file and never falls back
state: completed
reporter: primary
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: opus[1m]
created: 2026-09-16T02:12:34Z
updated: 2026-09-16T03:10:10Z
labels: feature, authproxy, security, environments, models
---

**Wanted.** Authenticating one project must not authenticate another. Work projects use work agent APIs, personal ones a personal account, on one machine, without the user having to remember which file is in force. David, 2026-09-16: "I don't want to auth my personal projects to certain work agent APIs, for example."

**Where the file lives, said first because it is the easiest thing to get wrong.** Project-scoped means *keyed by* the checkout's root path, never *inside* the checkout. The file stays on the host under `$XDG_STATE_HOME/taste-ide/`, in a per-workspace directory named by a hash of the root, the way `taste_core::state::file_for` already keys the workspace state file. Nothing is written into the working copy, hidden or otherwise, so nothing can be committed (David: "I don't want it actually in the working copy, even as a hidden file, because it risks getting committed"). ARCHITECTURE.md → Conventions already states the rule: secrets and caches are state, not configuration, and have no checkout path that would be right for them. Two consequences to write into the docs: the same repository cloned at two paths is two scopes, each provisioned on its own; and an environment's clone inherits the project's credential with no file of its own, because the clone's proxy is the workspace's proxy and the clone only ever sees a placeholder.

**Today.** `taste_authproxy::credentials::credential_path()` is `$XDG_STATE_HOME/taste-ide/anthropic.json`, read by every workspace's proxy on the machine. `taste_authproxy::private::private_model_path()` sits beside it and scopes the same way. Both are machine-wide. The two documented environment variables (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`) and the aimed-path variables are machine-wide by nature. Placeholders are per environment and the real key never enters a container (`taste-authproxy` → threat model), so the container half needs nothing. The agents' own sign-in state (Gemini, Copilot) lives in the agent home volume, which `environment::env_home_volume` keys per environment, narrower than the project.

**Shape.**

- Both files move under the workspace's state directory: `anthropic.json` and `private-model.json` beside the state file, keyed by root. `credential_path` and `private_model_path` take the workspace root; `IdeCredentials` and `private::discover` are constructed with it. The proxy is already one per IDE process and one process per workspace, so nothing about routing changes.
- Resolution is aimed path, then the documented environment variables, then the project's file, then **nothing**. No machine-wide file is consulted. A project with no credential refuses its first request naming the provisioning step, exactly as an unprovisioned IDE does today. The fallback is the decision here: a machine-wide default would be the very mechanism that lets a work key into a personal project, and its absence is the point.
- Each file may carry an optional `label` ("work", "personal"). The chat header shows it beside the Plan gauge and the Utilization tab's Subscription section names it, so the identity in force is visible where spend is shown. Decide and write down what the header shows when there is no label: the file's kind, or nothing.
- The agent home volume is re-keyed from per environment to per workspace and agent, per `docs/spikes/secret-service.md` § Phase 4, so a Gemini or Copilot sign-in serves the fleet of one project and no other. If that half proves larger than the rest, it is a follow-on issue, filed and linked, not a widening.

**Migration.** An existing machine-wide `anthropic.json` is not silently adopted by every project: that would reproduce the leak the issue exists to close. On first launch of a workspace with no project file and a machine file present, the IDE offers once, in the chat where the first refused request lands, to copy the machine file into this project, and says which account it is copying. Alpha rules apply to the state layout (`state.rs` → the version note): a reset is told to the user once.

**Rules in force.** The boundary is the host; nothing here changes what a container can reach. The IDE reads no other program's storage. GTK objects never leave the main thread; the file reads stay on the request path or in `spawn_blocking`. Documented mechanisms only. Oxford commas in everything written. Commit per verified batch; never push.

**Gate.** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus: a proxy test that two workspace roots resolve two different credentials and that a root with none is refused rather than served by the other's; a test that the private-model file scopes the same way; and a probe of the header label at 1440x900 and at the consolidated rung, since it is chat-pane work. `docs/ENVIRONMENTS.md` → The auth proxy gains the scope and the no-fallback rule; ARCHITECTURE.md's Conventions table rows for the three state files gain the per-workspace path; README → "A private model on your own hardware" step 6 names the new path.

**Not in scope.** A UI for pasting the credential, which ENVIRONMENTS.md already records as a UX gap. Anything found along the way is a new issue.
