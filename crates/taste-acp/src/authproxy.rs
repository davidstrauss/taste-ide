//! Point an agent's Anthropic client at the IDE's auth proxy instead of
//! at the API — on by default, `TASTE_AUTH_PROXY=0` to opt out.
//!
//! Claude Code spawns get `ANTHROPIC_BASE_URL` (the proxy's loopback
//! port) and `ANTHROPIC_AUTH_TOKEN` (a placeholder the proxy issued).
//! Both head the pinned adapter's `PROVIDER_ROUTING_ENV_VARS`, so this is
//! the mechanism it already expects rather than a trick played on it.
//!
//! On by default since both preconditions were met live (2026-08-31): a
//! user-provisioned `claude setup-token` credential, and a real turn
//! whose tokens were counted by the proxy itself — the adapter routes
//! everything through the base URL. A missing credential now fails the
//! chat's first request with an error naming the provisioning step,
//! which is the honest failure: the fix is one command, and silently
//! bypassing the proxy would un-verify everything this module is for.
//!
//! Loopback reaches the agent in all three outside-confined topologies:
//! the agent container runs `--network=host`, bwrap shares the host netns,
//! and the self-hosting direct spawn is in the IDE's own container. A
//! **relocated** agent is the exception — its environment's devcontainer
//! has a network namespace of the repo's choosing — and the answer there
//! is the proxy's second door, `taste_authproxy::Handle::serve_stream`,
//! fed by connections that environment's channel carried out of the
//! container and turned back into a loopback endpoint in there by the
//! forwarder in `crate::relocate`.
//!
//! Sign-in deliberately does not go through here. The credential the proxy
//! substitutes is one the *user* provisioned to the IDE — an API key, or a
//! `claude setup-token` token — held in the IDE's own state
//! (`taste_authproxy::credentials`). `login_command` remains the agent's
//! own affair, and the IDE never reads what it writes.

use std::sync::{Arc, OnceLock};

use taste_authproxy::{AuthProxy, Handle, IdeCredentials, ANTHROPIC_UPSTREAM};

use crate::registry::AgentSpec;

/// Agents whose client honours `ANTHROPIC_BASE_URL`. Gemini and Copilot
/// each have their own auth and their own provider; a proxy for them is
/// separate machinery, and until it exists they keep their credentials.
const PROXIED_AGENTS: &[&str] = &["claude-code"];

/// Whether a relocated spawn of this agent needs the in-container
/// forwarder — i.e. whether [`spawn_env`] would give it a base URL that
/// only means something on the IDE's own loopback.
pub fn proxies(spec: &AgentSpec) -> bool {
    enabled() && PROXIED_AGENTS.contains(&spec.id.as_str()) && handle().is_some()
}

/// On by default since the live round-trip proved the pinned adapter
/// routes everything through the base URL with the real credential
/// injected and every token counted (2026-08-31: EndTurn, 713 in / 18
/// out through the proxy, zero unrecognized credentials).
/// `TASTE_AUTH_PROXY=0` opts out — the escape hatch for debugging the
/// proxy itself, not a supported mode.
fn enabled() -> bool {
    enabled_from(std::env::var("TASTE_AUTH_PROXY").ok().as_deref())
}

fn enabled_from(var: Option<&str>) -> bool {
    var != Some("0")
}

static PROXY: OnceLock<Option<Handle>> = OnceLock::new();

