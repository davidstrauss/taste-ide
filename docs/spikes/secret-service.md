# Spike: a secret service for the agents

The plan behind David, 2026-09-06: "I want to provide a secret service,
ideally leveraging desktop APIs via the IDE, so it *can* securely store
the credentials." Written after the Copilot auth terminal
(`ea6345e`) got as far as its last question and asked it of the user:

> Storing the token in the config file saves it as plain text, which is
> insecure. If you decline, sign-in will be cancelled and no account
> state will be changed.
>
> Store token in plain text config file?
> **1. Yes, store in plain text (insecure)** / 2. No, cancel sign-in

Inside the agent's confinement that is not a choice: "No" makes sign-in
impossible, so the IDE's own sign-in flow ends in the user consenting to
a plain-text GitHub token. This document is the plan, not a measurement:
**Phase 0 is the spike, and it gates the rest.** Nothing here has been
built.

**Conclusion up front.** The CLI is right and the environment is wrong —
both agent topologies deliberately have no session bus, so `libsecret`
finds nothing and every keyring-capable CLI degrades to its file
fallback. The fix is not to open that hole but to give the agent **a bus
of its own carrying exactly one service, implemented by the IDE**, whose
backing store is the desktop's secret store on the host side of the
boundary. It rides the environment channel as a third service code, so
it needs no new mount and does not touch the devcontainer contract.

## Why the CLI asks

| Topology | What the agent's D-Bus is | Result |
| --- | --- | --- |
| Outside-confined (bwrap) | a tmpfs over `$XDG_RUNTIME_DIR` and `/tmp`, **hiding the session D-Bus** (`taste-acp/src/sandbox.rs`, module docs); `DBUS_SESSION_BUS_ADDRESS` is inherited (bwrap gets no `--clearenv`) and points at a masked path | `connect(2)` fails, `libsecret` reports no service |
| Relocated (container) | nothing — no bus in the image, and none should be | same |

Both are correct as they stand. Handing an agent the user's session bus
hands it the login keyring: ssh passphrases, browser passwords, every
secret the desktop holds. The masking is the boundary CLAUDE.md defends
("the boundary is the host, not the agent"; agents launch with no
runtime-dir sockets), and this plan does not weaken it anywhere.

What the agent gets instead is a socket the IDE serves, on which the
only reachable name is `org.freedesktop.secrets`, and behind which the
only visible items are the ones that agent stored.

## Where it plugs in

The hard part is already built. `taste-devcontainer/src/channel.rs`
exists because a `container_t` process may not dial a socket the
unconfined IDE bound: the IDE `podman exec`s one helper per environment,
the helper binds `mcp.sock` and `auth.sock` **inside** the container, and
every connection it accepts is multiplexed over its own stdio back to the
IDE, where the demux attaches the environment. `Service` is a closed set
of two codes.

A secret service is a **third code on that same channel**. No new mount,
no change to the config hash, no rendezvous protocol, and identity stays
where ENVIRONMENTS.md puts it: the channel is the identity.

## Phase 0 — the spike, and it is a gate

Everything below assumes the CLIs actually speak Secret Service. Verify
first, and record the traces here:

1. **Do they dial it at all?** Point `DBUS_SESSION_BUS_ADDRESS` at a
   socket that only logs, and run `@github/copilot@1.0.82` and
   `@google/gemini-cli@0.58.0` under `strace -f -e
   trace=connect,openat` in the devcontainer. A CLI that never connects
   cannot be helped by any of this.
2. **Which binding?** `@napi-rs/keyring` implements the Secret Service
   protocol in Rust and links statically — nothing new in the image.
   `keytar` and the GObject bindings `dlopen`
   `libsecret-1.so.0`, which would raise the devcontainer contract past
   "it carries `node`" (ENVIRONMENTS.md → conventions a devcontainer
   must meet). That cost must be paid deliberately, not discovered.
3. **The cheap branch.** If `copilot` honours a documented
   `GH_TOKEN`/`GITHUB_TOKEN` (intended interfaces only: documented env
   vars, public CLI), an IDE-held token injected at spawn solves Copilot
   with no bus at all. Gemini's OAuth credentials file would still want
   Phases 1–3. Decide per agent, in writing.

Outcome: "build the service" or "inject a token", with the traces that
say which.

## Phase 1 — `crates/taste-secrets` (no GTK, tokio only)

An `org.freedesktop.secrets` implementation the IDE hosts, served per
connection like the other two services.

- **A minimal bus, not a peer-to-peer connection.** GDBus and zbus
  clients do SASL `EXTERNAL`, send `Hello`, and then address
  `org.freedesktop.secrets` *by name* — a p2p connection is not enough.
  The crate answers `org.freedesktop.DBus` itself (`Hello`,
  `GetNameOwner`, `NameHasOwner`, `AddMatch`, `Peer.Ping`) and routes
  everything else to its one object tree. This is the real
  implementation cost, and what it buys is an image contract that stays
  at "node".
- **The object model.** `Service`: `OpenSession` for both `plain` and
  `dh-ietf1024` (libsecret negotiates the encrypted session by default,
  so DH plus AES-128-CBC is not optional), `SearchItems`, `GetSecrets`,
  `ReadAlias("default")`, and `Unlock`/`Lock` as no-ops. One
  `Collection`. `Item` with `GetSecret`, `SetSecret`, `Delete`,
  attributes, label, `Locked=false`. No `Prompt` path exists, because
  the collection the agent sees is never locked.
