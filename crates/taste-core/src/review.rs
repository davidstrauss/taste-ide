//! The review lifecycle: where an environment is in the arc from working
//! to gone.
//!
//! An environment is one branch, one agent session, one devcontainer and
//! one place to review changes against a merge target. That makes the
//! *environment* the unit of review — not a branch, not a diff, not an
//! inbox row — and this module is that lifecycle as a fact the whole IDE
//! reads from one place:
//!
//! ```text
//! Working ──flag──▶ FlaggedForReview ──▶ Merged ──┐
//!    ▲                    │                       ├──▶ destroyable
//!    └──── unflag ────────┘             Rejected ──┘
//! ```
//!
//! Three things about the shape are deliberate.
//!
//! - **Flagging is explicit, not implied by publishing.** An agent
//!   checkpoints its branch far more often than it finishes, and flagging
//!   stops the container — so a publish that always flagged would stop
//!   environments mid-thought. `publish` moves the branch; `publish` with
//!   `ready` says "I am done, review me", and only that one flags.
//! - **Merged is not a latch.** It records what the user decided, but
//!   whether the work is actually *in* the target is
//!   `taste_git::Mergedness`, asked fresh. A state file that claimed
//!   "merged" over a branch a reset had since removed would be the one
//!   lie the whole design exists to avoid.
//! - **Settled means destroyable.** Once a person has merged or rejected an
//!   environment there is nothing left to warn them about on destroy, which
//!   is the difference between a fleet that accumulates and one that
//!   drains.
//!
//! The state is persisted with the rest of the environment's entry in
//! [`crate::state::WorkspaceState`], so a flag survives a restart — an IDE
//! that forgot which environments were waiting on the user would quietly
//! restart every container it had stopped to save them resources.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentId;
use crate::event::{Event, EventBus};

/// Where an environment stands in the review arc.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewState {
    /// The ordinary state: an agent is working, and nothing is waiting on
    /// the user.
    #[default]
    Working,
    /// The environment says it is done. Its container is stopped, its
    /// branch is what the user reviews, and the fleet row says so.
    FlaggedForReview,
    /// The user merged it. Nothing to lose by destroying it.
    Merged,
    /// The user does not want it. Also nothing to lose — the branch is
    /// still there to look at until they delete it.
    Rejected,
}

impl ReviewState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewState::Working => "working",
            ReviewState::FlaggedForReview => "flagged-for-review",
            ReviewState::Merged => "merged",
            ReviewState::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "working" => Some(ReviewState::Working),
            "flagged-for-review" | "flagged" | "ready" => Some(ReviewState::FlaggedForReview),
            "merged" => Some(ReviewState::Merged),
            "rejected" => Some(ReviewState::Rejected),
            _ => None,
        }
    }

    /// Waiting on the user.
    pub fn flagged(self) -> bool {
        self == ReviewState::FlaggedForReview
    }

    /// The user has decided. There is nothing left to warn about on
    /// destroy — which is the whole point of tracking this.
    pub fn settled(self) -> bool {
        matches!(self, ReviewState::Merged | ReviewState::Rejected)
    }

    /// Whether an environment in this state should have its container
    /// stopped.
    ///
    /// Flagged and settled environments are both waiting on nothing: the
    /// agent has said its piece, or the user has ruled. Keeping a container
    /// up for either is a share of the machine spent on a world nobody is
    /// talking to.
    pub fn should_be_stopped(self) -> bool {
        self != ReviewState::Working
    }

    /// One line for a fleet row or a tool result.
    pub fn detail(self) -> &'static str {
        match self {
            ReviewState::Working => "",
            ReviewState::FlaggedForReview => "waiting for your review",
            ReviewState::Merged => "merged — safe to destroy",
            ReviewState::Rejected => "rejected — safe to destroy",
        }
    }
}

/// An environment's review state and when it last changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewRecord {
    pub state: ReviewState,
    /// RFC 3339, for "flagged 20 minutes ago". `None` for an environment
    /// that has never left `Working`.
    pub since: Option<String>,
}

