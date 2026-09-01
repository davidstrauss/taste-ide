//! ACP client: the primary agent abstraction of taste-ide.
//!
//! Claude Code, Gemini CLI, GitHub Copilot, and anything else speaking the
//! Agent Client Protocol are interchangeable here. Agents are confined
//! subprocesses talking ACP over stdio, siblings of the IDE rather than
//! children of the devcontainer; they survive devcontainer reloads by
//! construction (nothing in a session references the container). They hold
//! no workspace of their own: file contents travel over the client-side
//! `fs/*` methods in [`session`], and everything else through the IDE's MCP
//! tools.
//!
//! An agent is aimed at exactly one **environment** of the workspace — its
//! checkout, its MCP socket, its mode — and [`aim::AgentAim`] is that
//! binding in the shape a spawn takes it.

pub mod aim;
pub mod authproxy;
pub mod escape;
pub mod registry;
pub mod sandbox;
pub mod session;

pub use aim::AgentAim;
pub use registry::{builtin_agents, AgentSpec};
pub use session::{login_command, AgentClient, LoginCommand, PermissionReply, SessionEvent};