/// Start the workspace's proxy, once, on `rt`.
///
/// One per process, which today is one per workspace. Credential discovery
/// runs a podman command and is therefore deferred inside the proxy to the
/// first request — the caller never waits on a process.
///
/// **The runtime is an argument because it has to be.** `AuthProxy::spawn`
/// needs a tokio runtime context, and almost everyone who wants the proxy
/// asks from the GTK main thread, which does not have one: a panel drawing
/// spend, the hosting probe, a chat composing a spawn. When starting was
/// something [`handle`] did lazily, whichever of those asked first decided
/// the answer for the whole process — a `OnceLock` caches the failure as
/// hard as it caches success, so one console tick at startup meant no proxy
/// until the app was restarted. Taking a `tokio::runtime::Handle` moves
/// that from a thing to remember to a thing to type: a caller without a
/// runtime cannot name one, and every other entry point can only *read*
/// what this started.
///
/// Idempotent, and returns the same handle every later call does.
pub fn start(rt: &tokio::runtime::Handle) -> Option<&'static Handle> {
    PROXY
        .get_or_init(|| {
            // Off means off: nothing binds, and `serves` tells the channel
            // probe there is no door here rather than opening one.
            if !enabled() {
                tracing::info!("auth proxy is off (TASTE_AUTH_PROXY=0)");
                return None;
            }
            let upstream = std::env::var("TASTE_AUTH_PROXY_UPSTREAM")
                .unwrap_or_else(|_| ANTHROPIC_UPSTREAM.to_string());
            let upstream = match upstream.parse() {
                Ok(uri) => uri,
                Err(e) => {
                    tracing::error!("auth proxy upstream {upstream} is not a URI: {e}");
                    return None;
                }
            };
            let _guard = rt.enter();
            match AuthProxy::spawn(upstream, Arc::new(IdeCredentials::new())) {
                Ok(handle) => {
                    tracing::info!("auth proxy listening on {}", handle.addr());
                    Some(handle)
                }
                Err(e) => {
                    // Not fatal: without the proxy the agent uses its own
                    // credential, which is exactly today's behaviour.
                    tracing::error!("auth proxy failed to start: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// The running proxy, if [`start`] brought one up.
///
/// A pure read — it never starts anything, which is what makes it safe to
/// call from the GTK main thread, and what keeps a main-thread caller from
/// deciding the process's answer. `None` here means "not started, or off",
/// and every caller already has an honest thing to do with that.
///
/// Public because the placeholder's whole point is attribution: whoever
/// renders per-environment spend reads it from here, and so does the live
/// routing test, whose assertion *is* that these counters moved.
pub fn handle() -> Option<&'static Handle> {
    PROXY.get().and_then(|proxy| proxy.as_ref())
}

/// Environment to add to one agent spawn. Empty unless the proxy is turned
/// on, running, and fronting a provider this agent speaks.
///
/// `environment` is the id of the environment this chat is bound to. The
/// placeholder is minted against it, which is what makes the spend counters
/// and `revoke` per environment rather than per process.
///
/// The `ANTHROPIC_BASE_URL` here is the IDE's own loopback address, and it
/// is correct for every topology but one: a relocated agent's container may
/// have its own network namespace, where that address means nothing. The
/// in-container forwarder overwrites it with a port it is actually
/// listening on — see `crate::relocate`.
pub fn spawn_env(spec: &AgentSpec, environment: &str) -> Vec<(String, String)> {
    if !enabled() || !PROXIED_AGENTS.contains(&spec.id.as_str()) {
        return Vec::new();
    }
    // A spawn is the one caller that can start the proxy as well as read
    // it: `AgentClient::spawn` runs under the runtime (the app enters it,
    // and a library user is already inside one), so the handle is there for
    // the asking. Nothing is started from a thread without one — that is
    // the whole point of `start` taking the runtime — so a proxy-less
    // process here just spawns the agent without a base URL, exactly as an
    // opted-out one does.
    let handle = match handle() {
        Some(handle) => handle,
        None => match tokio::runtime::Handle::try_current() {
            Ok(rt) => match start(&rt) {
                Some(handle) => handle,
                None => return Vec::new(),
            },
            Err(_) => {
                tracing::warn!("auth proxy was never started; this agent keeps its own credential");
                return Vec::new();
            }
        },
    };
    let mut env = vec![
        ("ANTHROPIC_BASE_URL".to_string(), handle.base_url()),
        (
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            handle.issue_placeholder(environment),
        ),
    ];
    env.extend(fable_picker_row());
    env
}

/// The Fable model, as the top of Claude Code's picker.
const FABLE_MODEL: &str = "claude-fable-5-1[1m]";

/// The one picker row the proxy costs the agent, given back.
///
/// Claude Code's `/model` list is the built-in aliases plus what the server
/// reports for the ACCOUNT — and a subscription's Fable row is reported
/// only to a Claude Code that holds the account's login. Behind the proxy
/// it holds a placeholder, so it never asks, and its picker — which is the
/// adapter's `model` option, which is the pane's slider — stops at Opus for
/// an account that has Fable. The cache of a login-era agent home shows the
/// row that was lost: `claude-fable-5[1m]`, "Fable".
///
/// Claude Code documents a way to add one picker entry from the
/// environment, without replacing the built-in aliases and without
/// validating the id ("any string your API endpoint accepts"). That is the
/// row, spelled the way Claude Code spelled the account's own, with the
/// capabilities Fable has so the effort control stays when it is picked.
/// Whether THIS account can use it is the API's to say, at the first turn,
/// which is also when a wrong guess would have surfaced under a login.
fn fable_picker_row() -> Vec<(String, String)> {
    [
        ("ANTHROPIC_CUSTOM_MODEL_OPTION", FABLE_MODEL),
        ("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME", "Fable"),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "Fable 5.1 · 1M context · most capable, for the hardest and longest-running tasks",
        ),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
            "effort,xhigh_effort,max_effort,thinking,adaptive_thinking,interleaved_thinking",
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_agents;

    /// The row rides on Claude Code's documented picker variables, by their
    /// exact names, and names a Fable id the pane's slider ranks above
    /// Opus (`model_rank` finds the family in the value).
    #[test]
    fn the_fable_row_is_claude_codes_own_custom_option() {
        let row = fable_picker_row();
        let keys: Vec<&str> = row.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ANTHROPIC_CUSTOM_MODEL_OPTION",
                "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
                "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
                "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
            ]
        );
        assert!(row[0].1.contains("fable"));
        assert!(
            row[0].1.ends_with("[1m]"),
            "Fable's window is 1M, and the slider reads it off the hint"
        );
        assert!(row[3].1.split(',').any(|c| c == "effort"));
    }

    #[test]
    fn the_proxy_defaults_on_and_zero_refuses() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
    }

    #[test]
    fn non_anthropic_agents_are_never_proxied() {
        // Whatever the gate says, only the agent whose provider the proxy
        // fronts gets its env rewritten — and only that agent needs the
        // in-container forwarder when it relocates.
        for spec in builtin_agents() {
            if !PROXIED_AGENTS.contains(&spec.id.as_str()) {
                assert!(spawn_env(&spec, "primary").is_empty(), "{}", spec.id);
                assert!(!proxies(&spec), "{}", spec.id);
            }
        }
    }

    /// The regression this module shipped with: a reader on a thread with
    /// no runtime used to *attempt* the start, fail, and cache the failure
    /// in the `OnceLock` — after which the process had no proxy at all. The
    /// only starter now takes a runtime handle, so a reader cannot decide
    /// anything, and `handle()` before a start is simply "not yet".
    ///
    /// This is the whole PROXY static, so it is deliberately the only test
    /// in here that touches it.
    #[test]
    fn a_reader_never_starts_the_proxy() {
        assert!(
            handle().is_none(),
            "nothing has started the proxy, so reading it must not either"
        );
        // Leaked on purpose: the proxy's task outlives this scope the same
        // way the app's does, and a handle to a dropped runtime would be a
        // worse thing to leave in a static than a live one.
        let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
        let started = start(rt.handle()).map(|h| h.addr());
        assert_eq!(
            handle().map(|h| h.addr()),
            started,
            "the reader sees exactly what the starter started"
        );
        if enabled() {
            assert!(
                started.is_some(),
                "the gate is on, so the proxy should be up"
            );
        }
    }

    #[test]
    fn only_the_anthropic_agent_is_proxied() {
        assert!(PROXIED_AGENTS.contains(&"claude-code"));
        assert!(!PROXIED_AGENTS.contains(&"gemini"));
        assert!(!PROXIED_AGENTS.contains(&"copilot"));
    }
}
