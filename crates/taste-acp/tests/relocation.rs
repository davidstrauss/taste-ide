//! A REAL relocation, against real podman, on a real confined container.
//!
//! Everything else about relocation is tested by composing an argv and
//! reading it back, which proves the IDE's intent and nothing about the
//! world. This starts an actual container from the repo's own devcontainer
//! image, opens the environment channel into it, spawns the fake agent
//! INSIDE it through the same `AgentClient::spawn` the IDE uses, and asks
//! the agent what it can see. Its answers are the whole of phase 4 plus the
//! socket-direction inversion that unblocked it:
//!
//! - it is in the container (`TASTE_IDE_CONFINEMENT=container-exec`),
//! - `HOME` is the per-environment volume, where its history goes,
//! - the workspace is at its REAL host path, so the adapter's cwd-keyed
//!   history and every path it exchanges with the IDE mean the same thing
//!   on both sides,
//! - its git carries the agent policy,
//! - the IDE's **MCP tools answer it** — through the bridge, the channel's
//!   in-container socket, the helper, and the demux — and answer *as its
//!   environment*,
//! - and an API call at `ANTHROPIC_BASE_URL` reaches the IDE's auth proxy
//!   and gets the real credential swapped in, moving that environment's
//!   spend counters.
//!
//! **The container here is an ordinary confined one.** It was not before:
//! phase 4's fixture needed `--security-opt label=disable` to exercise the
//! auth chain, because a `container_t` process is refused `connectto` on a
//! socket the unconfined IDE bound, and the IDE's honest answer was to
//! refuse relocation on such hosts. That crutch is gone with the direction
//! it was propping up — the endpoints are inside the container now, so
//! nothing has to be exempted from anything. If these tests pass on an
//! enforcing host, relocation works there.
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
use taste_core::environment::EnvironmentId;
use taste_devcontainer::channel::{ChannelServices, ChannelStream, EnvChannel, Service};
use taste_devcontainer::Substrate;

/// Which podman this run talks to. Local unless pointed elsewhere, exactly
/// as the IDE itself resolves it — so the whole relocation proof can be
/// re-run against a machine or a remote host without a second copy of it:
///
/// ```sh
/// TASTE_PODMAN_CONNECTION=taste-ide ./target/debug/deps/relocation-* --ignored
/// ```
fn substrate() -> Arc<Substrate> {
    match std::env::var("TASTE_PODMAN_CONNECTION") {
        Ok(name) if !name.trim().is_empty() => Substrate::connection_for_tests(name.trim()),
        _ => Substrate::local_for_tests(),
    }
}

/// The image to start the stand-in environment from. The repo's own
/// devcontainer image by default: it has node (the channel helper, the auth
/// forwarder, the MCP bridge and, in real life, the adapter) and python3
/// (this fake agent).
fn image() -> String {
    std::env::var("TASTE_TEST_IMAGE").unwrap_or_else(|_| "taste-ide-devcontainer".to_string())
}

