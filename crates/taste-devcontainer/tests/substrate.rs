//! The substrate, against a **real non-local podman**.
//!
//! The unit tests say what the abstraction composes. These say that what it
//! composes actually works when the containers are not on this host — which
//! is the only half that matters, because the whole claim of the substrate
//! work is that nothing downstream had to change.
//!
//! Three things are proven here, in order of how much they would have cost
//! to discover late:
//!
//! 1. **Lifecycle through the connection** — an environment builds its image
//!    and starts its container over there, not here. (The image build is the
//!    part that mattered most: repo-supplied `RUN` steps are the earliest
//!    untrusted code in the system, and a substrate that covered runs but
//!    not builds would have missed them.)
//! 2. **Exec through the connection** — `ExecContext`, the same object
//!    `ide_exec`, the user's terminals and the language server resolve
//!    against, lands a command inside that container.
//! 3. **The environment channel through the connection** — the transport
//!    everything else rides. It is stdio over `podman exec`, so it *should*
//!    be transport-agnostic; this is the test that says it is, and it makes
//!    the container answer the IDE's own [`REACH_PROBE`] rather than a
//!    paraphrase of it.
//!
//! # Running it
//!
//! Gated on a connection being named, because there is nothing to test
//! without one and CI has no KVM:
//!
//! ```sh
//! # a machine is an ssh-reachable podman host; point at its own connection
//! podman machine init --cpus 8 --memory 7936 --disk-size 64 taste-ide
//! podman machine start taste-ide
//! cargo test -p taste-devcontainer --test substrate --no-run   # in the devcontainer
//! TASTE_PODMAN_CONNECTION=taste-ide ./target/debug/deps/substrate-* \
//!     --ignored --nocapture --test-threads=1
//! ```
//!
//! # On proving "remote" with a local machine
//!
//! A running podman machine **is** an ssh-reachable podman host: `podman
//! system connection list` shows its endpoint as `ssh://core@127.0.0.1:PORT`,
//! and `podman -c <name>` reaches it the same way it would reach a machine
//! in another building. Pointing the remote provider at it exercises the
//! whole path — connection resolution, `-c` on every invocation, exec over
//! ssh, the channel's stdio across the boundary — with nothing faked.
//!
//! What a genuinely foreign host would differ in is **not** the transport:
//! it is the files. A machine shares `$HOME` over virtiofs, so an
//! environment's checkout exists at the same path on both sides; a foreign
//! host does not, and the clone would have to live there. That gap is
//! clone locality, it is deliberately out of this batch, and it is the gate
//! the real remote and cloud tiers wait behind — see `docs/ENVIRONMENTS.md`
//! → "Remote substrate".

use std::path::Path;
use std::sync::{Arc, Mutex};

use taste_core::environment::EnvironmentId;
use taste_core::{ConfigAuthority, EventBus, ExecContext};
use taste_devcontainer::channel::{ChannelServices, ChannelStream, Service};
use taste_devcontainer::{EnvironmentIdentity, Substrate, Supervisor, SupervisorState};

/// The connection under test, or `None` — in which case every test here
/// skips loudly rather than passing vacuously.
fn connection() -> Option<String> {
    std::env::var("TASTE_PODMAN_CONNECTION")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

macro_rules! require_connection {
    () => {
        match connection() {
            Some(name) => name,
            None => {
                eprintln!(
                    "SKIP: set TASTE_PODMAN_CONNECTION=<podman connection> to run the \
                     substrate tests against a real non-local podman"
                );
                return;
            }
        }
    };
}

fn substrate() -> Arc<Substrate> {
    Substrate::connection_for_tests(&connection().expect("checked by require_connection"))
}

/// Removes whatever a test started, however it ends — through the same
/// connection it was started on, which is itself part of the point.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = substrate()
            .std_command(&[
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "1".into(),
                self.0.clone(),
            ])
            .output();
    }
}

