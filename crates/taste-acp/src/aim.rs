//! Where an agent is aimed.
//!
//! A chat is bound to one environment, and that binding decides four things
//! about the agent it spawns: which checkout is its working directory,
//! which MCP socket it reaches the IDE on (the socket is its identity — see
//! `taste-mcp`), how the bridge command is spelled, and whether it starts in
//! safe mode. [`AgentAim`] is those four, computed in one place from the
//! environment id, so a spawn cannot get half of them right.
//!
//! It is deliberately *not* the confinement. Phase 4 relocates the agent
//! into its environment's container; that changes the topology the process
//! runs in, and none of the four values here. Today every agent runs
//! outside-confined regardless of which environment it is aimed at — only
//! the aim moved.
//!
//! Two consequences worth naming, because both are load-bearing and neither
//! is spelled out anywhere else:
//!
//! - **The stand-in workspace follows the cwd**, and the cwd is now the
//!   environment's checkout, so each environment gets its own stub with its
//!   own `CLAUDE.md`/`AGENTS.md` bound in from its own clone (see
//!   `sandbox::ensure_workspace_stub`). One stub per workspace would have
//!   served one environment's conventions to all of them.
//! - **Write policy follows the cwd too.** `AgentClient` checks agent
//!   writes with `taste_core::policy::write_allowed(cwd, safe_mode, path)`,
//!   so a bound chat's writes are bounded by its own clone and its own
//!   mode — an agent in a down environment may author that environment's
//!   `.devcontainer/`, and nothing in the user's checkout.

use std::path::{Path, PathBuf};

use taste_core::environment::{self, EnvironmentId};

/// The spawn-shaped view of a chat's environment binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAim {
    /// The environment this agent works in.
    pub environment: EnvironmentId,
    /// Its checkout: the main one for the primary, that environment's clone
    /// otherwise. The agent's working directory, the root its file reads and
    /// writes are bounded by, and the tree its stand-in workspace stands in
    /// for.
    pub cwd: PathBuf,
    /// The IDE MCP socket for this environment. Connecting here is how the
    /// agent tells the IDE which environment it is; there is no other
    /// channel and no field on the wire.
    pub mcp_socket: PathBuf,
    /// The stdio bridge, as the IDE's own binary spells it. Replaced by a
    /// `node` bridge when the agent runs inside a container, where the IDE
    /// binary's path means nothing.
    pub mcp_bridge: (String, Vec<String>),
    /// This environment's mode at spawn time. Safe mode is per environment:
    /// an environment with no container of its own is in it, whatever the
    /// others are doing.
    pub safe_mode: bool,
}

impl AgentAim {
    /// Aim an agent at one environment of `workspace_root`.
    ///
    /// `bridge_command` is the IDE binary's own path; `container_running`
    /// is whether *this* environment has a container up, which is the whole
    /// of what decides its mode.
    pub fn new(
        workspace_root: &Path,
        environment: EnvironmentId,
        bridge_command: &str,
        container_running: bool,
    ) -> Self {
        let mcp_socket = environment::env_socket_path(workspace_root, &environment);
        Self {
            cwd: environment::env_repo_root(workspace_root, &environment),
            mcp_bridge: (
                bridge_command.to_string(),
                vec!["--mcp-bridge".into(), mcp_socket.display().to_string()],
            ),
            mcp_socket,
            environment,
            safe_mode: !container_running,
        }
    }

    /// The unbound chat's aim: the primary environment, the main checkout,
    /// the socket agents have always used. An unbound chat is not a chat
    /// with no environment — it is a chat in the primary one.
    pub fn primary(workspace_root: &Path, bridge_command: &str, container_running: bool) -> Self {
        Self::new(
            workspace_root,
            EnvironmentId::primary(),
            bridge_command,
            container_running,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDE: &str = "/usr/bin/taste-ide";

    /// An unbound chat must keep behaving exactly as it did before
    /// environments existed: the main checkout, the primary socket, the
    /// workspace's own mode.
    #[test]
    fn an_unbound_chat_is_aimed_at_the_main_checkout() {
        let root = Path::new("/work/project");
        let aim = AgentAim::primary(root, IDE, true);
        assert!(aim.environment.is_primary());
        assert_eq!(aim.cwd, root);
        assert_eq!(
            aim.mcp_socket,
            environment::env_socket_path(root, &EnvironmentId::primary())
        );
        assert!(!aim.safe_mode, "a running container is container mode");
        assert_eq!(aim.mcp_bridge.0, IDE);
        assert_eq!(
            aim.mcp_bridge.1,
            vec![
                "--mcp-bridge".to_string(),
                aim.mcp_socket.display().to_string()
            ]
        );
    }

    /// A bound chat aims at its environment's clone, its environment's
    /// socket, and its environment's mode — none of which is the primary's.
    #[test]
    fn a_bound_chat_is_aimed_at_its_environments_clone() {
        let root = Path::new("/work/project");
        let review = EnvironmentId::parse("review").unwrap();
        let bound = AgentAim::new(root, review.clone(), IDE, false);
        let unbound = AgentAim::primary(root, IDE, true);

        assert_eq!(bound.environment, review);
        assert_eq!(bound.cwd, environment::env_repo_root(root, &review));
        assert!(bound.cwd.ends_with("review/repo"));
        assert_ne!(bound.cwd, unbound.cwd);
        assert_ne!(bound.mcp_socket, unbound.mcp_socket);
        // The bridge argument carries the socket, so it moves with it —
        // spelling one from the aim and the other from somewhere else is
        // exactly the mistake this type exists to prevent.
        assert!(bound
            .mcp_bridge
            .1
            .contains(&bound.mcp_socket.display().to_string()));

        // Mode is per environment: this one is down while the primary is up.
        assert!(bound.safe_mode);
        assert!(!unbound.safe_mode);
    }

    /// Two environments share nothing a spawn cares about. Notably they get
    /// different working directories, which is what gives each its own
    /// stand-in workspace and its own agent-context files.
    #[test]
    fn no_two_environments_share_a_spawn_target() {
        let root = Path::new("/work/project");
        let a = AgentAim::new(root, EnvironmentId::parse("alpha").unwrap(), IDE, false);
        let b = AgentAim::new(root, EnvironmentId::parse("beta").unwrap(), IDE, false);
        assert_ne!(a.cwd, b.cwd);
        assert_ne!(a.mcp_socket, b.mcp_socket);
        assert_ne!(a.mcp_bridge.1, b.mcp_bridge.1);
    }
}
