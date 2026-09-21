//! Live proof that a real agent turn is routed through the auth proxy.
//!
//! `#[ignore]`d: this one spends real tokens on a real account, needs the
//! network, and needs a credential on disk. Everything else in the suite
//! runs against `fake_agent.py` with neither.
//!
//! # What it is for
//!
//! ROADMAP (Agent hardening #1) parks the proxy off by default with one
//! stated blocker: *"the switch stays off until live traffic confirms the
//! adapter routes everything through the base URL — a chat that cannot
//! talk is a worse failure than a credential sitting where it already sits
//! today."* This test is that confirmation, and it is deliberately not
//! satisfiable by a mock: the question is what the *pinned adapter* does
//! with `ANTHROPIC_BASE_URL`, which only the real one can answer.
//!
//! # Why the assertion is the spend counters
//!
//! A turn that merely *succeeds* proves nothing — the adapter finding the
//! credentials file itself and going straight to the API succeeds too, and
//! looks identical from the outside. It would be the silent version of the
//! failure this test exists to rule out. The counters only move in the
//! proxy's own request path, so `input_tokens > 0` means the bytes went
//! *through* this process and not around it.
//!
//! # Provisioning a credential for it
//!
//! This test uses a credential **you** give the IDE, and never another
//! program's credential storage. Either intended surface works.
//!
//! An API key, which is the simplest:
//!
//! ```sh
//! export ANTHROPIC_API_KEY=...
//! ```
//!
//! Or a long-lived subscription token. `claude setup-token` prints a
//! one-year OAuth token and saves it nowhere, so put it in a file of the
//! IDE's own format and aim the proxy at it:
//!
//! ```sh
//! claude setup-token                     # copy the printed token
//! cat > /tmp/anthropic.json <<'EOF'
//! {"kind":"oauth_token","token":"PASTE_IT_HERE"}
//! EOF
//! chmod 600 /tmp/anthropic.json
//! export TASTE_ANTHROPIC_CREDENTIALS=/tmp/anthropic.json
//! ```
//!
//! **The aimed path, not a project file, and deliberately.** A credential
//! is the project's now — the proxy reads one keyed by the workspace root
//! and falls back to no machine-wide file (`taste_authproxy::credentials`)
//! — and this test's workspace is a throwaway temporary directory, which
//! by construction has nothing provisioned for it. The two machine-wide
//! surfaces are exactly what a test harness wants, and
//! `TASTE_ANTHROPIC_CREDENTIALS` is also how a credential reaches a
//! container without going into the image.
//!
//! # Running it
//!
//! From the repo's devcontainer, passing whichever credential you
//! provisioned:
//!
//! ```sh
//! podman run --rm --userns=keep-id:uid=1000,gid=1000 \
//!   -v "$PWD:/workspaces/taste-ide:z" \
//!   -v taste-ide-cargo:/home/dev/.cargo \
//!   -e ANTHROPIC_API_KEY \
//!   taste-ide-devcontainer \
//!   cargo test -p taste-acp --test live_proxy -- --ignored --nocapture
//! ```
//!
//! Inside that container podman is absent, so `AgentClient::spawn` takes
//! its self-hosting direct-spawn path — the same seam
//! (`authproxy::spawn_env`) injects the proxy either way, which is what
//! makes this a fair test of the real spawn path rather than a rigged one.

use std::time::Duration;

use taste_acp::{builtin_agents, AgentClient, SessionEvent};

/// Long enough for `npx` to fetch the pinned adapter on a cold cache and
/// for a model turn to complete; short enough to fail rather than hang.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn describe(event: &SessionEvent) -> String {
    match event {
        SessionEvent::Ready { .. } => "Ready".into(),
        SessionEvent::AuthRequired { .. } => "AuthRequired".into(),
        SessionEvent::Closed(e) => format!("Closed({e:?})"),
        SessionEvent::Update(_) => "Update".into(),
        SessionEvent::TurnEnded { .. } => "TurnEnded".into(),
        SessionEvent::Permission { .. } => "Permission".into(),
        SessionEvent::ModeChangeFailed { message, .. } => format!("ModeChangeFailed({message})"),
        SessionEvent::CommandFailed { message } => format!("CommandFailed({message})"),
        SessionEvent::PromptFailed { message } => format!("PromptFailed({message})"),
    }
}