/// The workspace's review board: which environments are waiting on the
/// user, readable from anywhere.
///
/// A cloneable handle on the [`crate::Workspace`], like
/// [`crate::ide_state::IdeState`], because the readers and the writers are
/// in different crates and on different threads: the MCP server sets a flag
/// from tokio when an agent says it is ready, the console reads it while
/// drawing a row, and the supervisor asks whether a container should come
/// down. One handle means they cannot disagree.
///
/// Writes persist immediately (read-modify-write of the workspace state
/// file, the same way every other writer of that file works) so a flag
/// survives a crash, not merely a clean quit. They block on file IO: call
/// [`ReviewBoard::set`] off the GTK main thread.
#[derive(Clone)]
pub struct ReviewBoard {
    inner: Arc<Mutex<Board>>,
}

struct Board {
    root: PathBuf,
    /// State-file directory override. `None` — always, outside tests — is
    /// the XDG location. Tests point it at a tempdir so they neither read
    /// the developer's real state nor race each other over `XDG_STATE_HOME`.
    base: Option<PathBuf>,
    /// Filled from the state file on first use. `None` until then, so a
    /// board that is never asked never touches the disk.
    records: Option<BTreeMap<EnvironmentId, ReviewRecord>>,
    events: Option<EventBus>,
}

impl ReviewBoard {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Board {
                root: root.into(),
                base: None,
                records: None,
                events: None,
            })),
        }
    }

    /// A board whose state file lives under `base` rather than the XDG
    /// state directory.
    #[doc(hidden)]
    pub fn with_base_for_tests(base: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        let board = Self::new(root);
        if let Ok(mut inner) = board.inner.lock() {
            inner.base = Some(base.into());
        }
        board
    }

    /// Publish [`Event::EnvironmentReviewChanged`] on every change, so the
    /// fleet view redraws without polling.
    pub fn attach_events(&self, events: EventBus) {
        if let Ok(mut board) = self.inner.lock() {
            board.events = Some(events);
        }
    }

    /// Where an environment stands. Unknown environments are `Working`:
    /// absence of a flag is not a state of its own.
    pub fn state(&self, env: &EnvironmentId) -> ReviewState {
        self.record(env).map(|r| r.state).unwrap_or_default()
    }

    pub fn record(&self, env: &EnvironmentId) -> Option<ReviewRecord> {
        let mut board = self.inner.lock().ok()?;
        board.hydrate();
        board.records.as_ref()?.get(env).cloned()
    }

    /// Every environment waiting on the user, in id order.
    pub fn flagged(&self) -> Vec<(EnvironmentId, ReviewRecord)> {
        let Ok(mut board) = self.inner.lock() else {
            return Vec::new();
        };
        board.hydrate();
        board
            .records
            .as_ref()
            .map(|records| {
                records
                    .iter()
                    .filter(|(_, record)| record.state.flagged())
                    .map(|(env, record)| (env.clone(), record.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Move an environment to `state`, persist it, and announce it.
    ///
    /// Returns whether anything changed — the caller uses that to decide
    /// whether to act (stopping a container it has already stopped is
    /// noise, not idempotence).
    ///
    /// The primary environment is refused: it is the merge target, and an
    /// environment cannot be reviewed against itself.
    pub fn set(&self, env: &EnvironmentId, state: ReviewState) -> Result<bool> {
        if env.is_primary() {
            anyhow::bail!(
                "the primary environment is the merge target, not something submitted to it — \
                 there is nothing to review it against"
            );
        }
        let mut board = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("the review board is poisoned"))?;
        board.hydrate();
        if board.state_of(env) == state {
            return Ok(false);
        }
        let record = ReviewRecord {
            state,
            since: Some(crate::state::now_rfc3339()),
        };
        board
            .records
            .get_or_insert_with(BTreeMap::new)
            .insert(env.clone(), record.clone());
        board.persist(env, Some(&record))?;
        board.announce(env);
        Ok(true)
    }

    /// Forget an environment — it was destroyed. Leaves the state file's
    /// entry alone, because destroying already removes it wholesale
    /// (`WorkspaceState::forget_environment`); this only drops the cache so
    /// a slug that comes round again does not inherit a stale flag.
    pub fn forget(&self, env: &EnvironmentId) {
        if let Ok(mut board) = self.inner.lock() {
            if let Some(records) = board.records.as_mut() {
                records.remove(env);
            }
        }
    }
}

impl Board {
    fn hydrate(&mut self) {
        if self.records.is_some() {
            return;
        }
        let state = self.load();
        self.records = Some(
            state
                .environments
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        ReviewRecord {
                            state: entry.review,
                            since: entry.review_since.clone(),
                        },
                    )
                })
                .collect(),
        );
    }

    fn state_of(&self, env: &EnvironmentId) -> ReviewState {
        self.records
            .as_ref()
            .and_then(|records| records.get(env))
            .map(|record| record.state)
            .unwrap_or_default()
    }

    /// Read-modify-write of the one field this owns, exactly as every other
    /// writer of the workspace state file does it.
    fn persist(&self, env: &EnvironmentId, record: Option<&ReviewRecord>) -> Result<()> {
        let mut state = self.load();
        state.set_review(env, record.map(|r| r.state).unwrap_or_default());
        if let Some(record) = record {
            state.set_review_since(env, record.since.clone());
        }
        match &self.base {
            Some(base) => crate::state::save_to(base, &self.root, &state),
            None => crate::state::save(&self.root, &state),
        }
    }

    fn load(&self) -> crate::state::WorkspaceState {
        match &self.base {
            Some(base) => crate::state::load_from(base, &self.root),
            None => crate::state::load(&self.root),
        }
    }

    fn announce(&self, env: &EnvironmentId) {
        if let Some(events) = &self.events {
            events.publish(Event::EnvironmentReviewChanged { env: env.clone() });
        }
    }
}

