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
//! **Which upstream a spawn ends up on is not decided here.** The
//! placeholder minted below is bound to an environment, and the proxy
//! routes on it per request ([`set_route`]), so a chat can be moved
//! between the API and the user's own private model without respawning
//! anything. What the spawn carries is identical either way — that is the
//! point, and it is why choosing the private model costs no `session/load`
//! and no lost conversation.
//!
//! Sign-in deliberately does not go through here. The credential the proxy
//! substitutes is one the *user* provisioned to the IDE — an API key, or a
//! `claude setup-token` token — held in the IDE's own state
//! (`taste_authproxy::credentials`). `login_command` remains the agent's
//! own affair, and the IDE never reads what it writes.

use std::sync::{Arc, OnceLock};

use taste_authproxy::{AuthProxy, Handle, IdeCredentials, ANTHROPIC_UPSTREAM};

/// The private upstream's vocabulary, re-exported for the app: `taste-app`
/// reaches the proxy through this module and depends on no other part of
/// `taste-authproxy`.
pub use taste_authproxy::{PrivateFacts, Route, PRIVATE_MODEL_VALUE};

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

/// Start the workspace's proxy, once, on `rt`, for the project at
/// `workspace_root`.
///
/// One per process, which today is one per workspace. Credential discovery
/// runs a podman command and is therefore deferred inside the proxy to the
/// first request — the caller never waits on a process.
///
/// **The root is an argument because the credential is the project's.**
/// Everything this proxy reads off disk — the Anthropic credential, the
/// private model, the account's model listing — is keyed by it
/// (`taste_authproxy::credentials`), and nothing falls back to a
/// machine-wide file, so authenticating one project never authenticates
/// another. One process per folder means the root is known once, here,
/// and nothing downstream carries it.
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
pub fn start(
    rt: &tokio::runtime::Handle,
    workspace_root: &std::path::Path,
) -> Option<&'static Handle> {
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
            match AuthProxy::spawn(upstream, Arc::new(IdeCredentials::new(workspace_root))) {
                Ok(handle) => {
                    tracing::info!("auth proxy listening on {}", handle.addr());
                    // What the account can run, for the picker row below
                    // (`top_tier_picker_row`): the cache answers the first
                    // spawn, the API the ones after — and both land off
                    // this thread.
                    handle
                        .refresh_models(Some(taste_authproxy::models::cache_path(workspace_root)));
                    // The credential itself, read once now so the chat
                    // header can name the identity in force before any
                    // turn has happened. Off this thread, like the rest.
                    handle.warm_credentials();
                    // ...and the other upstream, if the user provisioned
                    // one. Installed here rather than in `AuthProxy::spawn`
                    // because "none" is the ordinary case and a proxy with
                    // one upstream is what most workspaces want. The warm
                    // read is so the first chat pane to build its model
                    // drop-down has a row to put in it; every read after
                    // that happens on the request path, where the file is
                    // re-read whenever it changes.
                    handle.set_private_upstream(
                        taste_authproxy::private::discover(workspace_root).map(std::sync::Arc::new),
                    );
                    handle.warm_private_upstream();
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

/// The private model this IDE was provisioned with, if it was.
///
/// A pure read, safe from the GTK thread, and free of the server's key —
/// what comes back is a label, an endpoint, and a context window, which is
/// everything a picker row and a header mark need. `None` means no private
/// model, and so no such row anywhere in the app.
pub fn private_model() -> Option<PrivateFacts> {
    handle()?.private_model()
}

/// The upstream a chosen model implies.
///
/// The one place the IDE's `private` value becomes a route, so the chat
/// pane, an orchestrator's `issue_start`, and a restored chat cannot
/// disagree about what a remembered model means. Every other value —
/// including `None`, the agent's own default — is the API: the private
/// server is reached because it was named, never because nothing was.
pub fn route_for_model(model: Option<&str>) -> Route {
    match model {
        Some(PRIVATE_MODEL_VALUE) => Route::Private,
        _ => Route::Anthropic,
    }
}

/// Point one environment's chat at one upstream or the other, from its
/// next request on.
///
/// Called from the chat pane the moment the user picks a model, and from
/// the pane's session-ready path when a remembered choice is re-applied.
/// It writes one map entry: no respawn, no ACP traffic, and nothing said
/// to the agent, whose `model` option is left exactly where it was — see
/// `taste_authproxy::private` for why the route is the only thing that
/// changes. A no-op when the proxy is off, which is the same rung at which
/// there is no private model to be on.
pub fn set_route(environment: &str, route: Route) {
    if let Some(handle) = handle() {
        handle.set_route(environment, route);
    }
}

/// Where this environment's chat is pointed today.
pub fn route(environment: &str) -> Route {
    handle()
        .map(|handle| handle.route(environment))
        .unwrap_or_default()
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
///
/// `workspace_root` is here only for the case this call is the one that
/// starts the proxy (below): whose credential it would hold is not a
/// question a spawn may leave open. A proxy already started ignores it,
/// because the first start decides the process's answer.
pub fn spawn_env(
    spec: &AgentSpec,
    environment: &str,
    workspace_root: &std::path::Path,
) -> Vec<(String, String)> {
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
            Ok(rt) => match start(&rt, workspace_root) {
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
    env.extend(top_tier_picker_row(handle.top_tier_model().as_ref()));
    env
}

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
/// row. WHICH model is the proxy's to know, not the IDE's to remember: it
/// holds the credential, and the documented Models API lists what that
/// credential can run — so the row is the newest model above Opus the
/// account has (`taste_authproxy::models::top_tier`), spelled as the API
/// spells it, with the `[1m]` hint Claude Code uses when the window is a
/// million tokens. No listing yet (first launch, before the cache exists)
/// or nothing above Opus in it means no row — Claude Code's own picker is
/// complete for that account. Only the capability list is the IDE's word:
/// the whole tier takes effort levels and adaptive thinking.
fn top_tier_picker_row(model: Option<&taste_authproxy::ModelListing>) -> Vec<(String, String)> {
    let Some(model) = model else {
        return Vec::new();
    };
    let value = if model.has_1m_context() {
        format!("{}[1m]", model.id)
    } else {
        model.id.clone()
    };
    // The slider's tick reads "Fable" beside "Opus", not "Claude Fable 5.1";
    // the long name goes where the picker shows descriptions.
    let name = ["Mythos", "Fable"]
        .into_iter()
        .find(|family| {
            model.display_name.contains(family) || model.id.contains(&family.to_lowercase())
        })
        .map(str::to_string)
        .unwrap_or_else(|| model.display_name.clone());
    let display = if model.display_name.is_empty() {
        model.id.clone()
    } else {
        model.display_name.clone()
    };
    let description = if model.has_1m_context() {
        format!("{display} · 1M context · the most capable model this account can run")
    } else {
        format!("{display} · the most capable model this account can run")
    };
    vec![
        ("ANTHROPIC_CUSTOM_MODEL_OPTION".to_string(), value),
        ("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME".to_string(), name),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION".to_string(),
            description,
        ),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES".to_string(),
            "effort,xhigh_effort,max_effort,thinking,adaptive_thinking,interleaved_thinking"
                .to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_agents;

    /// The row rides on Claude Code's documented picker variables, by their
    /// exact names, and spells the model the API's way plus the hint the
    /// pane's slider reads the window off (`model_rank` finds the family in
    /// the value, so a full id ranks above Opus like the alias would).
    #[test]
    fn the_top_tier_row_is_claude_codes_own_custom_option() {
        let fable = taste_authproxy::ModelListing {
            id: "claude-fable-5-1".into(),
            display_name: "Claude Fable 5.1".into(),
            created_at: "2026-08-25T00:00:00Z".into(),
            max_input_tokens: Some(1_000_000),
        };
        let row = top_tier_picker_row(Some(&fable));
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
        assert_eq!(row[0].1, "claude-fable-5-1[1m]");
        // A tick-sized name beside "Opus"; the long one in the description.
        assert_eq!(row[1].1, "Fable");
        assert!(row[2].1.starts_with("Claude Fable 5.1 · 1M context"));
        assert!(row[3].1.split(',').any(|c| c == "effort"));
        // Nothing known, nothing added: Claude Code's own picker stands.
        assert!(top_tier_picker_row(None).is_empty());
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
        let root = std::path::Path::new("/work/project");
        for spec in builtin_agents() {
            if !PROXIED_AGENTS.contains(&spec.id.as_str()) {
                assert!(spawn_env(&spec, "primary", root).is_empty(), "{}", spec.id);
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
        let started = start(rt.handle(), std::path::Path::new("/work/project")).map(|h| h.addr());
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
