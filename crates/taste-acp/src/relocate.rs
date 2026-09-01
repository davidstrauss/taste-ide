//! Relocation: running the agent inside its environment's devcontainer.
//!
//! This is the topology ENVIRONMENTS.md calls "beside the files", and it is
//! *only* a topology. The address — working directory, MCP socket, bridge
//! spelling, mode — is [`crate::AgentAim`] and does not change when an
//! agent relocates; what changes is which of [`crate::sandbox`]'s
//! confinements the spawn picks. That split is deliberate: the agent's
//! conversation has to survive moving between the two, and it survives by
//! everything addressable staying identical across the move.
//!
//! Three things had to be true for the history to survive, and each is a
//! property of a *value*, not of a code path (ROADMAP → "What C requires"):
//!
//! 1. **The working directory is the same string on both sides.** The
//!    adapter keys history by cwd (`~/.claude/projects/<flattened-cwd>/`).
//!    The supervisor binds each environment's checkout into its container a
//!    second time at its REAL host path, so `/home/u/.local/state/…/repo`
//!    resolves in both topologies and the key does not move.
//! 2. **`HOME` is the same volume at the same path on both sides.** Both
//!    the outside-confined agent container and the relocated exec use
//!    `environment::env_home_volume` mounted at
//!    `policy::AGENT_HOME_IN_DEVCONTAINER`. The volume outlives container
//!    rebuilds, which is what makes a rebuild a respawn rather than an
//!    amnesia event.
//! 3. **No path translation anywhere.** Falling out of (1): the IDE and the
//!    agent speak the same absolute paths, so `fs/read_text_file`,
//!    `ide_exec` and the transcript all mean one thing.
//!
//! What relocation does change is reachability. The agent is now inside a
//! network namespace of the repo's choosing, where the auth proxy's
//! loopback port does not exist — and inside an SELinux domain that may not
//! dial anything the unconfined IDE bound, which rules out the host sockets
//! too. Both are answered the same way: the IDE's endpoints are served on
//! sockets the container's own helper binds, and the bytes ride that
//! helper's `podman exec` stdio (`taste_devcontainer::channel`). The two
//! in-container addresses that falls out of are [`Relocation::mcp_socket`]
//! and [`AuthForward::socket`].

use std::path::{Path, PathBuf};

use crate::AgentSpec;

/// What a relocated agent announces in `TASTE_IDE_CONFINEMENT`.
///
/// Distinct from `container`, which is the sibling agent container the
/// outside-confined topology uses. Anything in either container — an MCP
/// tool, a bare `env` in a shell — can tell which world it is in without
/// reverse-engineering it from `/proc`.
pub const CONFINEMENT: &str = "container-exec";

/// How a relocated agent reaches the IDE's auth proxy.
///
/// Inside the environment's container the proxy's `127.0.0.1:<port>` is a
/// different loopback than the IDE's, so `ANTHROPIC_BASE_URL` pointing at
/// it would dial nothing. What is reachable in there is the auth endpoint
/// of that environment's **channel** — a socket the container's own helper
/// bound, whose bytes ride `podman exec` stdio back to the IDE
/// (`taste_devcontainer::channel`). A tiny node forwarder turns it back
/// into an HTTP endpoint the adapter's documented `ANTHROPIC_BASE_URL`
/// mechanism can use.
///
/// `node` and not `socat`, for the same reason the MCP bridge is node: the
/// image belongs to the repo, and the one interpreter an ACP agent's
/// presence guarantees is the one its adapter is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthForward {
    /// The channel's auth endpoint, at its path **inside** the container.
    pub socket: PathBuf,
}

