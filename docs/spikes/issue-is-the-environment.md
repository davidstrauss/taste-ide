# Spike: an environment is an issue in progress

David, 2026-09-05: "Except the primary one for me, the env is just an
active manifestation of a backlog item. We get to lose a whole panel. We
can couple issue and env state machines." **Approved and shipped the same
day** (`taste_core::work`, environment id = issue id, the one panel, the
tool fold). It rewrites opinion 1b of ARCHITECTURE.md, so it was written
down before anything moved; this is the design as it stands after
shipping, with the places the shipped version departed from the first
draft marked *shipped as*. Quality pass 2026-09-06.

## Conclusion up front

Yes. The flank carried two lists that described the same work from two
sides — the Environments panel said *where* work was happening, the
Backlog said *what* the work was — and the code already tied them at the
seams (`issue_claim`, `working_on_text`, `chat_create { issue? }`, the
publish gate that checked the claimed issue's branch). Making the tie the
model removed a panel, a naming scheme, three tools' worth of overlap,
and the case that had no answer: an environment with no issue, which is
work nobody wrote down.

**One list: the backlog.** The user's own checkout is its first row,
pinned. Every other row is an issue; an issue that has been *started* has
an environment, and the row shows it. Selecting a row that has an
environment aims the panes at it. That is the whole panel.

## The row is where the environment is seen

This is the part of the merge that is easy to lose and must not be: the
Environments panel went away, **its indicators did not**. A started
issue's row carries everything the environment's row used to, because the
backlog is now the only enumeration of environments in the window and a
monitor that does not show state is a list of names.

The row is two lines — the title, and under it what the work is doing —
with the environment's marks on the second line and the sparkline at the
end spanning both:

| Mark | What it says | Where it comes from |
| --- | --- | --- |
| **Traffic light** (`.env-dot`) | what the container is doing: green up, amber waiting or building, red failed, grey off | `SupervisorState` + the chat's binding |
| **Sparkline** | the last five minutes of activity: tokens, tool calls, shell output | `taste_core::activity`, one bucket a second, redrawn on the panel's tick |
| **Amber attention mark** | its chat stopped on the user — a permission, a sign-in, a drifted config | `ChatBinding::attention` |
| **Accent rail + review glyph** | flagged for review: the branch is what the user judges | `ReviewState::FlaggedForReview` |
| **Unpublished mark** | work here that no other checkout has | the per-environment git pass |
| **Lock** | the one being watched: its checkout is read-only in the panes | the aim, when it is not home |
| **State line** | the state in words (working, waiting, failed, stopped, review), the bound chat's name and role glyph | `taste_core::work::WorkState` |

A queued issue's row has none of these — a state glyph for queued,
completed or declined, the queue's word and an age — which is how the two
kinds of row read apart without a section header between them. The marks
are a commitment, not decoration: any redesign of the flank that drops
the light or the sparkline from a started row has reopened the question
this spike closed, and needs a spike of its own.

The row model is `fleet::FleetRow`, pure data assembled from the six
places the facts live (registry, workspace state, chats, git, podman,
proxy) and unit-tested as such. The backlog, gadget mode and the varlink
read model render the same rows; nothing grows an inventory of its own.
Rows sort by kind — the ones with an environment first, then the queue in
the user's order, then the resolved — and active rows are therefore at
the top, which is where a back-to-top button returns a list scrolled past
a page.

## What merged

| Before | After |
| --- | --- |
| `EnvironmentId` — a generated name (`calm-1`), never chosen | the issue's id (`i-0007`): the environment *is* that issue's |
| branch of record `agents/<env>` | `agents/i-0007` — same convention, derived from the issue |
| New Environment (panel `+`), `chat_create { task }` | New issue (the composer, pill **Create**), then **Start** in the backlog's header; `issue_start { issue }` for the orchestrator |
| `issue_claim { issue }` | gone — starting an issue is claiming it, and the claim is the environment |
| `env_list`, `env_status`, `issue_list`, `chat_status` | `issue_list` and `issue_status` carry the runtime half for rows that have an environment; `env_*` gone |
| `ReviewState` on the environment × `IssueState` on the issue | one derived `WorkState` (below) |
| Environments panel + Backlog panel | the Backlog |

The console's environment tab, the review band, the chat pane and the
publish gate are unchanged in what they do and renamed in what they are
about — "the selected issue's environment". None changed shape.

## The coupled state machine

The issue's `resolution` in the ref stays the durable record; the runtime
states come from the environment that manifests it. One `work_state()`
derives the row's word from both, in the order a reader needs:

```
Queued ─start─▶ Starting ─▶ Working ◀─▶ Waiting     (on the user: permission,
                   │            │                     sign-in, drift)
                   │            ├──▶ Failed          (build or start broke; red)
                   │            └──▶ Stopped         (container off, or started
                   │                                  on another machine)
                   │
                publish(ready)
                   ▼
                Review ──merge──▶ Completed          (branch merged; the environment
                   │                                  can be destroyed with nothing lost)
                   └──reject────▶ Queued again, with a comment — NOT Declined
                                                     (the user refused this attempt,
                                                      not the issue)

Queued ─decline─▶ Declined                          (there will be no work)
```

*Shipped as* `taste_core::work::work_state(outcome, started, runtime,
review)`, not in `taste_git`: the issue store knows the durable half and
the supervisor and review board know the runtime half, and neither crate
sees the other, so the derivation lives in the crate both can reach.
`Starting` and `Stopped` are states the first draft folded into their
neighbours and the light could not: a container on its way is amber, and
a started issue whose container is off — paused, finished but not flagged,
or started on another machine — is grey and not free to take.

- **Reject returns the issue to Queued**, with the rejection as a comment
  and the branch kept until deleted. This is the one place the merge
  changes a meaning: before it, `Rejected` was a terminal review state and
  `Declined` a terminal issue state, and they were different things —
  rejecting an attempt is not deciding against the work. Reject is a
  transition back; decline stays the only way an issue ends without a
  merge.
- **Merged is Completed** whatever the store has caught up to, so the row
  cannot say "review" over a branch that has already landed.

The panel, the console's state line, `issue_list` and the publish gate
all read that one function; before the merge they read three.

## Cardinality: one issue, one environment

An environment is one issue's, by construction. Follow-up work found
while working an issue is a new issue — filed by the agent with
`issue_create`, and either started as its own environment or left queued
for the user — which is what "issues are how work outlives a
conversation" already asks for. An agent that wants to do two things in
one container writes one issue that says both. This is a real constraint,
and the right one: it is what makes the row's state readable.

## The primary row

"Yours" is the user's own checkout: it has no issue, is never in review,
and is the row the panes aim at by default. It stays pinned at the top of
the list with its light and sparkline, above the queue's user-ordered
rows, and is not draggable among them. The panel is still "the single
namer of the selected environment"; the environment just has an issue
title under it now instead of a generated word.

## Selection, and what a row's activation does

Two gestures the backlog keeps apart:

- **Selecting** a row that has an environment aims the panes at it.
  *Shipped as* the list's own selection, with rows that have no
  environment made **unselectable**, so the selection can never land on a
  queued row and the aim needs no second mark of its own. The panel tints
  itself when the aim is not home.
- **Activating** a row (double-click, Enter) opens its issue for editing —
  the same composer, in a popover on the row, with Save as its pill.
  Start, Stop and Delete act on the selected row from the backlog's
  header; *shipped as* header actions rather than the composer's primary
  action, because the composer is for a *new* issue only and a
  half-written one may be sitting in it.

## What is lost, and what stands in for it

- **Environments as scratch worlds.** "New Environment" with no purpose
  written down goes away. An exploration is an issue too ("Try X"), and
  writing it down is the whole convention: the docs already say an
  environment that never published is not evidence of anything.
- **Generated names.** `calm-1` was pleasant; `i-0007` is a reference the
  user already types into chat, and the row shows the title, not the id.
- **Many claims per environment.** Above.

## Cross-machine, since the ref travels and containers do not

`refs/taste/issues` is pushed and fetched; environments are local
containers. Two machines can each try to start `i-0007`. The ref records
`started_by` (the identity `taste_git::host_identity` already computes
for commits), and a second start sees it and is **refused**, naming who
landed first — the compare-and-swap loop that used to settle claims by
environment name settles them by this instead. A started issue with no
environment on this machine renders `Stopped`, not `Queued`: it is not
free to take. The override ("start it here anyway, and let `publish`
report the divergence") can wait for someone to need it.

## Orchestration, simplified

`chat_create { task, agent?, model?, issue? }` became
`issue_start { issue, agent?, model? }` — the sub-agent's first prompt is
the issue text, which is what the `task` was — and an orchestrator that
wants new work writes it down first (`issue_create`) exactly as the user
does. `chat_send` and `chat_status` keep their names but address issues.
`env_list` / `env_status` folded into `issue_list` / `issue_status`. The
read/write split of 2026-09-05 holds: listing and status on every socket,
start and send on the orchestrator's.

## What shipped, in order

1. The derivation: one `work_state()` from resolution + environment, in
   `taste_core::work`, with the reject-returns-to-queued rule, tested
   against every pair.
2. Environment ids became issue ids: the registry keyed by issue,
   `env_branch` unchanged in shape. Alpha rule — state version bumped,
   existing environments reset with the one-time notice; the probe
   fixtures gained issues for their environments.
3. The backlog row grew the environment's marks (light, sparkline,
   attention, review, unpublished, lock) for started issues, and
   selection aims the panes. The Environments panel was deleted; gadget
   mode shows the one panel.
4. Start/Stop/Delete in the backlog's header; `issue_start`;
   `chat_create`, `issue_claim` and `env_*` removed; docs (opinion 1b
   rewritten, the ENVIRONMENTS panel section, the orchestration tool
   list).
5. `started_by` in the ref and the cross-machine refusal.

## Decisions taken

1. **Reject returns the issue to Queued** rather than declining it. The
   alternative loses the distinction between "not this attempt" and "not
   this work".
2. **One issue per environment**, follow-ups as new issues.
3. **Environment id = issue id**, and `started_by` for the cross-machine
   case, refusing a second start by default.
4. **Scratch work is an issue** — no environment without one.
5. **A started row keeps the environment's indicators** — light,
   sparkline, attention, review — as a commitment of the design, not a
   detail of the first implementation.
