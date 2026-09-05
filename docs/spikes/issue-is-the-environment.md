# Spike: an environment is an issue in progress

David, 2026-09-05: "Except the primary one for me, the env is just an
active manifestation of a backlog item. We get to lose a whole panel. We
can couple issue and env state machines." **Proposal, not yet approved.**
It rewrites opinion 1b of ARCHITECTURE.md, so it is written down before
anything moves.

## Conclusion up front

Yes. Today the flank carries two lists that describe the same work from
two sides — the Environments panel says *where* work is happening, the
Backlog says *what* the work is — and the code already ties them at the
seams (`issue_claim`, `working_on_text`, `chat_create { issue? }`, the
publish gate that checks the claimed issue's branch). Making the tie the
model removes a panel, a naming scheme, three tools' worth of overlap,
and the case that has no answer today: an environment with no issue,
which is work nobody wrote down.

**One list: the backlog.** The user's own checkout is its first row,
pinned. Every other row is an issue; an issue that has been *started* has
an environment, and the row shows it — light, sparkline, attention mark,
review mark — exactly as the Environments row does now. Selecting a row
that has an environment aims the panes at it. That is the whole panel.

## What merges

| Today | After |
| --- | --- |
| `EnvironmentId` — a generated name (`calm-1`), never chosen | the issue's id (`i-0007`): the environment *is* that issue's |
| branch of record `agents/<env>` | `agents/i-0007` — same convention, derived from the issue |
| New Environment (panel `+`), `chat_create { task }` | New issue (the composer), with **Start** as its primary action; `issue_start { issue }` for the orchestrator, and `issue_create` then `issue_start` where it used to say `chat_create { task }` |
| `issue_claim { issue }` | gone — starting an issue is claiming it, and the claim is the environment |
| `env_list`, `env_status`, `issue_list`, `chat_status` | `issue_list` carries the runtime half (state, light, chat status) for rows that have an environment; `env_*` go |
| `ReviewState` on the environment × `IssueState` on the issue | one state on the issue (below) |
| Environments panel + Backlog panel | the Backlog |

The console's environment tab, the review band, the chat pane, the
publish gate: unchanged in what they do, renamed in what they are about
— "the selected issue's environment" — and none of them has to change
its shape.

## The coupled state machine

The issue's `resolution` in the ref stays the durable record; the runtime
states come from the environment that manifests it. One `state()` is
derived, in the order a reader needs:

```
Queued ─start─▶ Working ──▶ Waiting ──▶ Working …
                  │  ▲          (on the user: permission, sign-in, drift)
                  │  └──── Failed (build or start broke; red)
                  │
                publish(ready)
                  ▼
               Review  ──merge──▶ Completed   (env destroyable, branch merged)
                  │
                  └──reject────▶ Queued again, with a comment — NOT Declined
                                  (the user refused this attempt, not the issue)

Queued ─decline─▶ Declined      (there will be no work; nothing to verify)
```

- **Queued** — written down, no environment. What the backlog's unstarted
  rows are today.
- **Working / Waiting / Failed** — the environment's supervisor state and
  the chat's, as the light says them now: green, amber, red.
- **Review** — `ReviewState::FlaggedForReview`: the environment is
  stopped (grey), the branch is what the user reviews.
- **Completed** — merged; the issue's gate (`Mergedness`) is the same
  check it is today, and the environment can be destroyed with nothing
  lost.
- **Reject returns the issue to Queued**, with the rejection as a comment
  and the branch kept until deleted. This is the one place the merge
  changes a meaning: today `Rejected` is a terminal review state and
  `Declined` is a terminal issue state, and they are different things —
  rejecting an attempt is not deciding against the work. The coupled
  machine has to keep that distinction, so reject is a transition back,
  and decline stays the only way an issue ends without a merge.

One derivation, in `taste_git::issues` next to `IssueState::of`, from the
resolution, the presence of an environment, and its supervisor/review
state. The panel, the console's state line, `issue_list` and the publish
gate all read that one function; today they read three.

## Cardinality: one issue, one environment

Today an environment can hold several claims (`claims_for` returns a
list). Under the merge an environment is one issue's, by construction.
Follow-up work found while working an issue is a new issue — filed by the
agent with `issue_create`, and either started as its own environment or
left queued for the user — which is what "issues are how work outlives a
conversation" already asks for. An agent that wants to do two things in
one container writes one issue that says both. This is a real
constraint, and the right one: it is what makes the row's state readable.

## The primary row

"Yours" is the user's own checkout: it has no issue, is never in review,
and is the row the panes aim at by default. It stays pinned at the top of
the list with its light and sparkline, above the queue's user-ordered
rows, and is not draggable among them. The panel is still "the single
namer of the selected environment"; the environment just has an issue
title under it now instead of a generated word.

## Selection, and what a row's activation does

Two gestures the backlog already separates:

- **Selecting** a row that has an environment aims the panes at it (the
  panel's job today). Selecting a queued row aims nothing — the panes
  stay where they are — and the aim is drawn as the current-row mark, not
  as list selection, so the two cannot be confused.
- **Activating** a queued row opens it in the composer (edit), as now;
  activating a row with an environment does the same for its issue text.
  Start is the composer's primary action on a queued issue.

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
containers. Two machines can each start `i-0007`. Today that is a claim
race the ref's compare-and-swap loop settles by environment *name*, and
names are per machine. With the environment named by the issue, the
claim needs the other half of the name: the ref records
`started_by: <user@host>` (the identity `taste_git::host_identity`
already computes for commits), and a second machine starting the same
issue sees it started elsewhere and is refused — or told, and allowed,
with the branch of record then being `agents/i-0007` from two places,
which is the divergence `publish` already reports. Refused is the right
default; the override can wait for someone to need it.

## Orchestration, simplified

`chat_create { task, agent?, model?, issue? }` becomes
`issue_start { issue, agent?, model? }` — the sub-agent's first prompt is
the issue text, which is what the `task` was — and an orchestrator that
wants new work writes it down first (`issue_create`) exactly as the user
does. `chat_send` and `chat_status` keep their names but address issues.
`env_list` / `env_status` fold into `issue_list` / `issue_status`. The
read/write split of 2026-09-05 holds: listing and status on every
socket, start and send on the orchestrator's.

## Order of work

1. The derivation: one `state()` from resolution + environment, in
   `taste_git`, with the reject-returns-to-queued rule, tested against
   every pair. No UI change.
2. Environment ids become issue ids: the registry keyed by issue,
   `env_branch` unchanged in shape. Alpha rule — state version bumps,
   existing environments are reset with the one-time notice; the
   fixtures for the probe frames gain issues for their environments.
3. The backlog row grows the environment's marks (light, sparkline,
   attention, review) for started issues, and selection aims the panes.
   The Environments panel is deleted; gadget mode shows the one panel.
4. The composer's Start action; `issue_start`; `chat_create` and
   `issue_claim` and `env_*` removed; docs (opinion 1b rewritten, the
   ENVIRONMENTS panel section, the orchestration tool list).
5. `started_by` in the ref and the cross-machine refusal.

## Decisions this needs

1. **Reject returns the issue to Queued** rather than declining it. (My
   recommendation; the alternative loses the distinction between "not
   this attempt" and "not this work".)
2. **One issue per environment**, follow-ups as new issues.
3. **Environment id = issue id**, and `started_by` for the cross-machine
   case, refusing a second start by default.
4. **Scratch work is an issue** — no environment without one.
