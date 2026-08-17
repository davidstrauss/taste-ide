//! ACP client: the primary agent abstraction of taste-ide.
//!
//! Claude Code, Gemini CLI, GitHub Copilot, and anything else speaking the
//! Agent Client Protocol are interchangeable here. Agents are host-side
//! subprocesses talking ACP over stdio; they survive devcontainer reloads by
//! construction (nothing in a session references the container).

pub mod escape;
pub mod registry;
pub mod sandbox;
pub mod session;

pub use registry::{builtin_agents, AgentSpec};
pub use session::{login_command, AgentClient, LoginCommand, PermissionReply, SessionEvent};
