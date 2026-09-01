//! Point an agent's Anthropic client at the IDE's auth proxy instead of
//! at the API — opt in, off by default.
//!
//! Set `TASTE_AUTH_PROXY=1` in the IDE's own environment and Claude Code
//! spawns get `ANTHROPIC_BASE_URL` (the proxy's loopback port) and
//! `ANTHROPIC_AUTH_TOKEN` (a placeholder the proxy issued). Both head the
//! pinned adapter's `PROVIDER_ROUTING_ENV_VARS`, so this is the mechanism
//! it already expects rather than a trick played on it.
//!
//! Off by default because the payoff only arrives once a live turn
//! confirms the adapter routes everything through the base URL *and* the
//! user has provisioned a credential to the IDE. Until both hold, the
//! switch exists to find that out without risking a chat that cannot talk:
//! with no provisioned credential, turning it on turns a working chat into
//! a broken one.
//!
//! Loopback reaches the agent in all three confinements: the agent
//! container runs `--network=host`, bwrap shares the host netns, and the
//! self-hosting direct spawn is in the IDE's own container. Phase 4
//! relocates the agent into devcontainers that may have their own netns —
//! a bind-mounted unix socket is the answer there, and the proxy's
//! connection handler is already transport-generic.
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

/// Phase 1 has one environment per workspace, so one id. Phase 2 replaces
/// this with the id of the environment the chat is bound to, which is what
/// makes the counters and `revoke` mean anything.
pub const PRIMARY_ENV: &str = "primary";

fn enabled() -> bool {
    matches!(std::env::var("TASTE_AUTH_PROXY").as_deref(), Ok("1"))
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
pub fn spawn_env(spec: &AgentSpec) -> Vec<(String, String)> {
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
            handle.issue_placeholder(PRIMARY_ENV),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_agents;

    #[test]
    fn the_proxy_is_off_unless_asked_for() {
        // The default path must not start a listener or touch a credential.
        assert!(!enabled() || std::env::var("TASTE_AUTH_PROXY").is_ok());
        for spec in builtin_agents() {
            if std::env::var("TASTE_AUTH_PROXY").is_err() {
                assert!(spawn_env(&spec).is_empty(), "{}", spec.id);
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
