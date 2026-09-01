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

pub mod authproxy;
pub mod escape;
pub mod registry;
pub mod sandbox;
pub mod session;

pub use registry::{builtin_agents, AgentSpec};
pub use session::{login_command, AgentClient, LoginCommand, PermissionReply, SessionEvent};
