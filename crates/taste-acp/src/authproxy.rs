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
//! is the proxy's second door: a bind-mounted unix socket
//! ([`ensure_unix_transport`]), turned back into a loopback endpoint
//! inside the container by the forwarder in `crate::relocate`.
//!
//! Sign-in deliberately does not go through here. The credential the proxy
//! substitutes is one the *user* provisioned to the IDE — an API key, or a
//! `claude setup-token` token — held in the IDE's own state
//! (`taste_authproxy::credentials`). `login_command` remains the agent's
//! own affair, and the IDE never reads what it writes.

use std::sync::{Arc, OnceLock};

use taste_authproxy::{AuthProxy, Handle, IdeCredentials, UnixTransport, ANTHROPIC_UPSTREAM};

use crate::registry::AgentSpec;

/// Agents whose client honours `ANTHROPIC_BASE_URL`. Gemini and Copilot
/// each have their own auth and their own provider; a proxy for them is
/// separate machinery, and until it exists they keep their credentials.
const PROXIED_AGENTS: &[&str] = &["claude-code"];

/// The IDE's own unix listener, so a relocated agent can reach the proxy
/// from inside a network namespace of the repo's choosing.
///
/// Started once, at IDE startup rather than at first spawn, because the
/// path has to exist *before* the supervisor hands it to podman — a bind
/// mount of a missing path creates a directory there, and then nothing can
/// ever bind the socket. Held for the life of the process: dropping the
/// transport unlinks the socket.
pub fn ensure_unix_transport(path: &std::path::Path) -> Option<&'static UnixTransport> {
    static TRANSPORT: OnceLock<Option<UnixTransport>> = OnceLock::new();
    TRANSPORT
        .get_or_init(|| {
            let handle = handle()?;
            match handle.listen_unix(path) {
                Ok(transport) => {
                    tracing::info!("auth proxy also listening on {}", path.display());
                    Some(transport)
                }
                Err(e) => {
                    // Not fatal, but it does cost relocation: a chat whose
                    // environment is up will stay outside-confined rather
                    // than run beside files it cannot pay for.
                    tracing::error!("auth proxy unix transport unavailable: {e:#}");
                    None
                }
            }
        })
        .as_ref()
}

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

/// The workspace's proxy, started on first use.
///
/// One per process, which today is one per workspace. Credential discovery
/// runs a podman command and is therefore deferred inside the proxy to the
/// first request — spawns are composed on the GTK main thread, which never
/// waits on a process.
/// The running proxy, if it is on and started.
///
/// Public because the placeholder's whole point is attribution: whoever
/// renders per-environment spend reads it from here, and so does the live
/// routing test, whose assertion *is* that these counters moved.
pub fn handle() -> Option<&'static Handle> {
    static PROXY: OnceLock<Option<Handle>> = OnceLock::new();
    PROXY
        .get_or_init(|| {
            if tokio::runtime::Handle::try_current().is_err() {
                tracing::error!("auth proxy needs a tokio runtime context; not starting");
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
    let Some(handle) = handle() else {
        return Vec::new();
    };
    vec![
        ("ANTHROPIC_BASE_URL".to_string(), handle.base_url()),
        (
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            handle.issue_placeholder(environment),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_agents;

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

    #[test]
    fn only_the_anthropic_agent_is_proxied() {
        assert!(PROXIED_AGENTS.contains(&"claude-code"));
        assert!(!PROXIED_AGENTS.contains(&"gemini"));
        assert!(!PROXIED_AGENTS.contains(&"copilot"));
    }
}
