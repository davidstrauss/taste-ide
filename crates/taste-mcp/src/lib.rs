//! taste-ide's MCP server: the IDE as a tool surface for agents.
//!
//! This is what lets the chat-pane agent *supervise the IDE's devcontainer*:
//! see the pending-config-changes flag, read the failing build log, and
//! initiate the reload — the core loop the project exists for.
//!
//! Transport: newline-delimited JSON-RPC 2.0 (MCP's stdio framing) served on
//! a unix socket in `$XDG_RUNTIME_DIR`. Agents connect through the stdio
//! bridge (`taste-ide --mcp-bridge <socket>`), which the IDE registers in
//! every agent session's MCP server list. The protocol surface is small and
//! fixed, so it is implemented directly rather than through an SDK.

mod exec;
mod lsp;
mod orchestration;
mod protocol;
mod server;

pub use server::{socket_path, stdio_bridge, McpServer};

/// What the chat pane needs from the tool table to decide whether a
/// permission question can be settled once and for all: whether a call
/// names one of ours, and whether the IDE may keep a standing yes about
/// it. Two functions rather than the module, because the rest of it is
/// this server's own business (see `protocol::effect`, which is the
/// judgement these two are derived from).
pub use protocol::{is_ide_tool, may_stand};