fn podman(args: &[&str]) -> std::process::Output {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    substrate()
        .std_command(&owned)
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

/// What this host's SELinux is doing, for the record. The whole batch is
/// about `Enforcing`, and a green run on `Permissive` proves much less.
fn selinux_mode() -> String {
    Command::new("getenforce")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|mode| !mode.is_empty())
        .unwrap_or_else(|| "<no getenforce>".into())
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
    /// one: the checkout at its host path and this environment's agent-home
    /// volume at the shared agent home.
    ///
    /// **And nothing else.** No IDE socket rides in any more, and no
    /// `--security-opt` softens the label — this is the container the
    /// supervisor starts, confined exactly as the repo's own build code
    /// runs in.
    fn start(root: &Path) -> Self {
        let unique = std::process::id();
        let container = format!("taste-relocation-test-{unique}");
        let volume = format!("taste-relocation-test-{unique}-home");
        let _ = podman(&["rm", "-f", "-t", "1", &container]);

        let root = root.display().to_string();
        let home = taste_core::policy::AGENT_HOME_IN_DEVCONTAINER;
        let image = image();
        let args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.clone(),
            // The host user is uid 1000 in here, so the checkout and the
            // volume are readable as the same person.
            "--userns=keep-id:uid=1000,gid=1000".into(),
            // The double bind: the checkout at its REAL host path. This is
            // the one line that defuses two of the three pitfalls.
            "-v".into(),
            format!("{root}:{root}:Z"),
            "-v".into(),
            format!("{volume}:{home}"),
            "-w".into(),
            root.clone(),
            image,
            "sleep".into(),
            "infinity".into(),
        ];
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

/// What the IDE serves down the channel, which in the IDE is
/// `taste_app::env_channel::IdeChannelServices`. Reproduced here rather
/// than imported because the app crate links GTK and this test must not:
/// the routing it stands in for is four lines, and the servers on the far
/// side of it are the real ones.
struct TestServices {
    mcp: Arc<taste_mcp::McpServer>,
    proxy: taste_authproxy::Handle,
}

impl ChannelServices for TestServices {
    fn serves(&self, _service: Service) -> bool {
        true
    }

    fn accept(&self, env: &EnvironmentId, service: Service, stream: ChannelStream) {
        match service {
            Service::Mcp => self.mcp.clone().serve_stream(env.clone(), stream),
            Service::Auth => self.proxy.serve_stream(stream),
        }
    }
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

/// Everything the live tests need standing up: a mock upstream, the IDE's
/// auth proxy and MCP server, a confined container, the environment channel
/// into it, and a relocated agent talking to us from inside.
struct Relocated {
    client: AgentClient,
    proxy: taste_authproxy::Handle,
    placeholder: String,
    root: PathBuf,
    environment: EnvironmentId,
    /// The roster the served terminals surface in — the console's data
    /// source, asserted directly because a tab is a widget and this test
    /// has no GTK.
    shells: taste_core::ShellRoster,
    _env: TestEnvironment,
    _channel: Arc<EnvChannel>,
    _workspace: tempfile::TempDir,
    _state: tempfile::TempDir,
}

/// The environment the fixture speaks for. Deliberately NOT the primary:
/// `ide_environment` returning this slug is the proof that the id came from
/// which channel the bytes arrived on, and not from a default.
const ENV_SLUG: &str = "review";

impl Relocated {
    async fn start() -> Self {
        if !podman(&["--version"]).status.success() {
            panic!("podman did not answer; this test is about a real container");
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

        // The IDE's auth proxy, against a mock upstream that costs nothing
        // and reports what credential reached it.
        let upstream = mock_upstream().await;
        let proxy = taste_authproxy::AuthProxy::spawn(
            upstream.parse().unwrap(),
            Arc::new(taste_authproxy::StaticKey::api_key("the-real-credential")),
        )
        .unwrap();

        // The IDE's MCP server, over a throwaway workspace with a real
        // non-primary environment in it. Small on purpose: what is being
        // proven is that tools answer and answer as THIS environment, and a
        // one-commit repo clones in milliseconds.
        let workspace_dir = tempfile::Builder::new()
            .prefix("taste-relocation-ws-")
            .tempdir_in("/tmp")
            .unwrap();
        let state_dir = tempfile::Builder::new()
            .prefix("taste-relocation-state-")
            .tempdir_in("/tmp")
            .unwrap();
        init_repo(workspace_dir.path());
        let workspace = taste_core::Workspace::open(workspace_dir.path());
        let environments = taste_devcontainer::EnvironmentRegistry::new_for_tests(
            workspace_dir.path(),
            workspace.events.clone(),
            workspace.exec.clone(),
            state_dir.path(),
        );
        let environment = EnvironmentId::parse(ENV_SLUG).unwrap();
        environments
            .create(environment.clone())
            .expect("creating the fixture environment");
        let packager = taste_flatpak::Packager::new(
            workspace_dir.path().to_path_buf(),
            workspace.events.clone(),
        );
        let mcp = taste_mcp::McpServer::new(environments, packager, workspace.clone());

        let env = TestEnvironment::start(&repo);

        // The channel: one `podman exec` into that container, binding the
        // endpoints the agent will dial. This is `Supervisor::ensure_channel`
        // without the supervisor — same function, same arguments.
        let channel = EnvChannel::start(
            environment.clone(),
            &env.container,
            &substrate(),
            Arc::new(TestServices {
                mcp,
                proxy: proxy.clone(),
            }),
        )
        .await
        .expect("opening the environment channel");

        // Everything from here is the IDE's own spawn path, with the values
        // `ChatPane` would compute.
        let placeholder = proxy.issue_placeholder(ENV_SLUG);
        let mut spec = AgentSpec::new("fake", "Fake Agent", "python3", &[&script], &[]);
        spec.env
            .push(("ANTHROPIC_BASE_URL".into(), proxy.base_url()));
        spec.env
            .push(("ANTHROPIC_AUTH_TOKEN".into(), placeholder.clone()));

        // The terminal extension, with exactly the values ChatPane would
        // compute: this environment's id, an ExecContext pointed at its
        // running container, its checkout at the host path that is also its
        // container path, and the workspace roster.
        let exec = taste_core::ExecContext::host_unsandboxed_for_tests();
        exec.set_container(
            &env.container,
            repo.display().to_string(),
            taste_core::ConfigAuthority::Project,
        );
        let shells = taste_core::ShellRoster::new();
        let terminals = taste_acp::TerminalHost {
            environment: environment.clone(),
            exec,
            cwd: repo.clone(),
            roster: shells.clone(),
        };

        let client = AgentClient::spawn(
            spec,
            repo.clone(),
            repo.clone(),
            None,
            None,
            AgentHome {
                environment: ENV_SLUG.into(),
                volume: env.volume.clone(),
            },
            Some(Relocation {
                // The agent execs into the container wherever that
                // container lives — the same value the IDE takes off the
                // supervisor's substrate.
                podman: substrate().target().clone(),
                container: env.container.clone(),
                mcp_socket: channel.paths().mcp.clone(),
                auth: Some(AuthForward {
                    socket: channel.paths().auth.clone(),
                }),
            }),
            Some(terminals),
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
            proxy,
            placeholder,
            root: repo,
            environment,
            shells,
            _env: env,
            _channel: channel,
            _workspace: workspace_dir,
            _state: state_dir,
        }
    }

    async fn ask(&self, prompt: &str) -> String {
        ask(&self.client, prompt).await
    }
}

/// A one-commit git repo, so `EnvironmentRegistry::create` has something to
/// clone.
fn init_repo(root: &Path) {
    let repo = git2::Repository::init(root).unwrap();
    std::fs::write(root.join("README.md"), "fixture\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let who = git2::Signature::now("taste-ide tests", "tests@taste-ide.invalid").unwrap();
    repo.commit(Some("HEAD"), &who, &who, "fixture", &tree, &[])
        .unwrap();
}

/// The topology, as the agent itself can see it — against the ORDINARY
/// confined container the supervisor would start, so everything asserted
/// here is what a real relocation delivers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_relocated_agent_runs_beside_its_files() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;
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
}

/// **The gate this batch exists to lift.** The IDE's tools reach a
/// relocated agent, on a confined container, on an enforcing host.
///
/// Every hop is real: the agent spawns the node bridge the client
/// registered, the bridge dials a unix socket in the container, that socket
/// was bound by the channel helper the IDE `podman exec`'d in, the bytes
/// come back over that exec's stdio, the IDE demultiplexes them and hands
/// the connection to the MCP server. Nothing here is mounted from the host,
/// and nothing is exempted from SELinux.
///
/// Before the inversion this could not pass: the bridge dialled a host
/// socket the unconfined IDE bound, and `connect(2)` returned EACCES.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn the_ides_tools_answer_a_relocated_agent() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;

    let answer = live.ask("/mcp ide_environment").await;
    assert!(
        !answer.contains("ERROR"),
        "the IDE's tools did not reach the relocated agent: {answer}"
    );
    // ...and they answered as THIS environment. The id was never on the
    // wire: it is which channel the bytes arrived on, which is which
    // container the IDE exec'd into — the same unforgeable-by-construction
    // identity the per-environment socket gave.
    assert!(
        answer.contains(&format!("\"id\":\"{}\"", live.environment)),
        "ide_environment did not name this environment: {answer}"
    );
    assert!(
        answer.contains("\"primary\":false"),
        "the environment fell back to the primary instead of routing: {answer}"
    );
    // A tool that names a checkout means this environment's clone, which is
    // the routing working rather than a label being copied through.
    assert!(
        answer.contains(&format!("/{}/repo", live.environment)),
        "the environment-facing answer is not this environment's clone: {answer}"
    );
}

/// The hosting probe's own question, asked of a real channel.
///
/// `Supervisor::probe_agent_hosting` is what decides whether a chat
/// relocates at all, and on an enforcing host it used to answer no. It now
/// runs [`taste_devcontainer::channel::REACH_PROBE`] inside the container —
/// literally these bytes, not a paraphrase — and this asserts the answer is
/// yes here, with both services answering as themselves: MCP returning the
/// ping's id, the auth proxy returning its own 401 to a credential-less
/// request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn the_hosting_probe_says_yes_on_this_host() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;
    let out = podman(&[
        "exec",
        &live._env.container,
        "node",
        "-e",
        taste_devcontainer::channel::REACH_PROBE,
        &live._channel.paths().mcp.display().to_string(),
        &live._channel.paths().auth.display().to_string(),
    ]);
    assert!(
        out.status.success(),
        "the hosting probe would refuse relocation on this host: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("reachable"));
}

/// The auth chain, end to end, from inside a confined container.
///
/// The other half of the same inversion: the agent's forwarder dials the
/// channel's auth endpoint rather than a bind-mounted proxy socket, and the
/// proxy answers down the same `podman exec` pipe. Phase 4 could only test
/// this against a `label=disable` fixture; this one has no such exemption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_relocated_agent_pays_for_its_turns_through_the_ide() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;

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
    let spend = live.proxy.spend(ENV_SLUG);
    assert_eq!(spend.requests, 1, "the proxy did not see the request");
    assert_eq!(spend.input_tokens, 7, "usage counters did not stream past");
}

/// The live-shells batch, end to end: a relocated agent is offered the ACP
/// terminal extension, uses it, and the command really runs **in its
/// environment's container** — with the roster row a console tab is built
/// from appearing for it.
///
/// Asserted at the roster/data layer rather than at a widget: the tab is
/// GTK and this test has none, but everything the tab renders (label,
/// state, output stream, killability) is the roster's, and the console does
/// nothing to it but draw it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_relocated_agent_runs_a_command_through_a_client_served_terminal() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;

    // 1. The capability is advertised — the check a real adapter makes
    //    before it prefers a client terminal over spawning something
    //    itself. Without this the rest is testing an API nobody would call.
    assert_eq!(live.ask("/termcap").await, "termcap yes");

    // 2. A command runs through it, and its output comes back.
    let answer = live.ask("/term echo hello-from-a-served-terminal").await;
    assert!(
        answer.contains("hello-from-a-served-terminal"),
        "the terminal produced no output: {answer}"
    );
    // Whitespace-agnostic: the exit status crosses back through the fake
    // agent's own `json.dumps`, and asserting on its spacing would be a
    // test of Python's formatter.
    assert!(
        answer.replace(' ', "").contains("\"exitCode\":0"),
        "{answer}"
    );

    // 3. It ran INSIDE the container, not on the host. `hostname` in a
    //    podman container is the container id's short form; on this host it
    //    is the machine name. The two are never equal, which is the whole
    //    assertion — a terminal that fell back to the host would pass
    //    every other check in this test.
    let inside = live.ask("/term cat /etc/hostname").await;
    let outside = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    assert!(
        !outside.trim().is_empty() && !inside.contains(outside.trim()),
        "the terminal ran on the HOST, not in the container: {inside} vs {outside}"
    );

    // 4. ...and it is that environment's container specifically: the
    //    workspace the supervisor bound is visible at its host path, which
    //    is the identity relocation is built on.
    let seen = live
        .ask(&format!("/term ls {}/README.md", live.root.display()))
        .await;
    assert!(seen.contains("README.md"), "{seen}");

    // 5. The agent's git policy rides on the terminal, as it does on every
    //    other agent-triggered spawn.
    let policy = live.ask("/term printenv GIT_CONFIG_COUNT").await;
    assert!(
        policy.contains('5'),
        "the git policy did not ride along: {policy}"
    );
}

