# Spike: an agent host, and environments reached from more than one surface

A proposal, written 2026-10-29. No product code changed and nothing was
measured; every fact about the code below names the file it was read
from, and every fact about the Agent Host Protocol names its source.

**Conclusion up front: the obstacle to reaching an environment from more
than one surface is not where the VMs are — cloud hosts already exist —
but that the GTK process owns every live session. Split that ownership
into a per-workspace agent host, a process with no GTK that the desktop
app is the first client of, run locally first and in a cloud VM after;
shape its client protocol on the Agent Host Protocol's model (a snapshot
and ordered actions over channels) without depending on AHP itself while
it is a draft; and make the second surface a supervision client for the
coordinator's chat, the permission prompts, and the fleet, not a second
IDE.** Five standing commitments change with it, and are listed under
"What this changes" for a decision each, before any of it is built.

## What was asked for

David, 2026-10-29, after asking whether AHP has a role here (it has none
today): "I've been interested in having cloud VMs with envs I can connect
to from different surfaces." And, on the outline of this proposal: "Yes,
add it to spikes."

What "surfaces" means is open (see "Questions"): another desktop running
this IDE, a browser, or a phone. The design below does not depend on the
answer until its last step.

## Where things stand

Agent environments already run on cloud hosts: pool VMs made in the
project's GCP, reached through IAP, with the cloud Start and
`environment_move` (ENVIRONMENTS → "Cloud hosts, as built"). Personal
stays local; moving it is ROADMAP item 7, "Personal in the cloud".

None of that lets a second surface attach, because what makes a session
live is in the IDE's process on one machine:

| What | Where it lives now | Read from |
|---|---|---|
| The ACP client, one per chat | the chat pane spawns it | `crates/taste-app/src/chat.rs` (`AgentClient::spawn_aimed`) |
| The agent process | a `podman exec` child of the IDE, its stdio the ACP connection | `crates/taste-acp/src/relocate.rs` (`exec_in_environment`) |
| The MCP server (`ide_*` tools) | built by the window | `crates/taste-app/src/window.rs` (`McpServer::new`) |
| The auth proxy and the credential | started at launch | `crates/taste-app/src/main.rs` (`taste_acp::authproxy::start`) |
| Transcripts | this machine's state directory, on purpose | `crates/taste-core/src/chatarchive.rs` |
| The coordinator's reach into other chats | a request seam to the GTK thread | `crates/taste-core/src/orchestration.rs` |
| Unsaved buffers, IDE-applied edits, confirmations, screenshots | a request seam to the GTK thread | `crates/taste-core/src/ui_probe.rs` (`UiRequest`) |
| Environments, VMs, and moves | `EnvironmentRegistry`, already GTK-free | `crates/taste-devcontainer` |
| The project's "don't ask again" answers | one shared handle | `crates/taste-core/src/standing.rs` |

Close the IDE and the agents end with their exec sessions, and a guest
powers itself off five minutes after the last ssh session
(`crates/taste-devcontainer/src/provision.rs`, `taste-idle-off`). There
is nothing left running for another surface to attach to.

Two things in that table make the split smaller than it looks:

- **The seams already exist.** `ui_probe` and `orchestration` were made
  so tokio-side code could ask the GTK thread for things as plain data,
  "no widget, no `Rc`, no agent handle". They are the first draft of
  the host's client protocol: what crosses them today is what would
  cross the wire.
- **There is already an outside reader.** `taste-fleetlink` serves the
  fleet's read model over varlink (ARCHITECTURE → the crate table),
  read-only and with no GTK. It is the precedent for a client that is
  not the window.

## The Agent Host Protocol

