//! Flatpak packaging as a first-class IDE task.
//!
//! The devcontainer is where work happens; the Flatpak is how work leaves
//! the machine. This crate supervises build → install → launch of the
//! workspace's Flatpak manifest, host-side (flatpak-builder runs as a
//! Flatpak itself), with output streamed to the console and a ring buffer
//! the MCP server can serve.
//!
//! Trust boundary, deliberately: **the AI never triggers this pipeline.**
//! Installing to the host is the "user deploys" line in the trust model.
//! Agents get read-only status and logs over MCP — enough to debug a
//! failing manifest — while the trigger is the user's button.

pub mod manifest;
pub mod packager;

pub use manifest::FlatpakManifest;
pub use packager::{Packager, PackagerState};
