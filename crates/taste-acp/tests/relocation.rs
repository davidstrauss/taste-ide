//! A REAL relocation, against real podman.
//!
//! Everything else about relocation is tested by composing an argv and
//! reading it back, which proves the IDE's intent and nothing about the
//! world. This starts an actual container from the repo's own devcontainer
//! image, spawns the fake agent INSIDE it through the same
//! `AgentClient::spawn` the IDE uses, and asks the agent what it can see.
//! Its answers are the whole of phase 4:
//!
//! - it is in the container (`TASTE_IDE_CONFINEMENT=container-exec`),
//! - `HOME` is the per-environment volume, where its history goes,
//! - the workspace is at its REAL host path, so the adapter's cwd-keyed
//!   history and every path it exchanges with the IDE mean the same thing
//!   on both sides,
//! - its git carries the agent policy,
//! - and an API call at `ANTHROPIC_BASE_URL` reaches the IDE's auth proxy
//!   and gets the real credential swapped in — from inside a network
//!   namespace where the proxy's loopback port does not exist.
//!
//! Two tests, because the last one needs something the others do not: a
//! container allowed to dial the IDE's sockets. See
//! `a_relocated_agent_pays_for_its_turns_through_the_ide` for why that is a
//! property of the HOST and not of this code.
//!
//! `#[ignore]`d because it needs podman and the image. It costs no tokens
//! (the upstream is a mock) and touches no credential file.
//!
//! Run it where podman is reachable:
//!
//! ```sh
//! cargo test -p taste-acp --test relocation -- --ignored --nocapture
//! ```
//!
//! On a machine set up the way CLAUDE.md describes — cargo in a container,
//! podman only on the host — build the binary in the devcontainer and run
//! it outside, pointing `TASTE_TEST_REPO` at the checkout:
//!
//! ```sh
//! cargo test -p taste-acp --test relocation --no-run   # in the container
//! TASTE_TEST_REPO=$PWD ./target/debug/deps/relocation-* \
//!     --ignored --nocapture --test-threads=1           # on the host
//! ```

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use taste_acp::{AgentClient, AgentHome, AgentSpec, AuthForward, Relocation, SessionEvent};

/// The image to start the stand-in environment from. The repo's own
/// devcontainer image by default: it has node (the auth forwarder and, in
/// real life, the adapter) and python3 (this fake agent).
fn image() -> String {
    std::env::var("TASTE_TEST_IMAGE").unwrap_or_else(|_| "taste-ide-devcontainer".to_string())
}

fn podman(args: &[&str]) -> std::process::Output {
    Command::new("podman")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running `podman {}`: {e}", args.join(" ")))
}

