//! A loopback HTTP proxy so agent processes never hold a usable Anthropic
//! credential.
//!
//! The IDE runs the proxy, holds the real credential, and hands each agent
//! spawn `ANTHROPIC_BASE_URL` (pointing here) plus `ANTHROPIC_AUTH_TOKEN`
//! (a per-environment placeholder this proxy issued). On the way out the
//! placeholder is stripped and the real `Authorization` header substituted.
//! Responses stream back chunk by chunk; SSE is the normal case, not an
//! exception.
//!
//! # Threat model
//!
//! **What this protects against.** Today the agent's OAuth token sits at
//! `~/.claude/.credentials.json` on the `taste-agent-home` volume, inside
//! the same container as the adapter's native shell. Anything that runs
//! there — a prompt injection talking the agent into `cat`ing a file, a
//! compromised transitive dependency of the pinned adapter, repo-supplied
//! build code once the agent relocates into the devcontainer (ENVIRONMENTS
//! → Relocation) — can read that file and walk off with a credential that
//! is valid everywhere, for as long as it lives. With the proxy, the only
//! secret on that side of the line is a placeholder that is bound to one
//! environment, revocable in isolation, worthless anywhere but this
//! process's loopback port, and gone when the IDE exits.
//!
//! **What this deliberately does not protect against.** It is not a wall
//! between the agent and the model. A placeholder still buys inference on
//! the user's account: an agent that wants to burn tokens, or to ask the
//! model something the user would not have asked, can. Attribution
//! (per-environment counters) and, later, limits are the answer to that —
//! not the header swap. Nor does the proxy separate the agent from the
//! code it writes; per CLAUDE.md those are one principal inside an
//! environment, and mediation is user experience, not a gate. The line
//! being defended here is narrow and real: **a credential that outlives
//! the session and works off this machine must not be readable by code
//! running beside the agent.**
//!
//! Bodies are never logged. The response stream is scanned for the
//! Messages API's `usage` counters — in memory, for attribution — and
//! nothing else is inspected or retained.

pub mod credentials;
pub mod proxy;

pub use credentials::{
    discover, Credential, CredentialFuture, CredentialSource, DiscoveredCredentials,
    FileCredentials, StaticKey,
};
pub use proxy::{AuthProxy, Handle, Spend, ANTHROPIC_UPSTREAM};
