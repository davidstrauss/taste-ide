//! ACP client: the primary agent abstraction of taste-ide.
//!
//! Claude Code, Gemini CLI, GitHub Copilot, and anything else speaking the
//! Agent Client Protocol are interchangeable here. Agents are confined
//! subprocesses talking ACP over stdio, and they run in one of two
//! topologies:
//!
//! - **Beside the files** ([`relocate`]) when their environment's
//!   devcontainer is up: `podman exec` into that container, where native
//!   file tools work because the files are there.
//! - **Outside-confined** ([`sandbox`]) otherwise: a sibling of the IDE
//!   with no workspace at all, reaching the project only through the
//!   client-side `fs/*` methods in [`session`] and the IDE's MCP tools.
//!   This is per-environment safe mode, and it is permanent infrastructure
//!   — the bootstrap path for every environment whose container does not
//!   exist yet.
//!
//! Neither topology outlives a container rebuild, and neither has to: the
//! conversation is carried by the persisted session id and `session/load`,
//! so moving between them is a respawn the chat does not notice.
//!
//! An agent is aimed at exactly one **environment** of the workspace — its
//! checkout, its MCP socket, its home volume, its mode — and
//! [`aim::AgentAim`] is that binding in the shape a spawn takes it. The aim
//! is the address; the topology is not part of it.

pub mod aim;
pub mod authproxy;
pub mod escape;
pub mod registry;
pub mod relocate;
pub mod sandbox;
pub mod session;
pub mod terminal;

pub use aim::AgentAim;
pub use registry::{builtin_agents, AgentSpec};
pub use relocate::{AuthForward, Relocation};
pub use session::{
    login_command, AgentClient, AgentHome, LoginCommand, PermissionReply, SessionEvent,
};
pub use terminal::{TerminalHost, Terminals};