fn podman_ok(args: &[&str]) -> String {
    let out = podman(args);
    assert!(
        out.status.success(),
        "podman {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A stand-in for one environment's devcontainer: the container plus the
/// agent-home volume, removed however the test ends.
struct TestEnvironment {
    container: String,
    volume: String,
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        let _ = podman(&["rm", "-f", "-t", "1", &self.container]);
        let _ = podman(&["volume", "rm", "-f", &self.volume]);
    }
}

impl TestEnvironment {
    /// Start a container mounted the way `Supervisor::ide_mounts` mounts
    /// one: the checkout at its host path, this environment's agent-home
    /// volume at the shared agent home, and the IDE sockets it has to
    /// reach.
    ///
    /// `socket_reach` asks for a container that is allowed to dial those
    /// sockets. On an SELinux-enforcing host a confined one is not, and the
    /// IDE's own answer to that is to refuse relocation rather than to
    /// weaken the container — so a fixture that wants to exercise the auth
    /// chain has to stand in for a host where it is permitted.
    fn start(root: &Path, auth_socket: &Path, socket_reach: bool) -> Self {
        let unique = std::process::id();
        let suffix = if socket_reach { "reach" } else { "confined" };
        let container = format!("taste-relocation-test-{unique}-{suffix}");
        let volume = format!("taste-relocation-test-{unique}-{suffix}-home");
        let _ = podman(&["rm", "-f", "-t", "1", &container]);

        let root = root.display().to_string();
        let home = taste_core::policy::AGENT_HOME_IN_DEVCONTAINER;
        let image = image();
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.clone(),
            // The host user is uid 1000 in here, so the checkout, the
            // volume and the socket are all readable as the same person.
            "--userns=keep-id:uid=1000,gid=1000".into(),
            // The double bind: the checkout at its REAL host path. This is
            // the one line that defuses two of the three pitfalls.
            "-v".into(),
            format!("{root}:{root}:Z"),
            "-v".into(),
            format!("{volume}:{home}"),
            "-v".into(),
            format!("{}:{}:z", auth_socket.display(), auth_socket.display()),
            "-w".into(),
            root.clone(),
        ];
        if socket_reach {
            args.push("--security-opt".into());
            args.push("label=disable".into());
        }
        args.push(image);
        args.push("sleep".into());
        args.push("infinity".into());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        podman_ok(&args);

        // What `Supervisor::probe_agent_hosting` does after a start: a
        // brand-new named volume can arrive owned by container-root, and
        // an agent that cannot write its home has nowhere to keep a
        // conversation.
        let owner = podman_ok(&["exec", &container, "sh", "-c", "id -u; id -g"]);
        let owner: Vec<&str> = owner.split_whitespace().collect();
        podman_ok(&[
            "exec",
            "--user",
            "root",
            &container,
            "sh",
            "-c",
            &format!(
                "mkdir -p {home} && chown -R {}:{} {home}",
                owner[0], owner[1]
            ),
        ]);
        Self { container, volume }
    }

    /// The reachability question `Supervisor::probe_socket_reach` asks, in
    /// the same words: can something in here dial that socket?
    fn can_reach(&self, socket: &Path) -> bool {
        podman(&[
            "exec",
            &self.container,
            "node",
            "-e",
            "const c=require('net').connect(process.argv[1]);\
             c.on('connect',()=>{c.destroy();process.exit(0)});\
             c.on('error',e=>{console.error(e.code);process.exit(1)});",
            &socket.display().to_string(),
        ])
        .status
        .success()
    }
}

