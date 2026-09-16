---
title: The auth terminal signs in to a volume the relocated agent never reads when the substrate is a podman machine
state: open
reporter: i-0037
created: 2026-09-16T03:31:03Z
updated: 2026-09-16T03:31:03Z
labels: environments, agents, bug, substrate
---

**Found during i-0037**, which inherits the mismatch exactly rather than fixing or worsening it.

**The two halves.** The outside-confined agent container and the auth terminal are composed by `taste_acp::sandbox::container_agent_command`, which is pinned to the **local** podman by an explicit comment (`crates/taste-acp/src/sandbox.rs:407`): that rung is built out of host sockets — the IDE's MCP socket, the URL bridge, `--network=host` for an OAuth callback — and "a unix socket bind-mounted through virtiofs is not connectable from inside a VM, and the host's loopback is not the VM's". Correct, and the reason is sound.

An environment's own container, meanwhile, lives on whatever `taste_devcontainer::substrate` resolved: local, a `podman machine`, or a remote connection.

**A named volume does not cross that boundary.** `taste-env-<key>-<env>-home` on the host and `taste-env-<key>-<env>-home` in the VM are the same *name* and two different *volumes*. So when the substrate is a machine and a chat's agent is relocated:

- the agent reads its home from the VM's volume, mounted by `Supervisor::ide_mounts`;
- the auth terminal writes the sign-in into the host's volume, because it is outside-confined always (`chat.rs::open_sign_in_terminal` → `taste_acp::login_command`);
- the sign-in never reaches the agent, and the chat's own message says the opposite: "the login must run in the SAME confinement as the agent (same home, same container) or the credentials land where the agent never looks".

This is exactly the bug that comment was written about, one substrate down. It predates i-0037: the same is true of the conversation home, so a relocated agent on a machine substrate also cannot see anything an outside-confined spawn of the same environment wrote, which is the *other* half of what "the home is the same volume in both topologies" promises.

**Where the premise is written down as true.** `docs/spikes/secret-service.md` § Phase 3 ("it writes to the agent's home and the home is the same volume in both topologies") — now annotated with this issue; `crates/taste-app/src/chat.rs::open_sign_in_terminal`; `crates/taste-acp/src/aim.rs` module docs. CLAUDE.md's rule ("the cwd and the home volume are identical in both topologies") is the commitment this breaks.

**Not yet measured.** The reasoning is from the code and the substrate ladder; it has not been reproduced live, because this machine's environments are on local podman (`TASTE_PODMAN_CONNECTION` unset, no machine). Reproducing it is step one: register a machine, sign an agent in from an environment on it, and see where the file lands.

**Shapes an answer could take**, none chosen:

- **Run the login on the environment's substrate.** The device-code flow is what every `LoginHint` already mandates, precisely because a loopback callback cannot work from a container with its own network — so the host-loopback reason for pinning the login to local podman may not apply to the login at all. What it would lose is the `BROWSER` URL bridge, which is a host path bind; the device-code flow prints a URL the user opens themselves, so the loss is real but small.
- **Refuse relocation on a non-local substrate until the volumes agree**, which is honest and costs the whole point of the machine tier.
- **Make the identity and home volumes substrate-aware**, i.e. accept that an environment on a machine has its own and say so.

**Gate.** A test that the login command and the relocated agent name the same podman service, in the style of `both_container_topologies_mount_the_same_agent_home`; whatever is chosen written into ENVIRONMENTS.md → Relocation and the spike's Phase 3, replacing the premise rather than footnoting it.