/// What the console shows while an agent command runs, and what the Kill
/// button does to it — the supervision half of the batch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn a_live_agent_terminal_is_watchable_and_killable_from_the_roster() {
    println!("SELinux: {}", selinux_mode());
    let live = Relocated::start().await;

    // A command that will not end on its own: exactly the case the Kill
    // button exists for.
    let held = live.ask("/termhold sh -c echo\\ started;sleep\\ 600").await;
    let terminal_id = held
        .strip_prefix("termhold ")
        .unwrap_or_else(|| panic!("the terminal was not created: {held}"))
        .to_string();

    // The console's data: one running, killable agent shell in THIS
    // environment, labelled `<env> · <command>`.
    let listed = live.shells.list(Some(&live.environment));
    assert_eq!(listed.len(), 1, "expected one agent terminal: {listed:?}");
    let row = &listed[0];
    assert_eq!(row.kind, taste_core::ShellKind::Agent);
    assert!(row.state.is_running());
    assert!(row.killable, "a running agent terminal must be stoppable");
    assert!(
        row.label()
            .starts_with(&format!("{} · sh ", live.environment)),
        "the tab title must name the environment and the command: {}",
        row.label()
    );

    // The tab's live feed: what already happened, then what comes next.
    let (backlog, updates) = live.shells.watch(row.id).expect("a watchable shell");
    let mut seen = backlog;
    while !seen.contains("started") {
        match tokio::time::timeout(Duration::from_secs(20), updates.recv()).await {
            Ok(Ok(taste_core::ShellUpdate::Output(bytes))) => {
                seen.push_str(&String::from_utf8_lossy(&bytes))
            }
            Ok(Ok(taste_core::ShellUpdate::State(state))) => {
                panic!("it ended before it printed anything: {state:?}")
            }
            other => panic!("no output from the running terminal: {other:?}"),
        }
    }

    // Kill from the USER's side — the console button, not an agent
    // request — and the command dies.
    live.shells.kill(row.id);
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(30), updates.recv()).await {
            Ok(Ok(taste_core::ShellUpdate::State(state))) => break state,
            Ok(Ok(taste_core::ShellUpdate::Output(_))) => continue,
            other => panic!("Kill did not end the terminal: {other:?}"),
        }
    };
    assert!(!ended.is_running(), "{ended:?}");

    // The agent is told, honestly, that its command died — and can still
    // read what it produced. The row stays listed until release, which is
    // what keeps the tab's output on screen.
    let status = live.ask(&format!("/termstatus {terminal_id}")).await;
    assert!(!status.contains("running"), "{status}");
    assert!(
        status.contains("started"),
        "output survives the kill: {status}"
    );

    let row = live
        .shells
        .get(row.id)
        .expect("still listed after the kill");
    assert!(!row.killable, "a dead terminal cannot be killed again");

    // Release is what drops it from the roster.
    assert_eq!(
        live.ask(&format!("/termrelease {terminal_id}")).await,
        "termrelease ok"
    );
    assert!(live.shells.list(Some(&live.environment)).is_empty());
}

