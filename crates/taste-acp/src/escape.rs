//! The escape hatch: direct Agent SDK embedding for capabilities ACP does
//! not model yet.
//!
//! Deliberately thin. It mirrors the ACP session surface so the chat pane
//! renders both identically, and anything that graduates into ACP moves to
//! `session.rs`. New agent integrations must justify using this instead of
//! ACP (see CLAUDE.md).

use anyhow::Result;

/// A minimal, ACP-shaped surface for an embedded (non-ACP) agent.
pub trait EmbeddedAgent: Send {
    /// Human-readable name for the agent picker.
    fn display_name(&self) -> &str;

    /// Queue a prompt; updates arrive on the stream from [`Self::events`].
    fn prompt(&self, text: &str) -> Result<()>;

    /// Stream of plain-text output chunks. Intentionally poorer than ACP's
    /// typed updates: the escape hatch should feel like an escape hatch.
    fn events(&self) -> async_channel::Receiver<String>;

    /// Cancel the in-flight turn, if any.
    fn cancel(&self) -> Result<()>;
}
