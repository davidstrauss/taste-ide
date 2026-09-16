---
title: The agent home volume is re-keyed per workspace and agent, so one sign-in serves the fleet
state: open
reporter: i-0036
started_by: david@davidstrauss.net@phoenix.davidstrauss.net
agent: claude-code
model: opus[1m]
created: 2026-09-16T02:26:54Z
updated: 2026-09-16T03:10:12Z
labels: environments, security, agents, follow-on
---

**Split out of i-0036**, which made the Anthropic credential, the private model, and the model listing the *project's* (keyed by the checkout's root, no machine-wide fallback). i-0036's third bullet asked for the same widening one layer down — the **agents' own** sign-in state, which is Gemini's and Copilot's, not Anthropic's — and its body allows exactly this: "If that half proves larger than the rest, it is a follow-on issue, filed and linked, not a widening."

**Wanted.** `docs/spikes/secret-service.md` § Phase 4: the agent home is a volume **per environment** (`taste_core::environment::env_home_volume`, `taste-<workspace-key>-<env>-home`), so a Copilot or Gemini sign-in has to be repeated in every environment. Key it by `(workspace, agent)` instead — like the auth proxy, which is workspace-wide — and one sign-in serves the fleet. The credential is the user's GitHub or Google identity, not an environment's.

**Why it is not a rename.** The per-environment key is load-bearing and its own comment says why: "The single-environment scheme used one machine-global `taste-agent-home` for every workspace on the machine. That was already wrong (two projects shared one agent history); with N environments it would also mean N agents writing one home concurrently." Widening the key back to the workspace re-creates precisely that concurrency, which is the thing that has to be designed rather than typed:

- Which parts of the home are identity (`~/.config/gh`, Gemini's OAuth credentials file — shareable) and which are conversation and cache (`~/.claude` history, adapter caches — not). A single volume cannot be both, so this is likely a split: a shared per-`(workspace, agent)` identity volume mounted beside a per-environment home, not a re-key of the one that exists.
- Two agents of the same kind writing one identity volume at once — a re-login in one environment while another is mid-turn.
- The rule CLAUDE.md states for the home: "the cwd and the home volume are identical in both topologies. Changing either at one spawn site and not the other silently loses every conversation." Every spawn site moves together or none does: `taste_acp::aim`, `relocate`, the auth terminal (outside-confined **always**, even for a relocated chat — Phase 3's asymmetry), and `taste_devcontainer::supervisor`.
- Existing volumes are orphaned by the re-key. `legacy_container_name` and the startup sweep are the precedent for reporting what a re-key orphaned rather than leaking it.

**Not this.** The Anthropic credential is settled and out of scope here: it never enters a container at all (`taste-authproxy` → threat model), and i-0036 scoped its file to the project. This issue is only about the sign-in state the agent CLIs write into their own home.

**Gate.** A test that two environments of one workspace resolve one identity volume and two conversation homes; a test that the spawn sites agree, in the style of `a_login_hint_pins_the_adapters_own_version`; the sweep reporting orphans; ENVIRONMENTS.md beside the note that worker agents from other providers "keep their own credentials", and the spike's Phase 4 marked as done or amended by what was actually built.