/// No container at all keeps the extension unserved — the refusal
/// ARCHITECTURE.md argued, holding exactly where it was argued. An
/// outside-confined agent is offered no terminals and, if it asks anyway,
/// is told why rather than having its command run somewhere.
///
/// Not "safe mode" any more: safe mode is the baseline container, the
/// agent relocates into it, and terminals are served there. What is
/// asserted here is the rung below both — no podman, nowhere to be — and
/// that it resolves to a refusal rather than to the user's host.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman and the devcontainer image on this machine"]
async fn with_nowhere_to_run_no_terminals_are_served() {
    let workspace = tempfile::Builder::new()
        .prefix("taste-safe-mode-ws-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = workspace.path().canonicalize().unwrap();
    init_repo(&root);
    let script = std::env::var("TASTE_TEST_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()
        .unwrap()
        .join("crates/taste-acp/tests/fake_agent.py");
    let spec = taste_acp::AgentSpec::new(
        "fake",
        "Fake Agent",
        "python3",
        &[&script.display().to_string()],
        &[],
    );
    let environment = EnvironmentId::primary();

    // No container, so no relocation and — the point — no TerminalHost.
    let client = AgentClient::spawn_unconfined_for_tests(spec, root);
    loop {
        match next_event(&client).await {
            SessionEvent::Ready { .. } => break,
            SessionEvent::Closed(e) => panic!("the agent never came up: {e:?}"),
            _ => continue,
        }
    }
    let _ = &environment;

    assert_eq!(
        ask(&client, "/termcap").await,
        "termcap no",
        "an agent with no container must not be advertised terminals"
    );
    let refused = ask(&client, "/term echo nope").await;
    assert!(
        refused.contains("never fall back to the user's host"),
        "an unadvertised terminal must be refused with a reason: {refused}"
    );
}
