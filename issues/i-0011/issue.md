---
title: issue_start loses the first prompt when the agent spawn beats its container, so a claimed issue sits with no task
state: open
reporter: primary
created: 2026-09-08T16:53:39Z
updated: 2026-09-08T16:53:39Z
labels: bug, environments, orchestration
---

**What is wrong.** `issue_start` creates the environment, claims the issue, and then fails to deliver the first prompt if the agent process is not live at that instant — returning `"<id> exists but did not take the issue; chat_send can retry it"`. The environment is left claimed, running, and idle, holding no task. Every start today has hit this: i-0004, i-0006, i-0007, and i-0009, four for four. Each was recovered only because the coordinator noticed and re-sent the brief by hand with `chat_send`.

**This contradicts the tool's own description**, which is the contract callers are entitled to trust:

> Its container is started FIRST and the agent starts inside it, so it has a shell from its first turn; **the first prompt is queued while the container comes up**, and chat_status says when it has.

The prompt is not queued. It is dropped, and the caller is handed a refusal instead.

**The trigger, from the IDE log.** The environment announces itself available before podman will accept an exec into it:

```
16:32:06 INFO taste_ide::window: environment i-0007 is available
16:32:06 INFO taste_mcp::server: MCP server listening for environment i-0007 on …-i-0007-mcp.sock
16:32:08 INFO taste_ide::chat: chat i-0007: spawning its agent (resume=none)
16:32:08 INFO taste_ide::chat: chat i-0007: connection closed (Process exited with exit status: 255:
         Error: can only create exec sessions on running containers: container state improper)
```

Two seconds. The same shape for i-0009, where the spawn is simply retried and the caller has already been refused:

```
16:50:59 INFO taste_ide::window: environment i-0009 is available
16:50:59 INFO taste_mcp::server: MCP server listening for environment i-0009 on …-i-0009-mcp.sock
16:51:02 INFO taste_ide::chat: chat i-0009: spawning its agent (resume=none)
16:51:07 INFO taste_ide::chat: chat i-0009: spawning its agent (resume=none)
```

`b85b8ac` ("Outside the user's own environment, the container starts before the agent") already established the ordering, so the sequencing intent is there. What is missing is that "available" is being reported on a container podman will still refuse to exec into, and that the agent's failure to come up is treated as terminal for the prompt rather than as something to wait through.

**Why it matters.** A claimed issue with no task is worse than a refused start, because it looks started. The environment is spent, the cap is consumed, the panel shows it live, and nothing is happening inside it. A coordinator that does not read its own tool result carefully will move on and the issue will sit forever. It also costs a slot against a cap of six, which today meant a second issue could not be started at all.

**Done looks like.** Starting an issue either delivers its first prompt or creates nothing. Concretely: the prompt survives the agent not being live yet — held and delivered when the connection comes up, which is what the description already promises — and/or "available" is not announced until an exec would succeed. A test that makes the container slow to accept an exec and asserts the first prompt still arrives is what keeps this fixed; without one, the timing that produces it is invisible.

**A second failure with the same symptom, probably a separate defect.** i-0004 died four times before it ever ran a turn, with a different signature — the agent process exiting 243 on `npm error code EACCES / npm error syscall spawn sh`, against the host path `/var/home/straussd/.local/state/taste-ide/environments/799fd7acd369bf5c/i-0004/repo`. That is a spawn against a host state directory rather than an exec into the container, which is a topology question and touches CLAUDE.md's boundary rules, not a race. It is recorded here because it produced the same user-visible outcome — an environment that never took its issue — but it should be its own issue if confirmed distinct. Do not widen this one to cover it.

**Rules in force.** The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Note that `cargo test --workspace` currently SIGSEGVs nondeterministically in `taste-app`'s `chat::` GTK tests in the primary container — unrelated, recorded on i-0004.