/// A directory a container on this substrate can actually bind.
///
/// **`/tmp` is not shareable into a podman machine at all** — `podman
/// machine init --volume` refuses that destination by name, and the default
/// share is `$HOME:$HOME`. A tempdir there is invisible over the connection,
/// and podman says so loudly (`statfs ...: no such file or directory`)
/// rather than mounting an empty directory — the good failure mode, but
/// still a failure.
///
/// Real environments are never in `/tmp`: a workspace is the user's own
/// checkout and every clone lives under `$XDG_STATE_HOME`. So this is a
/// fixture concern only. It is written down here because it is the one
/// compatibility rule the substrate imposes on the rest of the IDE — every
/// host path bound into a container must be under the shared set — and a
/// future path staged in `/tmp` will fail exactly here first.
fn tempdir() -> tempfile::TempDir {
    match connection() {
        None => tempfile::tempdir().unwrap(),
        Some(_) => {
            let base = std::path::PathBuf::from(std::env::var("HOME").expect("HOME"))
                .join(".cache/taste-ide/tests");
            std::fs::create_dir_all(&base).unwrap();
            tempfile::Builder::new()
                .prefix("substrate-")
                .tempdir_in(&base)
                .unwrap()
        }
    }
}

fn workspace_without_a_config() -> tempfile::TempDir {
    let dir = tempdir();
    std::fs::write(dir.path().join("README.md"), "# no devcontainer here\n").unwrap();
    dir
}

fn supervisor(root: &Path) -> Arc<Supervisor> {
    let exec = ExecContext::host_unsandboxed_for_tests();
    let substrate = substrate();
    exec.set_podman_target(substrate.target().clone());
    Supervisor::new_outside_container_for_tests(
        EnvironmentIdentity::primary(root),
        EventBus::new(),
        exec,
        substrate,
    )
}

/// The smallest thing that can be on the IDE's end of a channel.
///
/// It answers the MCP reach probe's JSON-RPC `ping` and records every
/// environment it was asked for, so the test can assert that the identity
/// came from *which channel the bytes arrived on* — the property
/// ENVIRONMENTS.md rests the whole socket-is-the-identity design on, which
/// has no reason to survive a change of transport unless it is checked.
struct ProbeServices {
    seen: Mutex<Vec<(EnvironmentId, Service)>>,
}

impl ChannelServices for ProbeServices {
    fn serves(&self, service: Service) -> bool {
        // Only MCP: the auth proxy is not running in this test, and the
        // hosting probe must not fail an environment for a door the IDE
        // never opened.
        service == Service::Mcp
    }

