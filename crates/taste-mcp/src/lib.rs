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

mod protocol;
mod server;

pub use server::{socket_path, stdio_bridge, McpServer};