- **`trait SecretStore` behind it**, with three implementations: host
  `libsecret` — the desktop store, the point of the exercise —, a
  Flatpak `org.freedesktop.portal.Secret`-keyed encrypted file for
  sandboxed installs that cannot talk to the name, and in-memory for
  tests.
- **Namespacing is the security boundary.** Every item is stored
  host-side under attributes scoped to `(workspace, agent)`;
  `SearchItems` can only match within that namespace and the collection
  listing shows nothing else, so the agent cannot name, enumerate or
  unlock anything the user owns. A locked host keyring raises the
  desktop's own unlock dialog — off the main thread, with a bounded
  timeout, since the agent's call is blocked on it.
- **Reachability inside the environment is unchanged from the other two
  sockets.** Anything in the container can dial it, `ide_exec` shells
  included. That is the standing position, not a regression: the agent
  writes the code the container runs, so agent and repo code are one
  principal.

## Phase 2 — delivery on the existing channel

- `Service::Secrets` = code 3 in `channel.rs`; the helper binds
  `secrets.sock`; `container_secrets_socket()` lands beside
  `container_auth_socket` in `taste-core/src/environment.rs`.
- Honour the probe convention — each service is made to **reply as
  itself**: a `Peer.Ping` that comes back. And the opt-out convention:
  `TASTE_SECRETS=0` means the door is never opened, so it is never
  probed and never fails an environment for a door nobody asked for.
- Outside-confined, the IDE binds the socket in its **cache** directory,
  not `$XDG_RUNTIME_DIR` (masked in the sandbox — the reason already
  written down for the MCP socket), and `--bind`s it in.

## Phase 3 — both spawn sites, or it silently does nothing

`DBUS_SESSION_BUS_ADDRESS=unix:path=…` has to be set for the agent
**and** for the auth terminal, and the asymmetry between them is sharp:
the login terminal is outside-confined *always*, even for a chat whose
agent is relocated, because it writes to the agent's home and the home
is the same volume in both topologies. **The socket is not the same.**
The two paths resolve to different sockets over one store, or the token
lands where the agent never looks — the failure CLAUDE.md already warns
about for cwd and home. A test asserts the two sites agree, in the style
of `a_login_hint_pins_the_adapters_own_version`.

## Phase 4 — scope: per agent, workspace-wide

The agent home is a volume **per environment** (`env_home_volume`), so
today a Copilot sign-in has to be repeated for every environment. Key
the store by `(workspace, agent)` instead — like the auth proxy, which
is workspace-wide — and one sign-in serves the fleet. This is a
deliberate widening: the credential is the user's GitHub identity, not
an environment's. It belongs in ENVIRONMENTS.md as such, next to the
existing note that worker agents from other providers "keep their own
credentials".

## Phase 5 — Flatpak and the desktop

- `--talk-name=org.freedesktop.secrets` in
  `build-aux/flatpak/net.davidstrauss.Taste.json`, so items land in the
  real login keyring and are visible in Seahorse.
- Where that name is unavailable, the Secret portal store: the portal
  hands the app a master secret, the IDE encrypts its own file with it.
  Sandboxed, no extra permission, invisible to Seahorse.
- Where the desktop has no secret service at all (headless CI, a host
  with no keyring daemon), the IDE **says so in the chat** rather than
  letting the user meet the plain-text question with no explanation.

## Phase 6 — what the user sees

The question stops appearing; that is the whole visible change to the
sign-in flow. Two additions follow from the IDE now holding a
credential: a meta row when one lands ("GitHub Copilot's sign-in is in
your keyring"), and a reachable **Sign out** that deletes the item.
Holding a secret with no way to forget it is not acceptable.

## Phase 7 — tests, docs, and looking at it

- Unit: a zbus client against an in-process socket; an isolation test
  proving agent A cannot see agent B's item; an `#[ignore]` integration
  test against a real keyring.
- Regression: the session bus stays masked, and no host bus socket is
  ever bound at any spawn site.
- Docs: a section beside the channel one in ENVIRONMENTS.md (three
  service codes now), a trust-model line in ARCHITECTURE.md, the crate
  in CLAUDE.md's layout — and an explicit note on **Varlink, not
  D-Bus**: consuming the desktop's secrets API is squarely allowed, and
  implementing `org.freedesktop.secrets` is speaking a *platform* API to
  third-party CLIs, not defining an interface. Every interface taste-ide
  invents still goes varlink over a unix socket.
- Live: run `/login` in the auth terminal, confirm the question is gone,
  `secret-tool search` on the host shows the item under our attributes,
  `~/.copilot` holds no token, and a second environment is already
  signed in.

## Decisions this plan does not make

1. **Whether Phase 0's env-var branch is enough for Copilot.** It
   removes most of the work, for one agent.
2. **The widened credential scope** (Phase 4): one sign-in across the
   fleet, against per-environment homes everywhere else.
3. **The price of the minimal bus and the DH session** versus the
   alternative this plan rejects: shipping `dbus-daemon` and
   `gnome-keyring-daemon` in the image, which raises the devcontainer
   contract for every repo and puts the secret back inside the
   container, encrypted by a key the IDE would have to hand it anyway.
4. **CLIs that ignore Secret Service entirely.** For those the plan
   degrades to the IDE explaining, in the chat, why their own CLI is
   asking — which is worth doing regardless.