Microsoft's protocol for VS Code's Agent Host: "a dedicated process that
runs agent harnesses and manages their sessions", locally, "inside a Dev
Container, or on a remote machine", where "multiple clients can observe
and control the same session, staying in sync" ([VS Code Agent Host
architecture](https://code.visualstudio.com/docs/agents/concepts/agent-host)).
It sits above ACP rather than replacing it: "AHP is a coordination layer.
ACP is a communication layer. They compose naturally", a host speaking
"AHP upstream (to clients) and ACP downstream (to agents)" ([AHP and
ACP](https://microsoft.github.io/agent-host-protocol/guide/ahp-and-acp.html)).

What the specification overview defines
([overview](https://microsoft.github.io/agent-host-protocol/specification/overview.html)):

- JSON-RPC 2.0 over any reliable, ordered, bidirectional stream, with a
  negotiated SemVer protocol version.
- Channels, each a URI every message names: `ahp-root://`,
  `ahp-session:/<uuid>`, `ahp-chat:/<cid>`, and terminal, changeset,
  resource-watch, annotation, and automation channels.
- `subscribe` answers with a snapshot (`state`, `fromSeq`); after it, the
  host sends `action` notifications carrying a `serverSeq`, and clients
  send `dispatchAction`. The host's state is authoritative and changes
  only through reducers.
- Root state holds agents, terminals, and the session catalogue; a
  session holds chats, its active clients, and changesets; a chat holds
  turns, streaming, tool calls, pending messages, and input requests.
- Extensions: `x-` names for channel schemes, methods, notifications, and
  action types, "by prior agreement between peers".
- Status: a draft, versions 1.0.0 and 1.1.0 (draft) published, breaking
  changes expected; client libraries in Rust, TypeScript, Kotlin, Go, and
  Swift.

How it maps here: an AHP chat is one of our conversations, nearly field
for field (turns, tool calls, input requests, the messages that wait
behind a turn). An AHP session is closest to an environment. A changeset
is the Dirty list and the review. What AHP has no shape for is the rest
of what a client here shows: the fleet's rows, the issue queue, ports,
tasks, moves, the startup page, and the coordinator's acts. Those would
be `x-` channels of our own.

The protocol is a documented public surface, so "intended interfaces
only" does not stand in its way. Its draft status does: depending on it
now means following its breaking changes. Shaping our own protocol on its
model (channels, a snapshot, sequenced actions, reducers) costs nothing
extra and keeps adopting the real thing a translation rather than a
redesign.

## The proposed shape

A per-workspace **agent host**: a process with no GTK, holding everything
in the table above but the widgets.

- **It owns:** each conversation's ACP client and its agent's exec
  session; the MCP server; the auth proxy; the transcripts; the
  coordinator's orchestration; the registry and its supervisors; the
  standing answers; and the event bus that today is in-process.
- **A client owns:** what is drawn, the editor's buffers, the keyboard,
  and anything that needs the user's machine: the folder the mirror
  writes, and git's fetch and push with the user's keys.
- **Between them:** the protocol above. `UiRequest` becomes a request
  the host sends to a client, as AHP's reverse-direction requests are
  (`resourceRequest`), and `OrchestrationRequest` becomes host-internal,
  since the chats it reaches would live in the host.

## What this changes

Each of these is a standing commitment, written down elsewhere, that the
split reverses or strains. None should be decided by implementation.

1. **Continuity.** CLAUDE.md: continuity "comes from the persisted
   session id and `session/load`, never from the process outliving
   anything". A host is a process outliving its clients by design. The
   rule still holds one level down, since an agent in a container still
   dies with a reload and comes back through `session/load`.
2. **Mediation with more than one client.** `fs/read_text_file` reads
   "the user's" unsaved buffer, and an IDE-applied edit lands in "their"
   undo history. With two clients open on one file, both words need an
   owner. The likeliest answer: the client that most recently focused the
   file holds it, and the others read the disk.
3. **Where the credential is.** The auth proxy holds the project's
   credential so agents never do. In a cloud VM, a host run as the
   guest's user sits beside rootless containers run by that same user,
   so the credential would be one container escape away. On the desktop,
   that escape reaches a VM, never the credential. Isolation standard
   (ENVIRONMENTS → "Isolation: the standard, and what meets it") says
   what the line is for; the likely way to keep it is a host in a VM, or
   a container, of its own, holding no agent's containers.
4. **Where transcripts are.** `chatarchive.rs` keeps conversations "on
   the user's own machine, and nowhere else" (David, 2026-09-17), because
   a transcript is where a pasted secret sits. A cloud host holding them
   moves them into the project's cloud. They would still never be refs,
   so never pushed; but "on my machine" would no longer be true.
5. **When a cloud host stops.** Hosts stop ten minutes after holding no
   running container, and a guest five minutes after its last ssh
   session. Agents that work with nobody connected need "idle" to mean
   no client and no turn in flight, and a host billed for that time.

And two that are not reversals, but new obligations:

- **A permission prompt with nobody connected** waits until someone
  answers it, which wants a notification to some surface. The
  orchestrator still may not answer for the user (`orchestration.rs`:
  "no permission answer"); a second surface answering as the user is a
  different thing, and the reason that surface is worth building.
- **Push stays where the keys are.** The keys never enter a VM, so
  review, merge, and push remain a desktop client's acts, unless that
  is decided otherwise too.

## Staging

1. **The host, local.** Move the table's contents into a host process on
   this machine, with the GTK app as its only client and the protocol
   between them. Nothing about where things run changes, and it pays for
   itself: closing or restarting the window no longer ends a turn. Done
   when every probe and test passes against the split, and a window
   killed mid-turn reattaches to the turn still streaming.
2. **The protocol, written down.** Its channels, snapshot, and actions,
   with AHP's names where AHP has the concept and `x-` names where it
   does not. The checked-in IDL, as `taste-fleetlink` has one.
3. **The host in a cloud VM,** resolving "Where the credential is" and
   "When a cloud host stops" first. Personal in the cloud (ROADMAP item
   7) comes with it, since Personal's agent is then the host's too.
4. **The second surface.** Built for supervision: the coordinator's
   transcript and composer, the permission prompts, and the fleet's
   rows, which is where David works by preference (CLAUDE.md →
   "Supervision happens at the orchestrator level"). It needs none of the
   editor, which is most of the desktop app.

## Questions for David

- Which surfaces: another desktop running this IDE, a browser, or a
  phone? A desktop client needs all of step 1's protocol; a supervision
  surface needs a fraction of it.
- Should agents work with nobody connected, billed for that time, with
  their permission prompts sent to you?
- Transcripts in the project's cloud: acceptable, or do they stay on the
  machine you use, with a cloud host keeping only what a turn in flight
  needs?
- The credential: a host VM of its own per workspace, or another answer?

## What this spike did not do

It changed no code, ran no AHP implementation, and measured nothing: not
the cost of the host as a process, not the protocol's traffic for a busy
transcript, not the latency a remote client would see. It read the AHP
overview and guide pages, not the full reference or its JSON schemas,
which are the place to check every field name above before step 2.
