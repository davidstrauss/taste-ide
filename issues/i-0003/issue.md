---
title: Check that credential handling is done responsibly.
state: open
reporter: primary
created: 2026-09-08T05:03:08Z
updated: 2026-09-08T05:06:15Z
---

**What is wanted.** A written verification — not a rewrite — that every credential passing through the IDE is held, passed and destroyed on the host side of the boundary CLAUDE.md defends, and that none of them is reachable by an agent or by anything running in a container. The posture is already deliberate and documented in module prose; this issue confirms the code matches the prose, and names where it does not.

**The surfaces to cover.** Each one gets a verdict in the report:

1. **The Anthropic credential** — `crates/taste-authproxy/src/credentials.rs`. Where it is stored (`credential_path()`, line 333: `$XDG_STATE_HOME` or `~/.local/state`), with what file permissions, and whether the store is created with them rather than relaxed later. `Debug` is redacted for both variants (lines 89–90) — check every *other* path a credential could reach a log, an error message, or a `Context` string, including the `with_context(|| format!("reading {}", …))` sites that name the path.
2. **What the agent gets instead** — `crates/taste-acp/src/authproxy.rs:152` (`spawn_env`): `ANTHROPIC_BASE_URL` on loopback plus a minted placeholder. Confirm the real credential is never in the spawn environment, in either topology, and that the placeholder is per-environment and revocable as the docs claim.
3. **The sandbox's environment** — `crates/taste-acp/src/sandbox.rs`. bwrap gets no `--clearenv` (noted in `docs/spikes/secret-service.md`), so the agent inherits the IDE's environment: enumerate what actually crosses, and confirm no credential-bearing variable is among it.
4. **The relocated agent's reach** — the unix socket the proxy also serves, and the environment channel that carries it in. Confirm the socket grants no more than the loopback port does.
5. **Git and push** — `crates/taste-git/src/lib.rs:548,561`: push runs against the user's credential helpers and ssh-agent. Confirm that path is user-initiated only and that no agent-triggered call site can reach it.
6. **Podman substrate** — `crates/taste-devcontainer/src/substrate.rs`: a remote podman connection carries an ssh identity. Confirm where it comes from and that a container cannot reach it.
7. **Model fetch** — `crates/taste-models`: confirm it is genuinely unauthenticated (digest-pinned, no token), or say what it sends.
8. **Outward reporting** — `crates/taste-fleetlink`: spend counters and per-environment state leave the process. Confirm nothing credential-shaped rides along.

**Explicitly out of scope.** The secret service for agents (`docs/spikes/secret-service.md`) is designed and unbuilt, including the plain-text GitHub token that Copilot sign-in currently ends in. That is its own work — reference it, do not start it.

**Done looks like.** A document at `docs/spikes/credential-audit.md` with one verdict per surface above, each citing file and line; no code changes except test additions that pin an invariant found to be untested. **Every defect found is filed as its own issue, not fixed here** — the audit's value is the map.

**Rules in force.** CLAUDE.md: "the boundary is the host, not the agent"; "neither agents nor the repo are trusted"; "mediation is user experience, not a gate" — do not credit a mediation layer as a security control. `taste_core::policy::write_allowed` is the single source of truth for write checks; do not reimplement it.
