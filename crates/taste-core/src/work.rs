//! One state for a piece of work, derived from the two records that hold
//! it: what is written down about the issue, and what its environment is
//! doing right now.
//!
//! `docs/spikes/issue-is-the-environment.md`: an environment is an issue in
//! progress, so an issue's row has one state, not an issue state beside an
//! environment state. The issue store knows the durable half (open,
//! completed, declined; started by whom); the supervisor and the review
//! board know the runtime half (building, running, failed, stopped;
//! flagged, merged, rejected). Neither crate sees the other, so the
//! derivation lives here, over inputs both can produce, and the panel, the
//! console's state line, `issue_list` and the publish gate all read this
//! one function.
//!
//! The one meaning that must not merge is kept apart: a **rejected**
//! attempt sends the issue back to the queue, and only a **decline** ends
//! an issue without a merge. "Not this attempt" and "not this work" are
//! different sentences.

use crate::review::ReviewState;

/// The durable half — the issue's resolution, as the store records it.
/// Mirrors `taste_git::Resolution` without depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Completed,
    Declined,
}

/// The runtime half — what the issue's environment is doing, if it has
/// one. `Absent` is an issue nobody has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Absent,
    /// Building or starting: on its way.
    Starting,
    /// Up. `waiting` when it is stopped on a person — a permission, a
    /// sign-in, a config that has drifted from the running container.
    Running {
        waiting: bool,
    },
    /// The build or start broke.
    Failed,
    /// The container is off and nothing is wrong with it.
    Off,
}

/// Where a piece of work stands. The order is the order a queue lists
/// them in when it sorts by state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkState {
    /// Written down, not started — or started, then rejected and handed
    /// back to the queue.
    Queued,
    Starting,
    Working,
    /// Stopped on the user.
    Waiting,
    Failed,
    /// Started, and its container is off: paused, or finished but not
    /// yet flagged.
    Stopped,
    /// Flagged: the branch is what the user reviews.
    Review,
    Completed,
    Declined,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkState::Queued => "queued",
            WorkState::Starting => "starting",
            WorkState::Working => "working",
            WorkState::Waiting => "waiting",
            WorkState::Failed => "failed",
            WorkState::Stopped => "stopped",
            WorkState::Review => "review",
            WorkState::Completed => "completed",
            WorkState::Declined => "declined",
        }
    }

    /// Nothing more will happen to it.
    pub fn is_resolved(self) -> bool {
        matches!(self, WorkState::Completed | WorkState::Declined)
    }

    /// An environment exists for it (or did, until it settled).
    pub fn is_started(self) -> bool {
        !matches!(self, WorkState::Queued | WorkState::Declined)
    }
}

/// The derivation. `started` is the store's word — somebody started it —
/// and `runtime` is what exists for it here; a started issue with no
/// environment on this machine is still started (it may be on another).
pub fn work_state(
    outcome: Outcome,
    started: bool,
    runtime: Runtime,
    review: ReviewState,
) -> WorkState {
    match outcome {
        Outcome::Declined => return WorkState::Declined,
        Outcome::Completed => return WorkState::Completed,
        Outcome::Open => {}
    }
    match review {
        // Merged is completed, whatever the store has caught up to.
        ReviewState::Merged => return WorkState::Completed,
        // Rejected is "not this attempt": back to the queue, environment or
        // not. The environment stays until destroyed; the row says queued.
        ReviewState::Rejected => return WorkState::Queued,
        ReviewState::FlaggedForReview => return WorkState::Review,
        ReviewState::Working => {}
    }
    match runtime {
        Runtime::Absent => {
            if started {
                // Started elsewhere, or its clone is gone: it is not free
                // to take, and it is not running here.
                WorkState::Stopped
            } else {
                WorkState::Queued
            }
        }
        Runtime::Starting => WorkState::Starting,
        Runtime::Running { waiting: true } => WorkState::Waiting,
        Runtime::Running { waiting: false } => WorkState::Working,
        Runtime::Failed => WorkState::Failed,
        Runtime::Off => WorkState::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(started: bool, runtime: Runtime, review: ReviewState) -> WorkState {
        work_state(Outcome::Open, started, runtime, review)
    }

    #[test]
    fn the_store_settles_it_first() {
        for runtime in [
            Runtime::Absent,
            Runtime::Running { waiting: true },
            Runtime::Failed,
        ] {
            assert_eq!(
                work_state(Outcome::Declined, true, runtime, ReviewState::Working),
                WorkState::Declined
            );
            assert_eq!(
                work_state(Outcome::Completed, true, runtime, ReviewState::Rejected),
                WorkState::Completed
            );
        }
    }

    #[test]
    fn a_rejected_attempt_returns_to_the_queue_and_a_merge_completes() {
        assert_eq!(
            open(true, Runtime::Off, ReviewState::Rejected),
            WorkState::Queued,
            "not this attempt is not not this work"
        );
        assert_eq!(
            open(true, Runtime::Off, ReviewState::Merged),
            WorkState::Completed
        );
        assert_eq!(
            open(true, Runtime::Off, ReviewState::FlaggedForReview),
            WorkState::Review
        );
    }

    #[test]
    fn the_runtime_says_the_rest() {
        assert_eq!(
            open(false, Runtime::Absent, ReviewState::Working),
            WorkState::Queued
        );
        assert_eq!(
            open(true, Runtime::Absent, ReviewState::Working),
            WorkState::Stopped,
            "started somewhere, not running here"
        );
        assert_eq!(
            open(true, Runtime::Starting, ReviewState::Working),
            WorkState::Starting
        );
        assert_eq!(
            open(
                true,
                Runtime::Running { waiting: false },
                ReviewState::Working
            ),
            WorkState::Working
        );
        assert_eq!(
            open(
                true,
                Runtime::Running { waiting: true },
                ReviewState::Working
            ),
            WorkState::Waiting
        );
        assert_eq!(
            open(true, Runtime::Failed, ReviewState::Working),
            WorkState::Failed
        );
        assert_eq!(
            open(true, Runtime::Off, ReviewState::Working),
            WorkState::Stopped
        );
    }

    #[test]
    fn started_and_resolved_read_off_the_state() {
        assert!(!WorkState::Queued.is_started());
        assert!(WorkState::Stopped.is_started());
        assert!(WorkState::Completed.is_started() && WorkState::Completed.is_resolved());
        assert!(!WorkState::Declined.is_started() && WorkState::Declined.is_resolved());
    }
}