/// A mock Anthropic API on loopback: it reports the credential it was
/// given, so the test can prove the placeholder was swapped for it.
async fn mock_upstream() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(|req: Request<Incoming>| async move {
                    let seen = req
                        .headers()
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>")
                        .to_string();
                    Ok::<_, Infallible>(Response::new(BodyExt::boxed(Full::new(Bytes::from(
                        format!(r#"{{"upstream_saw":"{seen}","usage":{{"input_tokens":7}}}}"#),
                    )))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    format!("http://{addr}")
}

async fn next_event(client: &AgentClient) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(60), client.events.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("event channel closed")
}

/// Send one prompt and return the agent's answer, consuming the whole turn.
///
/// Draining to `TurnEnded` matters: leaving it queued would make the NEXT
/// question read the previous turn's ending and report an answer that never
/// came.
async fn ask(client: &AgentClient, prompt: &str) -> String {
    client.prompt(prompt).unwrap();
    let mut answer = String::new();
    loop {
        match next_event(client).await {
            SessionEvent::Update(SessionUpdate::AgentMessageChunk(chunk)) => {
                let ContentBlock::Text(text) = &chunk.content else {
                    panic!("expected text");
                };
                answer.push_str(&text.text);
            }
            SessionEvent::TurnEnded { .. } => {
                assert!(
                    !answer.is_empty(),
                    "turn ended with no answer to {prompt:?}"
                );
                return answer;
            }
            SessionEvent::Closed(e) => panic!("agent died: {e:?}"),
            _ => continue,
        }
    }
}

/// Everything both live tests need standing up: a mock upstream, the IDE's
/// auth proxy on both its doors, a container, and a relocated agent talking
/// to us from inside it.
struct Relocated {
    client: AgentClient,
    environment: TestEnvironment,
    proxy: taste_authproxy::Handle,
    placeholder: String,
    root: PathBuf,
    auth_socket: PathBuf,
    _socket_dir: tempfile::TempDir,
    _transport: taste_authproxy::UnixTransport,
}

impl Relocated {
    async fn start(socket_reach: bool) -> Self {
        if Command::new("podman").arg("--version").output().is_err() {
            panic!("podman is not on PATH; this test is about a real container");
        }
        let image = image();
        assert!(
            podman(&["image", "exists", &image]).status.success(),
            "image {image} is not present — build it with \
             `podman build -t {image} .devcontainer`"
        );

        // The environment's checkout, standing in for a clone: this repo,
        // so there are real files to see and the fake agent's own script is
        // reachable at its host path from inside the container.
        //
        // `TASTE_TEST_REPO` overrides the compiled-in manifest dir, because
        // on a machine developed the way CLAUDE.md describes, cargo runs in
        // a container and podman does not: the binary is built in one place
        // and has to run in another. `env!` is baked at compile time and
        // would name a path that exists only in the builder.
        let repo: PathBuf = std::env::var("TASTE_TEST_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .canonicalize()
            .expect("the repo root must exist where this test RUNS, not where it was built");
        let script = format!("{}/crates/taste-acp/tests/fake_agent.py", repo.display());

        // The IDE's auth proxy, on both its doors, against a mock upstream
        // that costs nothing and reports what credential reached it.
        let upstream = mock_upstream().await;
        let proxy = taste_authproxy::AuthProxy::spawn(
            upstream.parse().unwrap(),
            Arc::new(taste_authproxy::StaticKey::api_key("the-real-credential")),
        )
        .unwrap();
        let socket_dir = tempfile::Builder::new()
            .prefix("taste-relocation-")
            .tempdir_in("/tmp")
            .unwrap();
        let auth_socket = socket_dir.path().join("auth.sock");
        let transport = proxy.listen_unix(&auth_socket).unwrap();
        let placeholder = proxy.issue_placeholder("test-env");

        let environment = TestEnvironment::start(&repo, &auth_socket, socket_reach);

        // Everything from here is the IDE's own spawn path, with the values
        // `ChatPane` would compute.
        let mut spec = AgentSpec::new("fake", "Fake Agent", "python3", &[&script], &[]);
        spec.env
            .push(("ANTHROPIC_BASE_URL".into(), proxy.base_url()));
        spec.env
            .push(("ANTHROPIC_AUTH_TOKEN".into(), placeholder.clone()));

        let client = AgentClient::spawn(
            spec,
            repo.clone(),
            None,
            None,
            AgentHome {
                environment: "test-env".into(),
                volume: environment.volume.clone(),
            },
            Some(Relocation {
                container: environment.container.clone(),
                auth: Some(AuthForward {
                    socket: auth_socket.clone(),
                }),
            }),
            false,
            None,
            None,
        )
        .expect("spawning the relocated agent");

        loop {
            match next_event(&client).await {
                SessionEvent::Ready { .. } => break,
                SessionEvent::Closed(e) => panic!("the relocated agent never came up: {e:?}"),
                _ => continue,
            }
        }
        Self {
            client,
            environment,
            proxy,
            placeholder,
            root: repo,
            auth_socket,
            _socket_dir: socket_dir,
            _transport: transport,
        }
    }

    async fn ask(&self, prompt: &str) -> String {
        ask(&self.client, prompt).await
    }
}

/// The topology, as the agent itself can see it. This runs against an
/// ORDINARY confined container — the one the supervisor would start — so
/// everything asserted here is what a real relocation delivers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_relocated_agent_runs_beside_its_files() {
    let live = Relocated::start(false).await;
    let root = live.root.clone();

    // 1. It is in the container, and says which topology it is in.
    assert_eq!(
        live.ask("/env TASTE_IDE_CONFINEMENT").await,
        format!("env {}", taste_acp::relocate::CONFINEMENT)
    );

    // 2. HOME is the per-environment volume: where the history goes, and
    //    the same path the outside-confined topology uses. Pitfall 2.
    let home = taste_core::policy::AGENT_HOME_IN_DEVCONTAINER;
    assert_eq!(live.ask("/env HOME").await, format!("env {home}"));
    assert_eq!(
        live.ask(&format!("/exists {home}")).await,
        "exists yes",
        "the agent home must be writable-there to keep a conversation in"
    );

    // 3. The workspace is at its REAL host path — pitfalls 1 and 3, as one
    //    observable fact. A relocated agent that saw `/workspaces/...`
    //    would key its history where the outside-confined one cannot find
    //    it, and every path it exchanged with the IDE would need
    //    translating.
    assert_eq!(
        live.ask(&format!("/exists {}/Cargo.toml", root.display()))
            .await,
        "exists yes",
        "the checkout must be visible at the host path, not a container path"
    );
    assert_eq!(
        live.ask(&format!("/exists {}/CLAUDE.md", root.display()))
            .await,
        "exists yes",
        "and it is the real checkout, not the stand-in"
    );

    // 4. The git policy rode in with the spawn, so the agent's own git
    //    inside the container is still push-blocked and hook-masked.
    assert_eq!(
        live.ask("/env GIT_CONFIG_COUNT").await,
        format!("env {}", taste_core::policy::agent_git_config().len())
    );

    // 5. The forwarder replaced the base URL with a port it is listening on
    //    in HERE. The IDE's own loopback address means nothing inside a
    //    container with its own network namespace, and an unmodified value
    //    would be the bug this indirection exists to prevent.
    let base = live.ask("/env ANTHROPIC_BASE_URL").await;
    let base = base.strip_prefix("env ").unwrap().to_string();
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");
    assert_ne!(base, live.proxy.base_url());

    // 6. And the honest limit, asserted rather than assumed: on an
    //    SELinux-enforcing host this container may NOT dial the IDE's
    //    sockets, whatever is mounted where. That is exactly the condition
    //    `Supervisor::probe_socket_reach` asks about and refuses
    //    relocation on, so whichever way this machine answers, the IDE's
    //    behaviour is defined — and the auth chain itself is proven by the
    //    test below.
    let reachable = live.environment.can_reach(&live.auth_socket);
    println!(
        "socket reach from a confined container: {}",
        if reachable {
            "permitted — relocation is offered on this host"
        } else {
            "REFUSED by policy — the IDE keeps this chat outside-confined"
        }
    );
}

/// The auth chain, end to end, from inside a container.
///
/// Deliberately against a fixture container that is ALLOWED to dial the
/// IDE's sockets. On Fedora Silverblue 44 an ordinary `container_t`
/// container is not: it is refused `connectto` on a socket served by the
/// unconfined IDE, so `connect(2)` returns EACCES however the socket is
/// mounted or labelled. That is a host-policy fact, not a bug in the chain
/// below, and the IDE's answer to it is to refuse relocation there rather
/// than to weaken the container. What this test proves is the other half:
/// that where a container may reach the socket, the whole path works —
/// forwarder, unix transport, placeholder swap, and spend attribution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_relocated_agent_pays_for_its_turns_through_the_ide() {
    let live = Relocated::start(true).await;
    assert!(
        live.environment.can_reach(&live.auth_socket),
        "the fixture container must be able to dial the proxy socket"
    );

    let base = live.ask("/env ANTHROPIC_BASE_URL").await;
    let base = base.strip_prefix("env ").unwrap().to_string();

    let answer = live.ask(&format!("/get {base}/v1/messages")).await;
    assert!(
        answer.contains("the-real-credential"),
        "the request did not reach the upstream with the real credential \
         swapped in: {answer}"
    );
    assert!(
        !answer.contains(&live.placeholder),
        "the placeholder leaked upstream: {answer}"
    );

    // ...and the proxy counted it against this environment, which is what
    // makes the placeholder attribution rather than decoration.
    let spend = live.proxy.spend("test-env");
    assert_eq!(spend.requests, 1, "the proxy did not see the request");
    assert_eq!(spend.input_tokens, 7, "usage counters did not stream past");
}