/// Everything relocation needs that the aim does not carry.
///
/// Note what changed when the socket direction inverted: the aim's
/// `mcp_socket` is a HOST path and stays correct for every outside-confined
/// topology, but a relocated agent cannot use it — a confined container may
/// not dial a socket the unconfined IDE bound. So the in-container
/// addresses live here, in the topology, which is exactly where a value
/// that differs between topologies belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocation {
    /// The environment's running container, by name.
    pub container: String,
    /// Which podman service that container is on — the local host, a
    /// machine, or a remote one.
    ///
    /// It rides on the relocation rather than being detected at the spawn
    /// because it is a fact about *this container*, and the spawn site has
    /// no way to know it. The container name alone is not an address: the
    /// same name means a different thing (or nothing) on a different
    /// service, and a relocation that named a container on the host while
    /// the environment lives in a VM would fail with podman's "no such
    /// container" at the one moment the user is waiting for an answer.
    pub podman: taste_core::PodmanTarget,
    /// The channel's MCP endpoint, at its path **inside** the container.
    /// The agent's stdio bridge dials this instead of the host socket.
    pub mcp_socket: PathBuf,
    /// Present when this spawn's credentials go through the proxy — which
    /// is to say, when `spec.env` carries an `ANTHROPIC_BASE_URL` that has
    /// to be re-pointed at something reachable from in there.
    pub auth: Option<AuthForward>,
}

/// The in-container auth forwarder, as a node program.
///
/// It listens on an **ephemeral** loopback port and hands the port to the
/// agent it starts, rather than agreeing a fixed one with the IDE. Two
/// failure modes disappear with that choice: a port already taken by
/// something the repo's own container runs, and the race between a
/// backgrounded forwarder and an agent that dials before it is listening.
/// The agent is spawned from inside `listen`'s callback, so by construction
/// there is nothing to race.
///
/// Being the agent's parent also gives it the right lifetime: the exec
/// session owns one process tree, the agent's stdio is inherited untouched
/// (ACP speaks over it), and the agent's exit code is the forwarder's.
const FORWARDER: &str = "\
const net=require('net'),cp=require('child_process');\
const sock=process.argv[1],argv=process.argv.slice(2);\
const srv=net.createServer(c=>{\
const u=net.connect(sock);\
c.pipe(u);u.pipe(c);\
const end=()=>{c.destroy();u.destroy()};\
c.on('error',end);u.on('error',end);c.on('close',end);u.on('close',end);\
});\
srv.on('error',e=>{console.error('taste-ide auth forwarder: '+e.message);process.exit(1)});\
srv.listen(0,'127.0.0.1',()=>{\
const env=Object.assign({},process.env,\
{ANTHROPIC_BASE_URL:'http://127.0.0.1:'+srv.address().port});\
const child=cp.spawn(argv[0],argv.slice(1),{stdio:'inherit',env});\
child.on('error',e=>{console.error('taste-ide: agent failed to start: '+e.message);\
process.exit(1)});\
child.on('exit',code=>process.exit(code==null?1:code));\
});";

/// The command the agent runs under, inside the container: itself, or the
/// forwarder wrapping itself.
fn inner_command(spec: &AgentSpec, auth: Option<&AuthForward>) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(auth) = auth {
        argv.push("node".into());
        argv.push("-e".into());
        argv.push(FORWARDER.into());
        argv.push(auth.socket.display().to_string());
    }
    argv.push(spec.command.clone());
    argv.extend(spec.args.iter().cloned());
    argv
}