impl std::fmt::Debug for ReviewBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReviewBoard")
    }
}

/// A board over a directory that is not a real workspace, for tests and for
/// the headless paths that have no state file.
pub fn detached_board() -> ReviewBoard {
    ReviewBoard::new(Path::new("/nonexistent/taste-ide-detached"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    #[test]
    fn the_arc_says_what_each_state_means() {
        assert!(!ReviewState::Working.should_be_stopped());
        assert!(ReviewState::FlaggedForReview.flagged());
        assert!(ReviewState::FlaggedForReview.should_be_stopped());
        assert!(!ReviewState::FlaggedForReview.settled());
        for settled in [ReviewState::Merged, ReviewState::Rejected] {
            assert!(settled.settled(), "{settled:?}");
            assert!(settled.should_be_stopped());
            assert!(!settled.flagged());
        }
        for state in [
            ReviewState::Working,
            ReviewState::FlaggedForReview,
            ReviewState::Merged,
            ReviewState::Rejected,
        ] {
            assert_eq!(ReviewState::parse(state.as_str()), Some(state));
        }
        assert_eq!(ReviewState::parse("nonsense"), None);
    }

    /// The flag has to outlive the IDE: an IDE that forgot which
    /// environments were waiting would restart every container it stopped.
    #[test]
    fn a_flag_survives_a_restart() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");
        let calm = env("calm-1");

        let board = ReviewBoard::with_base_for_tests(base.path(), root);
        assert_eq!(board.state(&calm), ReviewState::Working);
        assert!(board.set(&calm, ReviewState::FlaggedForReview).unwrap());
        // Setting the same state again changes nothing and says so.
        assert!(!board.set(&calm, ReviewState::FlaggedForReview).unwrap());

        // A fresh board over the same workspace — a restarted IDE.
        let restarted = ReviewBoard::with_base_for_tests(base.path(), root);
        assert_eq!(restarted.state(&calm), ReviewState::FlaggedForReview);
        let flagged = restarted.flagged();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].0, calm);
        assert!(flagged[0].1.since.is_some(), "when it was flagged");

        assert!(restarted.set(&calm, ReviewState::Merged).unwrap());
        assert!(restarted.flagged().is_empty());
        assert_eq!(
            ReviewBoard::with_base_for_tests(base.path(), root).state(&calm),
            ReviewState::Merged,
            "a settled environment stays settled"
        );
    }

    #[test]
    fn the_primary_cannot_be_submitted_to_itself() {
        let base = tempfile::tempdir().unwrap();
        let board = ReviewBoard::with_base_for_tests(base.path(), "/work/p");
        let refused = board
            .set(&EnvironmentId::primary(), ReviewState::FlaggedForReview)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("merge target"), "{refused}");
    }
}
