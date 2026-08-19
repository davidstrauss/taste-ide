//! Shared state and events for taste-ide.
//!
//! Everything here is UI-free. The GTK layer (`taste-app`) subscribes to the
//! [`EventBus`] and renders; tokio-side services publish to it. No GTK object
//! ever crosses this boundary.

pub mod app_log;
pub mod conventions;
pub mod event;
pub mod exec;
pub mod ide_state;
pub mod mcp;
pub mod policy;
pub mod search;
pub mod state;
pub mod templates;
pub mod textfile;
pub mod ui_probe;
pub mod watcher;
pub mod workspace;

pub use event::{Event, EventBus};
pub use exec::{CommandSpec, ExecContext};
pub use workspace::Workspace;
