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
        }
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
            &["-y", "@agentclientprotocol/claude-agent-acp@0.69.0"],
            // .npm is npx's package cache; .claude/.claude.json hold auth.
            &[".claude", ".claude.json", ".npm"],
        ),
        AgentSpec::new("gemini", "Gemini CLI", "gemini", &["--acp"], &[".gemini"]),
        AgentSpec::new(
            "copilot",
            "GitHub Copilot",
            "copilot",
            &["--acp", "--stdio"],
            &[".copilot"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique() {
        let agents = builtin_agents();
        let mut ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), agents.len());
    }
}
