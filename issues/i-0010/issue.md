---
title: "Publish" is one word from "push", so agents commit and stop — the last step of an issue gets swallowed by the never-push rule
state: open
reporter: primary
created: 2026-09-08T16:42:54Z
updated: 2026-09-08T16:42:54Z
labels: bug, agents, docs
---

**What is wrong.** Finishing an issue in an environment takes three acts: commit, publish, and never push. Two of them are named in the house rules and one is not, and the one that is missing is the one that hands the work back. An agent that has internalized "never push" reliably treats publishing as the forbidden thing and stops after committing.

**How it was found.** i-0004, 2026-09-08. The brief said, verbatim: "When the gate passes, commit and publish with `ready: true`. Do not push." The agent ran the gate clean, committed `8ed1a5a`, and reported:

> Committed as `8ed1a5a`. The rebuilt container's gate passed clean: `fmt --check`, `clippy -D warnings`, and `cargo test --workspace` (0 failures across 32 test binaries). **No push was made, per the standing rule.**

It then went idle, treating the issue as done. It had been told to publish in the same sentence. It honoured the prohibition and dropped the requirement, because the two words describe acts that sound identical and only one of them is dangerous.

**Why this is a design fault and not one agent's mistake.** CLAUDE.md's house rules say "Commit per verified batch; never push," and say nothing about publishing. So an agent reading the project's own standing instructions learns a two-step finish for a three-step job, and the step it never hears about is the one that moves `agents/<env>` and raises the review flag. The prohibition is stated in the strongest terms available; the requirement is stated once, in a briefing that agent may never have read. The asymmetry decides the outcome.

It also fails in the safest-looking direction, which is why nobody catches it: the agent believes it has finished, its transcript reads like success, and the work is complete and correct — it is simply somewhere no one else can see. See the companion issue on unpublished work reading as "working".

**Which way to fix it.** Open, and worth some care:

- At minimum, name all three acts together everywhere the two are currently named — CLAUDE.md's house rules, `docs/ARCHITECTURE.md`, and the standing instructions given to a non-primary environment — with the distinction stated explicitly: publishing moves your own branch of record inside the user's checkout and is entirely local; pushing sends commits to the user's remote and is never yours to do.
- Strengthen the publish tool's own description so the distinction arrives at the point of use, where an agent about to go idle will actually read it.
- Consider renaming the act to something that cannot be confused with pushing — "hand back", "submit for review". That is a larger call with a documentation and habit cost, so treat it as an option to weigh in the PR, not a decision this issue has made.

**Done looks like.** An agent that has committed and is about to go idle with unpublished commits cannot mistake that state for finished — because it has been told, at the point of use, in words that do not collide with the prohibition it already knows. The three acts are named together wherever any of them is named.

**Rules in force.** CLAUDE.md, "Commit per verified batch; never push" — this issue does not weaken that rule, it adds the step beside it. The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
