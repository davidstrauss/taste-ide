//! The Tasks section's pieces that are the UI's (tasks.rs in taste-core
//! lists, runs, and keeps them): the section's glyph, and a run's light.

pub use taste_core::tasks::{list, outline, Listing, OutlineEntry, RunState, TaskInfo};

/// The Tasks section's glyph and a task tab's.
pub const TASK_ICON: &str = "system-run-symbolic";

/// A run's light on its row (`env-dot` classes, the fleet's own colours).
pub fn dot(state: RunState) -> &'static str {
    match state {
        RunState::Idle => "unknown",
        RunState::Running => "amber",
        RunState::Succeeded => "green",
        RunState::Failed => "red",
    }
}