    fn accept(&self, env: &EnvironmentId, service: Service, stream: ChannelStream) {
        self.seen.lock().unwrap().push((env.clone(), service));
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let (read, mut write) = tokio::io::split(stream);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Echo the id back, which is exactly what the probe checks
                // for: a real answer from the IDE, not a socket that exists.
                let id = line
                    .split("\"id\":")
                    .nth(1)
                    .and_then(|rest| rest.split(&[',', '}'][..]).next())
                    .unwrap_or("null")
                    .trim()
                    .to_string();
                let reply = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}\n");
                if write.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// The whole substrate claim in one run: an environment builds, starts,
/// executes and talks to the IDE — all on a podman that is not this host's.
///
/// It is one test rather than four because the four share a container that
/// costs a minute to build, and because they are only interesting together:
/// a channel into a container the connection did not start would prove
/// nothing about the connection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs TASTE_PODMAN_CONNECTION naming a reachable podman connection"]
async fn an_environment_lives_entirely_on_a_non_local_podman() {
    let name = require_connection!();
    eprintln!("substrate: podman connection {name}");

    // The connection is genuinely not this host: a machine runs its own
    // kernel, and a remote host runs someone else's. Either way the kernel
    // over there is a fact we can print, and the isolation claim is exactly
    // this difference.
    let there = substrate()
        .std_command(&["info".into(), "--format".into(), "{{.Host.Kernel}}".into()])
        .output()
        .expect("asking the connection for its kernel");
    assert!(
        there.status.success(),
        "the connection did not answer: {}",
        String::from_utf8_lossy(&there.stderr)
    );
    eprintln!(
        "kernel over there: {}",
        String::from_utf8_lossy(&there.stdout).trim()
    );

    let workspace = workspace_without_a_config();
    let sup = supervisor(workspace.path());
    let _cleanup = Cleanup(sup.container_name());

    // --- 1. lifecycle: build and start, over there -----------------------
    //
    // No project config, so this is the IDE's own baseline — which means
    // the image is BUILT through the connection, not merely pulled. That is
    // the half the spike said had to be covered and could not be faked.
    sup.reload().await.unwrap_or_else(|e| {
        panic!(
            "the environment did not come up on {name}: {e:#}\nlog:\n{}",
            sup.logs_tail(60).join("\n")
        )
    });
    assert!(
        matches!(sup.state(), SupervisorState::Running { .. }),
        "state was {:?}; log:\n{}",
        sup.state(),
        sup.logs_tail(40).join("\n")
    );
    assert_eq!(sup.config_authority(), ConfigAuthority::Baseline);

    // The container is on the connection and nowhere else. Asking the LOCAL
    // podman for it must come back empty, or the test proved nothing.
    let here = std::process::Command::new("podman")
        .args([
            "ps",
            "--filter",
            &format!("name=^{}$", sup.container_name()),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .expect("asking local podman");
    assert!(
        String::from_utf8_lossy(&here.stdout).trim().is_empty(),
        "the container turned up on the LOCAL podman — the connection was not used"
    );

    // --- 2. exec: through the ExecContext, the way everything does --------
    let spec = sup
        .exec()
        .resolve("sh", &["-c", "echo substrate-exec-ok; uname -r"], false);
    assert_eq!(
        spec.args.first().map(String::as_str),
        Some("-c"),
        "every resolved command carries the connection: {:?}",
        spec.args
    );
    let out = std::process::Command::new(&spec.program)
        .args(&spec.args)
        .output()
        .expect("exec through the environment's ExecContext");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("substrate-exec-ok"),
        "exec over the connection failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!(
        "kernel inside the container: {}",
        stdout.lines().nth(1).unwrap_or("?")
    );

    // --- 3. the channel: the transport everything else rides -------------
    //
    // The IDE execs a helper into the container; the helper binds a socket
    // *inside* it; something in the container dials that socket and gets a
    // real answer out of the IDE. Every byte of that crosses the connection
    // in both directions.
    let services = Arc::new(ProbeServices {
        seen: Mutex::new(Vec::new()),
    });
    sup.set_channel_services(services.clone());

    let channel = sup
        .ensure_channel()
        .await
        .expect("the environment channel must open over the connection");
    assert!(channel.alive());
    assert_eq!(
        channel.environment(),
        sup.id(),
        "the channel speaks for the environment the IDE exec'd into"
    );

    // The production probe, not a paraphrase of it: this is the same
    // question `AgentHosting` asks, so a pass here is a pass for relocation.
    let hosting = sup.probe_agent_hosting().await;
    assert_eq!(
        hosting,
        taste_devcontainer::AgentHosting::Yes,
        "the container could not reach the IDE through the channel over {name}; log:\n{}",
        sup.logs_tail(30).join("\n")
    );

    // And the identity came from the channel, never from anything the
    // container said.
    let seen = services.seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|(env, service)| env == sup.id() && *service == Service::Mcp),
        "the IDE saw {seen:?}, and should have seen this environment's MCP"
    );

    // --- 4. the substrate is visible where the user looks ----------------
    let resources = sup.list_resources().await;
    assert!(
        resources
            .iter()
            .any(|r| r.kind == taste_devcontainer::ResourceKind::Substrate),
        "a non-local substrate must appear in the Resources view: {resources:?}"
    );
}

/// A machine that has been recreated takes every container with it, and the
/// supervisor's own state lives on the host and does not go with them.
///
/// This is the cheap version of that: the container is removed out from
/// under a supervisor that believes it is running. What the supervisor must
/// NOT do is keep saying `Running` — `ide_exec` would then fail with
/// podman's "no such container" instead of the IDE's "this environment is
/// down", and a chat would keep trying to relocate into nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs TASTE_PODMAN_CONNECTION naming a reachable podman connection"]
async fn a_container_that_vanished_from_the_substrate_is_reported_down() {
    let _name = require_connection!();
    let workspace = workspace_without_a_config();
    let sup = supervisor(workspace.path());
    let _cleanup = Cleanup(sup.container_name());

    sup.reload().await.expect("the environment must come up");
    assert!(sup.exec().has_exec_target());
    assert!(
        sup.reconcile_container_presence().await,
        "it is running, so it is present"
    );

    // Pull the floor out — what a `podman machine rm` does to every
    // container at once.
    let _ = substrate()
        .std_command(&[
            "rm".into(),
            "-f".into(),
            "-t".into(),
            "1".into(),
            sup.container_name(),
        ])
        .output();

    assert!(
        !sup.reconcile_container_presence().await,
        "the container is gone and the environment must say so"
    );
    assert_eq!(sup.state(), SupervisorState::Stopped);
    assert!(
        !sup.exec().has_exec_target(),
        "nowhere to run, and the host is not a fallback"
    );
    assert_eq!(
        sup.agent_hosting(),
        taste_devcontainer::AgentHosting::Unknown
    );
}
