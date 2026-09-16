---
title: Every environment re-downloads the same pinned adapter: the npx cache is per environment
state: open
reporter: i-0037
created: 2026-09-16T03:31:18Z
updated: 2026-09-16T03:31:18Z
labels: environments, agents, performance
---

**Found during i-0037**, which split the sign-in out of the agent home and deliberately left the caches where they were.

Every agent in the registry is launched as a pinned npm package (`npx -y @agentclientprotocol/claude-agent-acp@0.73.0`, `@google/gemini-cli@0.58.0`, `@github/copilot@1.0.82`), and every `AgentSpec` lists `.npm` among its `home_paths` — npx's package cache, inside the **per-environment** agent home (`taste_core::environment::env_home_volume`). So N environments of one workspace fetch and store N copies of the same pinned tarballs, and a fresh environment's first agent spawn pays a download before it can answer anything.

**Why it was left alone in i-0037.** That issue's line was identity versus conversation, and a cache is neither: sharing it is a performance argument, not a correctness one, and it deserves its own measurement rather than a free ride on a credentials change.

**What makes it plausible rather than obvious.** `cacache`, npm's content-addressed store, is built for concurrent readers and writers — that is what makes a shared cache safe where a shared `~/.claude.json` is not. Whether it survives six containers on one podman volume through virtiofs is a thing to test, not to assume.

**Rough shape.** A third volume keyed by workspace alone (the packages are not per agent — three agents share one `.npm`), mounted at `/home/agent/.npm` at the same five sites the identity volume goes to, using `taste_core::policy::agent_identity_mount`'s pattern. It would not be an *identity* mount and should not be modelled as one: no agent id, no `AGENT_IDENTITY_DIRS` entry, and no place in a table whose whole point is "this is a credential".

**Worth measuring first**: how long a cold `npx -y <pinned>` actually takes in a fresh environment, and how much disk N copies take against `MAX_ORCHESTRATED_DISK_BYTES`' neighbours. If it is a few seconds and a few tens of megabytes, this is not worth a fourth volume in every container.
