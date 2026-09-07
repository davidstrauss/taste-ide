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
    /// How the IDE's MCP server reaches an agent whose ACP server takes no
    /// stdio MCP server from `session/new`: the agent's own command-line
    /// flag for an extra MCP config, given the IDE's stdio bridge in the
    /// agent's own JSON shape (`mcp_config_json`). `None` for an agent that
    /// honours `session/new`.
    #[serde(default)]
    pub mcp_config_flag: Option<String>,
}

/// The IDE's MCP stdio bridge as the JSON a CLI's MCP config takes — one
/// server, named as the IDE names it over ACP, every tool allowed. The
/// shape is what `copilot mcp add` writes to `~/.copilot/mcp-config.json`
/// (`"type": "local"` for a stdio server), checked against the real thing
/// on 2026-09-06.
pub fn mcp_config_json(command: &str, args: &[String]) -> String {
    serde_json::json!({
        "mcpServers": {
            "taste-ide": {
                "type": "local",
                "command": command,
                "args": args,
                "tools": ["*"],
            }
        }
    })
    .to_string()
}

/// An agent's interactive sign-in, for the chat to open in a terminal.
/// ACP's `authenticate` is a request the agent answers, and an agent whose
/// sign-in is a TUI of its own answers it with a failure and nothing for
/// the user to do — Copilot did, in a chat that offered no terminal method
/// (David, 2026-09-06: "It ought to leverage a new terminal — even if we
/// need a special 'auth terminal' behavior for it").
///
/// The flow it asks for is the **device-code** one, never a loopback
/// callback. A CLI's default "web flow" opens the browser and then listens
/// on `127.0.0.1:<random>` for the redirect; that listener lives in
/// whatever network namespace the login runs in, and the browser is on
/// the host. The outside-confined login container shares the host's
/// namespace, so it *can* work there — and it still failed in practice
/// (David, 2026-09-06: "Copilot has me try to load this URL … I suspect
/// it doesn't work because of the containerization"), and it can never
/// work from an environment's own container, whose network is its own.
/// A device code is a URL and a code the user types into any browser:
/// nothing has to reach back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginHint {
    /// Arguments to the agent's `command` that start its sign-in, in
    /// place of the ACP ones.
    pub args: Vec<String>,
    /// Environment the sign-in runs with, on top of the agent's own — the
    /// documented switch that keeps a CLI off its browser callback.
    #[serde(default)]
    pub env: Vec<(String, String)>,
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
            mcp_config_flag: None,
        }
    }

    /// The agent takes the IDE's MCP server through this flag rather than
    /// through `session/new` (see [`AgentSpec::mcp_config_flag`]).
    pub fn with_mcp_config_flag(mut self, flag: &str) -> Self {
        self.mcp_config_flag = Some(flag.into());
        self
    }

    /// The agent's interactive sign-in (see [`LoginHint`]).
    pub fn with_login(mut self, args: &[&str], env: &[(&str, &str)], instructions: &str) -> Self {
        self.login = Some(LoginHint {
            args: args.iter().map(|s| s.to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
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
        // in from their own front door, so each says how to open it: the
        // same pinned package without the ACP flags, in its device-code
        // mode (see LoginHint). Gemini's interactive CLI starts with its
        // sign-in when nothing is stored; NO_BROWSER is its documented
        // headless switch (the URL is printed, the code is pasted back).
        AgentSpec::new(
            "gemini",
            "Gemini CLI",
            "npx",
            &["-y", "@google/gemini-cli@0.58.0", "--acp"],
            &[".gemini", ".npm"],
        )
        .with_login(
            &["-y", "@google/gemini-cli@0.58.0"],
            &[("NO_BROWSER", "true")],
            "open the link below, paste the code back, then /quit; the chat reconnects",
        ),
        // `copilot login --device-code`: its own subcommand, which exits when
        // the token is stored — and the tab closes with it.
        AgentSpec::new(
            "copilot",
            "GitHub Copilot",
            "npx",
            &["-y", "@github/copilot@1.0.82", "--acp", "--stdio"],
            &[".copilot", ".npm"],
        )
        .with_login(
            &["-y", "@github/copilot@1.0.82", "login", "--device-code"],
            &[],
            "open the link below and enter the code it shows; the chat reconnects when it finishes",
        )
        // Copilot's ACP server advertises mcpCapabilities {http, sse} and
        // spawns nothing a client lists as stdio in session/new (tested
        // 2026-09-06 against 1.0.83: a probe server never started), so the
        // agent saw GitHub's issue tools and not the IDE's, and found the
        // backlog only by reading this repository (David, 2026-09-06,
        // relaying Copilot: "the available tool metadata advertised only
        // GitHub issue tools, while the relevant backlog API was hidden
        // behind a local Unix socket"). Its documented way in is its own
        // flag, which does start the server.
        .with_mcp_config_flag("--additional-mcp-config"),
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

    /// No hint asks for a browser callback: a loopback listener cannot be
    /// reached from the host when the login runs in a container of its own.
    #[test]
    fn no_login_hint_relies_on_a_loopback_callback() {
        let copilot = builtin_agents()
            .into_iter()
            .find(|a| a.id == "copilot")
            .unwrap();
        assert!(copilot
            .login
            .unwrap()
            .args
            .iter()
            .any(|a| a == "--device-code"));
        let gemini = builtin_agents()
            .into_iter()
            .find(|a| a.id == "gemini")
            .unwrap();
        assert!(gemini
            .login
            .unwrap()
            .env
            .contains(&("NO_BROWSER".to_string(), "true".to_string())));
    }

    /// The JSON Copilot's flag takes is the shape Copilot writes itself.
    #[test]
    fn the_mcp_config_is_one_local_server_with_every_tool() {
        let json = mcp_config_json(
            "node",
            &["-e".into(), "bridge".into(), "/run/ide.sock".into()],
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let server = &value["mcpServers"]["taste-ide"];
        assert_eq!(server["type"], "local");
        assert_eq!(server["command"], "node");
        assert_eq!(server["args"][2], "/run/ide.sock");
        assert_eq!(server["tools"][0], "*");
        assert!(builtin_agents()
            .iter()
            .find(|a| a.id == "copilot")
            .unwrap()
            .mcp_config_flag
            .is_some());
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
