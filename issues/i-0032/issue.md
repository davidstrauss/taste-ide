---
title: An environment announces itself available before podman will accept an exec into it, so every start wastes an agent process
state: open
reporter: i-0011
created: 2026-09-16T01:57:05Z
updated: 2026-09-16T01:58:45Z
labels: environments, orchestration
---

**What is wrong.** The IDE reports an environment available — and its `ExecContext` reports `has_exec_target()` — while podman will still refuse to exec into the container. Two seconds later the agent spawns into it and dies at once:

```
01:27:45 INFO taste_ide::window: environment i-0028 is available
01:27:48 INFO taste_ide::chat: chat i-0028: spawning its agent (resume=none)
01:27:48 INFO taste_ide::chat: chat i-0028: connection closed (Process exited with exit status: 255:
         Error: can only create exec sessions on running containers: container state improper)
01:28:04 INFO taste_ide::chat: chat i-0028: spawning its agent (resume=none)
```

The same shape is in i-0011's body for i-0007 and i-0009, and in its 2026-09-16 comment for i-0028: nine starts, every one of them.

**This is no longer a correctness bug.** i-0011 made the prompt survive it — a brief is held and delivered whenever an agent does come up, a prompt orphaned by the dying process goes back on the queue, and a chat that died before its first `Ready` reconnects instead of going quiet. So nothing is lost. i-0011's "done looks like" offered two fixes joined by *and/or*, and that is the one it took.

**What is left is waste, and it is not nothing.** Every start pays for an agent process that cannot live, its `Closed`, a reconnect backoff (measured above: sixteen seconds), and a second spawn. Sixteen seconds per start is the cheap reading; the expensive one is that the log of every healthy start contains an `exit status: 255` and a `connection closed`, which is noise in exactly the place someone looks when a start really has gone wrong.

**Where it lives.** `ExecContext::has_exec_target()` answers off `Target::Container`, which the supervisor aims when its lifecycle run reports the container started. `container_gate` (`crates/taste-app/src/chat.rs`) reads exactly that to decide `Gate::Spawn` versus `Gate::Hold`, so the question is not the gate's — it is what "started" is allowed to mean. Somewhere between `podman start` returning and `podman exec` being willing there is a window the supervisor does not account for.

**Done looks like** an environment not announcing itself available, and not reporting an exec target, until an exec into it would succeed — so the first spawn is the only spawn. A probe (`podman exec … true`, bounded and backed off) is the obvious shape, and the thing to be careful about is the rung below the containers: a readiness check that can never pass must not hold an agent back forever, which is the same trap `container_gate`'s "every no ends in a spawn" was written around.

Filed out of i-0011 rather than folded into it, on that issue's own instruction not to widen.
