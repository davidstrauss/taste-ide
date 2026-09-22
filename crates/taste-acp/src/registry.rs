//! The agent registry: a declarative table of known ACP agents plus
//! user-defined entries (any command that speaks ACP on stdio).

use serde::{Deserialize, Serialize};
pub use taste_authproxy::Route;

/// The registry id of the private variant of Claude Code — the same
/// adapter as [`CLAUDE_CODE`], spending on the user's own server.
pub const CLAUDE_CODE_PRIVATE: &str = "claude-code-private";
/// The registry id of Claude Code, the default agent.
pub const CLAUDE_CODE: &str = "claude-code";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub id: String,
    pub display_name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Where the auth proxy sends this agent's requests: the API, or the
    /// user's own private server. Meaningful only for an agent the proxy
    /// fronts (`crate::authproxy`), and the ONE thing that separates
    /// "Claude Code" from "Claude Code (Private)": same command, same
    /// home, a placeholder minted for a different host. Two entries rather
    /// than a switch on one because the choice is made when a chat is
    /// opened and holds for its life, exactly as the choice of agent does
    /// — and because a user mixing the two wants them side by side in the
    /// same list, not a setting to flip between visits.
    #[serde(default)]
    pub upstream: Route,
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
    /// How to get a long-lived token for the IDE to hold, when the agent's
    /// CLI can print one: run in the same console tab as a sign-in, and
    /// the user pastes what it prints into the settings shade's credential
    /// row. `None` for an agent with no such command.
    #[serde(default)]
    pub token: Option<LoginHint>,
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
            upstream: Route::Anthropic,
            home_paths: home_paths.iter().map(|s| s.to_string()).collect(),
            login: None,
            token: None,
            mcp_config_flag: None,
        }
    }

    /// This agent's requests go to the user's private server rather than
    /// the API (see [`AgentSpec::upstream`]).
    pub fn on_private_upstream(mut self) -> Self {
        self.upstream = Route::Private;
        self
    }

    /// Whether this agent spends on the user's own server.
    pub fn is_private(&self) -> bool {
        self.upstream.is_private()
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

    /// See [`AgentSpec::token`].
    pub fn with_token(mut self, args: &[&str], instructions: &str) -> Self {
        self.token = Some(LoginHint {
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
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
    // Version pinned deliberately: the adapter runs next to the agent's
    // auth dir, so "@latest" would be a standing supply-chain exposure.
    // Bump explicitly — once, here, for both Claude Codes.
    const CLAUDE_CODE_ADAPTER: &str = "@agentclientprotocol/claude-agent-acp@0.81.0";
    // .npm is npx's package cache; .claude/.claude.json hold auth.
    const CLAUDE_CODE_HOME: &[&str] = &[".claude", ".claude.json", ".npm"];
    // `claude setup-token` is Claude Code's documented way to mint the
    // year-long token the IDE holds for a subscription. It is Claude Code's
    // OWN package, pinned like the adapter: the adapter runs on the Agent
    // SDK, which bundles no `claude` command, so `npx -p <adapter> claude`
    // was "claude: command not found" in the Sign In tab (2026-09-16).
    const CLAUDE_CODE_CLI: &str = "@anthropic-ai/claude-code@2.1.273";
    const CLAUDE_CODE_TOKEN: &[&str] = &["-y", CLAUDE_CODE_CLI, "setup-token"];
    const CLAUDE_CODE_TOKEN_STEPS: &str =
        "sign in below, copy the token it prints, paste it into the Token row, and Save";
    vec![
        AgentSpec::new(
            CLAUDE_CODE,
            "Claude Code",
            "npx",
            &["-y", CLAUDE_CODE_ADAPTER],
            CLAUDE_CODE_HOME,
        )
        .with_token(CLAUDE_CODE_TOKEN, CLAUDE_CODE_TOKEN_STEPS),
        // The same agent, spending on the user's own Anthropic-compatible
        // server instead of their account (`taste_authproxy::private`).
        // A second entry rather than a row in the model picker: the
        // private model is not a model of Claude Code's, it is a
        // different place for Claude Code to send its requests, and
        // offering it where agents are offered is what lets one
        // environment hold a chat on each — the real thing for the work
        // that matters, the private one for what it is good enough for.
        // The settings shade shows the server's configuration on this
        // variant and the model drop-down on the other (`chat.rs`).
        AgentSpec::new(
            CLAUDE_CODE_PRIVATE,
            "Claude Code (Private)",
            "npx",
            &["-y", CLAUDE_CODE_ADAPTER],
            CLAUDE_CODE_HOME,
        )
        .with_token(CLAUDE_CODE_TOKEN, CLAUDE_CODE_TOKEN_STEPS)
        .on_private_upstream(),
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
    /// The server's schema lists the agents by id from `taste_core`, since
    /// it cannot depend on this crate; the two lists are one list.
    #[test]
    fn the_shipped_agents_are_the_ones_the_server_lists() {
        let mut here: Vec<String> = super::builtin_agents()
            .into_iter()
            .map(|spec| spec.id)
            .collect();
        here.sort_unstable();
        let mut listed: Vec<String> = taste_core::orchestration::AGENT_IDS
            .iter()
            .map(|id| id.to_string())
            .collect();
        listed.sort_unstable();
        assert_eq!(here, listed);
    }

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

    /// The private variant is Claude Code with a different upstream and
    /// nothing else different: a bump to the adapter, or a change to its
    /// home, that reached one and not the other would be two agents
    /// pretending to be one.
    #[test]
    fn the_private_claude_code_is_the_same_adapter_on_another_upstream() {
        let agents = builtin_agents();
        let plain = agents.iter().find(|a| a.id == CLAUDE_CODE).unwrap();
        let private = agents.iter().find(|a| a.id == CLAUDE_CODE_PRIVATE).unwrap();
        assert_eq!(plain.command, private.command);
        assert_eq!(plain.args, private.args);
        assert_eq!(plain.home_paths, private.home_paths);
        assert_eq!(plain.login, private.login);
        assert_eq!(plain.mcp_config_flag, private.mcp_config_flag);
        assert_eq!(plain.upstream, Route::Anthropic);
        assert_eq!(private.upstream, Route::Private);
        assert!(private.is_private() && !plain.is_private());
        assert_eq!(private.display_name, "Claude Code (Private)");
        // The default agent is still the plain one, and the private one is
        // beside it rather than at the end of the list.
        assert_eq!(agents[0].id, CLAUDE_CODE);
        assert_eq!(agents[1].id, CLAUDE_CODE_PRIVATE);
        // Every other agent is on the API by default — it is the value a
        // spec gets when nobody says, and a user-defined entry that says
        // nothing must not land on a server it knows nothing about.
        for agent in &agents[2..] {
            assert_eq!(agent.upstream, Route::Anthropic, "{}", agent.id);
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
