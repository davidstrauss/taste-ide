//! The agent registry: a declarative table of known ACP agents plus
//! user-defined entries (any command that speaks ACP on stdio).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub id: String,
    pub display_name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Home-relative paths bound back into the agent's otherwise-empty
    /// sandbox home: its own auth/config/cache, nothing else.
    #[serde(default)]
    pub home_paths: Vec<String>,
    /// How to sign in when ACP cannot: the agent's own interactive CLI,
    /// which the IDE runs in a console tab — the auth terminal — in the
    /// agent's confinement. `None` for an agent whose sign-in comes through
    /// ACP (a `Terminal` auth method, or an `authenticate` that works).
    #[serde(default)]
    pub login: Option<LoginHint>,
}

/// An agent's interactive sign-in, for the chat to open in a terminal.
/// ACP's `authenticate` is a request the agent answers, and an agent whose
/// sign-in is a TUI of its own answers it with a failure and nothing for
/// the user to do — Copilot did, in a chat that offered no terminal method
/// (David, 2026-09-06: "It ought to leverage a new terminal — even if we
/// need a special 'auth terminal' behavior for it").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginHint {
    /// Arguments to the agent's `command` that start its interactive CLI,
    /// in place of the ACP ones.
    pub args: Vec<String>,
    /// What the user does in it, as the chat's status while the tab is up.
    pub instructions: String,
}

impl AgentSpec {
    pub fn new(
        id: &str,
        display_name: &str,
        command: &str,
        args: &[&str],
        home_paths: &[&str],
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            home_paths: home_paths.iter().map(|s| s.to_string()).collect(),
            login: None,
        }
    }

    /// The agent's interactive sign-in (see [`LoginHint`]).
    pub fn with_login(mut self, args: &[&str], instructions: &str) -> Self {
        self.login = Some(LoginHint {
            args: args.iter().map(|s| s.to_string()).collect(),
            instructions: instructions.into(),
        });
        self
    }
}

/// Agents taste-ide knows out of the box. Adapter commands as of the ACP
/// ecosystem's move to the `agentclientprotocol` org (2026).
///
/// Order matters: the first entry is the default agent (Claude Code).
pub fn builtin_agents() -> Vec<AgentSpec> {
    vec![
        AgentSpec::new(
            "claude-code",
            "Claude Code",
            "npx",
            // Version pinned deliberately: the adapter runs next to the
            // agent's auth dir, so "@latest" would be a standing supply-chain
            // exposure. Bump explicitly.
            &["-y", "@agentclientprotocol/claude-agent-acp@0.73.0"],
            // .npm is npx's package cache; .claude/.claude.json hold auth.
            &[".claude", ".claude.json", ".npm"],
        ),
        // The other two run the same way, for the same reason: the agent
        // lives in the environment's container (or the baseline), and
        // neither image carries a `gemini` or a `copilot` binary — nor
        // should it, since the version is the IDE's to pin. A bare command
        // name was "crun: executable file `gemini` not found" the moment
        // anyone picked it. npx fetches the pinned package into the agent
        // home's own cache, exactly as the Claude Code adapter arrives.
        // Neither adapter offers a Terminal auth method, and both CLIs sign
        // in from inside their own TUI, so each says how to open that TUI:
        // the same pinned package, without the ACP flags.
        AgentSpec::new(
            "gemini",
            "Gemini CLI",
            "npx",
            &["-y", "@google/gemini-cli@0.58.0", "--acp"],
            &[".gemini", ".npm"],
        )
        .with_login(
            &["-y", "@google/gemini-cli@0.58.0"],
            "sign in below — /auth picks the method — then /quit; the chat reconnects",
        ),
        AgentSpec::new(
            "copilot",
            "GitHub Copilot",
            "npx",
            &["-y", "@github/copilot@1.0.82", "--acp", "--stdio"],
            &[".copilot", ".npm"],
        )
        .with_login(
            &["-y", "@github/copilot@1.0.82"],
            "type /login below and follow it, then /exit; the chat reconnects",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login hint is the same pinned package as the adapter, or a bump
    /// to one would leave the other behind.
    #[test]
    fn a_login_hint_pins_the_adapters_own_version() {
        for agent in builtin_agents() {
            let Some(login) = agent.login else { continue };
            let package = |args: &[String]| {
                args.iter()
                    .find(|a| a.contains('@') && !a.starts_with('-'))
                    .cloned()
                    .expect("a pinned package")
            };
            assert_eq!(package(&login.args), package(&agent.args), "{}", agent.id);
            assert!(!login.args.iter().any(|a| a == "--acp"), "{}", agent.id);
        }
    }

    #[test]
    fn builtin_ids_are_unique() {
        let agents = builtin_agents();
        let mut ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), agents.len());
    }
}