async fn next_event(client: &AgentClient, within: Duration) -> SessionEvent {
    tokio::time::timeout(within, client.events.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("event channel closed unexpectedly")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends real tokens, needs network and a credentials file"]
async fn live_proxy_roundtrip() {
    // Force the gate on for this process before anything reads it. The
    // proxy handle is a OnceLock, so this must precede the first spawn.
    std::env::set_var("TASTE_AUTH_PROXY", "1");

    let spec = builtin_agents()
        .into_iter()
        .find(|s| s.id == "claude-code")
        .expect("the claude-code spec is built in");

    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();

    // Fail early and legibly rather than deep inside a proxied request:
    // without a provisioned credential there is nothing to verify. Asked
    // of this workspace, because that is the only scope there is — and a
    // throwaway one, so what has to be set is one of the two machine-wide
    // surfaces (see this file's header).
    if taste_authproxy::discover(&root).await.is_err() {
        panic!(
            "no credential this test can use — set ANTHROPIC_API_KEY, or point \
             TASTE_ANTHROPIC_CREDENTIALS at a file of the IDE's format (see this file's \
             header for `claude setup-token`). This test's workspace is a temporary \
             directory, so the project file at {} does not exist and nothing falls back \
             to a machine-wide one.",
            taste_authproxy::credential_path(&root).display(),
        );
    }

    let env = taste_core::environment::EnvironmentId::primary();
    let home = taste_acp::AgentHome {
        environment: env.to_string(),
        volume: taste_core::environment::env_home_volume(&root, &env),
    };
    // Outside-confined deliberately: relocation needs an environment with a
    // container up, and what this test verifies is the credential path, not
    // the topology.
    // No terminals either: they are served only where the agent runs
    // beside the files, which outside-confined is exactly what it does not.
    let client = AgentClient::spawn(
        spec,
        root.clone(),
        taste_core::files::Files::Local,
        root,
        None,
        None,
        home,
        None,
        None,
        false,
        None,
        None,
    )
    .expect("spawning the claude-code agent");

    // The proxy must actually be running, or the rest of this test is
    // measuring nothing. It starts lazily on the first spawn.
    let handle = taste_acp::authproxy::handle()
        .expect("the auth proxy should be running once the gate is on");
    let env = env.to_string();
    let env = env.as_str();
    let before = handle.spend(env);
    assert_eq!(
        before.input_tokens, 0,
        "a fresh proxy should not have spent anything yet"
    );

    // Come up. Anything other than Ready is a failure worth naming, and
    // AuthRequired specifically means the credential never reached us.
    loop {
        let event = next_event(&client, READY_TIMEOUT).await;
        match event {
            SessionEvent::Ready { .. } => break,
            SessionEvent::Update(_) => continue,
            other => panic!("expected Ready, got {}", describe(&other)),
        }
    }

    client.prompt("Reply with exactly: ok").unwrap();

    let reason = loop {
        let event = next_event(&client, TURN_TIMEOUT).await;
        match event {
            SessionEvent::TurnEnded { reason, .. } => break reason,
            // Permission prompts would block a turn forever; this prompt
            // should not trigger tool use, so treat one as a failure
            // rather than auto-approving anything.
            SessionEvent::Permission { .. } => panic!("unexpected permission request"),
            SessionEvent::Update(u) => {
                if std::env::var_os("TASTE_LIVE_DEBUG").is_some() {
                    println!("update: {u:?}");
                }
                continue;
            }
            other => panic!("turn did not complete: {}", describe(&other)),
        }
    };

    let after = handle.spend(env);
    println!(
        "live proxy roundtrip: stop reason {reason:?}; \
         requests {} -> {}, response bytes {} -> {}, input tokens {} -> {}, output tokens {} -> {}",
        before.requests,
        after.requests,
        before.response_bytes,
        after.response_bytes,
        before.input_tokens,
        after.input_tokens,
        before.output_tokens,
        after.output_tokens,
    );

    // What the account said about itself while that turn went past.
    //
    // Printed rather than asserted, deliberately. The documented
    // rate-limit headers describe the per-minute API limits; whether a
    // *subscription* credential also carries its five-hour and weekly
    // windows is not documented anywhere, so this is the only honest way
    // to find out — and an assertion would be a claim about an interface
    // nobody promised. `other` is the important line: it lists the
    // rate-limit headers this proxy received and does not model, which is
    // where a family we have not seen would first show up.
    let quota = handle.quota();
    println!(
        "live proxy roundtrip: quota observed = {}",
        !quota.is_empty()
    );
    if let Some(age) = quota.age(std::time::SystemTime::now()) {
        println!("  observed {age:?} ago, during {:?}", quota.observed_for);
    }
    println!("  session window: {:?}", quota.session);
    println!("  weekly window:  {:?}", quota.weekly);
    println!("  requests:       {:?}", quota.requests);
    println!("  tokens:         {:?}", quota.tokens);
    println!("  input tokens:   {:?}", quota.input_tokens);
    println!("  output tokens:  {:?}", quota.output_tokens);
    println!("  exhausted:      {:?}", quota.exhausted);
    println!("  unmodelled rate-limit headers seen:");
    for (name, value) in &quota.other {
        println!("    {name}: {value}");
    }
    if let Some(headline) = quota.headline(std::time::SystemTime::now()) {
        println!(
            "  headline: {} at {:.0}%",
            headline.meter.label(),
            headline.used * 100.0
        );
    }

    // The actual proof: the traffic went through this process.
    assert!(
        after.requests > before.requests,
        "the proxy forwarded no requests, so the adapter went around it"
    );
    assert!(
        after.input_tokens > before.input_tokens,
        "no input tokens observed: the turn did not run through the proxy"
    );
    assert!(
        after.output_tokens > before.output_tokens,
        "no output tokens observed: the turn did not run through the proxy"
    );
    // Unauthenticated probes are expected (the CLI sends `HEAD /api/hello`
    // to its base URL, observed live) and are refused without cost. What
    // must never happen is a *presented* credential the proxy did not
    // issue — that means the placeholder plumbing is broken.
    println!(
        "live proxy roundtrip: {} unauthenticated probe(s) refused",
        handle.unauthenticated()
    );
    assert_eq!(
        handle.unrecognized(),
        0,
        "the proxy refused a credential it never issued: placeholder plumbing is broken"
    );
}