/// Compose the relocated spawn: `podman exec` into the environment's
/// container, in the environment's checkout, at its host path.
///
/// The [`Relocation`]'s own [`taste_core::PodmanTarget`] decides which
/// podman this reaches — the host's, the machine's, or a remote one's — and
/// whether it goes through `flatpak-spawn --host` because podman lives
/// outside the IDE's sandbox. Both were separate booleans once; they are
/// one value now because getting either wrong produces the same failure and
/// neither is knowable at this call site.
pub fn relocated_agent_command(
    spec: &AgentSpec,
    cwd: &Path,
    relocation: &Relocation,
) -> (String, Vec<String>) {
    let mut args: Vec<String> = Vec::new();
    // No `-t`: ACP is a stdio protocol and a pty would corrupt it. `-i`
    // because the agent reads requests from stdin for its whole life.
    args.extend(["exec".into(), "-i".into()]);
    args.push("--workdir".into());
    args.push(cwd.display().to_string());

    let mut env: Vec<(String, String)> = Vec::new();
    // The agent's own home: the per-environment volume the supervisor
    // already mounts, and the SAME volume at the SAME path the
    // outside-confined agent container uses. That identity is what makes
    // relocation a respawn rather than a fresh start — see the module docs.
    env.push((
        "HOME".into(),
        taste_core::policy::AGENT_HOME_IN_DEVCONTAINER.into(),
    ));
    // The agent's own git, inside the container, still gets the policy:
    // push blocked, hooks masked. Additive `GIT_CONFIG_*` rather than
    // `GIT_CONFIG_GLOBAL`, so the identity the supervisor inherited into
    // this container at start survives to sign the agent's commits.
    env.extend(taste_core::policy::agent_git_config_env());
    // The environment announces itself, as it does in every confinement.
    env.push((
        "TASTE_IDE_VERSION".into(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    env.push(("TASTE_IDE_CONFINEMENT".into(), CONFINEMENT.into()));
    // The spec's own environment last, so the auth proxy's placeholder
    // rides in. `ANTHROPIC_BASE_URL` is deliberately NOT filtered out here
    // — the forwarder overwrites it with the port it actually listened on,
    // and a value that never reaches the agent cannot go stale.
    env.extend(spec.env.iter().cloned());
    for (key, value) in env {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }

    args.push(relocation.container.clone());
    args.extend(inner_command(spec, relocation.auth.as_ref()));
    relocation.podman.argv(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AgentSpec {
        let mut spec = AgentSpec::new("claude-code", "Claude", "npx", &["acp"], &[]);
        spec.env
            .push(("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:41234".into()));
        spec.env
            .push(("ANTHROPIC_AUTH_TOKEN".into(), "sk-ant-taste-abc".into()));
        spec
    }

    fn relocation_on(podman: taste_core::PodmanTarget) -> Relocation {
        let env = taste_core::environment::EnvironmentId::parse("review").unwrap();
        Relocation {
            container: "taste-abc123-review".into(),
            podman,
            mcp_socket: taste_core::environment::container_mcp_socket(&env),
            auth: Some(AuthForward {
                socket: taste_core::environment::container_auth_socket(&env),
            }),
        }
    }

    fn relocation() -> Relocation {
        relocation_on(taste_core::PodmanTarget::local(false))
    }

    fn args_for(sandboxed: bool) -> (String, Vec<String>) {
        relocated_agent_command(
            &spec(),
            Path::new("/home/u/.local/state/taste-ide/environments/abc123/review/repo"),
            &relocation_on(taste_core::PodmanTarget::local(sandboxed)),
        )
    }

    /// The agent follows its environment onto the substrate. A relocation
    /// that named the container but not the service would exec into
    /// whatever happened to answer locally — usually nothing, at the exact
    /// moment the user is waiting for a reply.
    #[test]
    fn a_relocated_agent_execs_into_the_substrate_its_container_is_on() {
        let (program, args) = relocated_agent_command(
            &spec(),
            Path::new("/work/p"),
            &relocation_on(taste_core::PodmanTarget::connection("taste-ide", false)),
        );
        assert_eq!(program, "podman");
        assert_eq!(&args[..4], ["-c", "taste-ide", "exec", "-i"]);
        assert!(args.contains(&"taste-abc123-review".to_string()));

        // ...and under Flatpak, both facts at once: the host's binary and
        // the machine's service.
        let (program, args) = relocated_agent_command(
            &spec(),
            Path::new("/work/p"),
            &relocation_on(taste_core::PodmanTarget::connection("taste-ide", true)),
        );
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(
            &args[..6],
            ["--host", "podman", "-c", "taste-ide", "exec", "-i"]
        );
    }

    /// The whole of relocation, as one argv: exec into THIS environment's
    /// container, in THIS environment's checkout at its host path.
    #[test]
    fn the_relocated_spawn_execs_into_the_environments_container() {
        let (program, args) = args_for(false);
        assert_eq!(program, "podman");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"-i".to_string()));
        assert!(!args.contains(&"-t".to_string()), "a pty would corrupt ACP");

        let workdir = args.iter().position(|a| a == "--workdir").unwrap();
        assert_eq!(
            args[workdir + 1],
            "/home/u/.local/state/taste-ide/environments/abc123/review/repo",
            "the cwd must be the HOST path, or the adapter loses its history"
        );

        // The container name comes last before the command.
        let container = args
            .iter()
            .position(|a| a == "taste-abc123-review")
            .unwrap();
        assert_eq!(args[container + 1], "node", "the forwarder wraps the agent");
        assert!(args[container..].contains(&"npx".to_string()));
    }

    /// Flags stay paired and every `--env` carries a value, the failure
    /// that once made podman parse a value as the image.
    #[test]
    fn flags_keep_their_values() {
        for sandboxed in [false, true] {
            let (program, args) = args_for(sandboxed);
            assert_eq!(program, if sandboxed { "flatpak-spawn" } else { "podman" });
            if sandboxed {
                assert_eq!(&args[..2], ["--host", "podman"]);
            }
            let end = args
                .iter()
                .position(|a| a == "taste-abc123-review")
                .unwrap();
            for (index, arg) in args[..end].iter().enumerate() {
                if arg == "--env" || arg == "--workdir" {
                    let value = args
                        .get(index + 1)
                        .unwrap_or_else(|| panic!("{arg} at end of args: {args:?}"));
                    assert!(
                        !value.starts_with("--"),
                        "{arg} followed by flag {value}: {args:?}"
                    );
                }
            }
        }
    }

    /// Pitfall 2: history lives under HOME. Both topologies must name the
    /// same mount point, or relocating an agent loses its conversation.
    #[test]
    fn home_is_the_agent_home_the_supervisor_mounts() {
        let (_, args) = args_for(false);
        assert!(args.contains(&format!(
            "HOME={}",
            taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
        )));
    }

    /// The agent's own git inside the container is still push-blocked, and
    /// still cannot be steered by a repo-supplied hook.
    #[test]
    fn the_git_policy_rides_into_the_container() {
        let (_, args) = args_for(false);
        let joined = args.join(" ");
        for (key, value) in taste_core::policy::agent_git_config_env() {
            assert!(joined.contains(&format!("--env {key}={value}")), "{joined}");
        }
        assert!(joined.contains("pushInsteadOf"), "{joined}");
        assert!(joined.contains("core.hooksPath"), "{joined}");
        // GIT_CONFIG_GLOBAL would REPLACE the container's config and take
        // the inherited identity with it.
        assert!(!joined.contains("GIT_CONFIG_GLOBAL"), "{joined}");
    }

    /// The placeholder is the agent's only credential, in this topology as
    /// in every other, and it has to survive into the container.
    #[test]
    fn the_placeholder_reaches_the_relocated_agent() {
        let (_, args) = args_for(false);
        let joined = args.join(" ");
        assert!(joined.contains("--env ANTHROPIC_AUTH_TOKEN=sk-ant-taste-abc"));
        assert!(joined.contains(&format!("--env TASTE_IDE_CONFINEMENT={CONFINEMENT}")));
        assert!(joined.contains("--env TASTE_IDE_VERSION="));
        // Environment before the container name, or podman reads it as the
        // command.
        let container = args
            .iter()
            .position(|a| a == "taste-abc123-review")
            .unwrap();
        let token = args
            .iter()
            .position(|a| a == "ANTHROPIC_AUTH_TOKEN=sk-ant-taste-abc")
            .unwrap();
        assert!(token < container);
    }

    /// The forwarder is what makes the proxy reachable from a container
    /// with its own netns: a unix socket in, loopback HTTP out, and the
    /// agent started only once it is listening.
    #[test]
    fn the_forwarder_bridges_the_socket_and_starts_the_agent_after_listening() {
        let argv = inner_command(
            &spec(),
            Some(&AuthForward {
                socket: PathBuf::from("/tmp/taste-ide-review/auth.sock"),
            }),
        );
        assert_eq!(argv[0], "node");
        assert_eq!(argv[1], "-e");
        assert_eq!(argv[3], "/tmp/taste-ide-review/auth.sock");
        assert_eq!(&argv[4..], ["npx", "acp"]);

        let js = &argv[2];
        // Both directions, or the agent talks and never hears back.
        assert!(js.contains("c.pipe(u)") && js.contains("u.pipe(c)"), "{js}");
        // Ephemeral port, handed to the child — no fixed port to collide
        // with whatever the repo's own container runs.
        assert!(js.contains("srv.listen(0,'127.0.0.1'"), "{js}");
        assert!(js.contains("ANTHROPIC_BASE_URL"), "{js}");
        assert!(js.contains("srv.address().port"), "{js}");
        // The agent is spawned INSIDE the listen callback: no race, by
        // construction rather than by a sleep.
        let listening = js.find("srv.listen(0").unwrap();
        let spawning = js.find("cp.spawn(").unwrap();
        assert!(spawning > listening, "{js}");
        // stdio untouched: ACP speaks over it.
        assert!(js.contains("stdio:'inherit'"), "{js}");
        // A forwarder that cannot bind must SAY so rather than hang.
        assert!(js.contains("console.error"), "{js}");
        assert!(!js.contains("socat"), "{js}");
    }

    /// Both addresses a relocated agent dials are INSIDE the container.
    ///
    /// This is the whole of the socket-direction inversion, as a property of
    /// the values: a host path here would be a socket the unconfined IDE
    /// bound, which a `container_t` process is refused `connectto` on — the
    /// EACCES that made relocation impossible on every SELinux-enforcing
    /// host. Both endpoints are bound by the container's own channel helper
    /// instead.
    #[test]
    fn everything_a_relocated_agent_dials_is_inside_the_container() {
        let env = taste_core::environment::EnvironmentId::parse("review").unwrap();
        let relocation = relocation();
        for socket in [
            &relocation.mcp_socket,
            &relocation.auth.as_ref().unwrap().socket,
        ] {
            assert!(
                socket.starts_with(taste_core::environment::container_channel_dir(&env)),
                "{socket:?} is not an endpoint the container binds for itself"
            );
        }
        // ...and they are not the aim's host socket, which stays correct for
        // every outside-confined topology and wrong for this one.
        let aim = crate::AgentAim::new(Path::new("/work/p"), env, "/usr/bin/taste-ide", true);
        assert_ne!(relocation.mcp_socket, aim.mcp_socket);
    }

    /// No proxy for this spawn (a non-Anthropic agent, or the opt-out):
    /// nothing is wrapped, and the agent is exec'd directly.
    #[test]
    fn without_the_proxy_the_agent_is_exec_d_directly() {
        let mut spec = spec();
        spec.env.clear();
        let (_, args) = relocated_agent_command(
            &spec,
            Path::new("/work/p"),
            &Relocation {
                container: "c".into(),
                podman: taste_core::PodmanTarget::local(false),
                mcp_socket: PathBuf::from("/tmp/taste-ide-p/mcp.sock"),
                auth: None,
            },
        );
        let container = args.iter().position(|a| a == "c").unwrap();
        assert_eq!(&args[container + 1..], ["npx", "acp"]);
        assert!(!args.iter().any(|a| a.contains("createServer")));
    }
}
