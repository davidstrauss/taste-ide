//! Standing answers: the permission questions this project has already
//! settled.
//!
//! A permission card asks about one call. "Don't ask again" is a different
//! answer — it is about every call like it, from now on — and the thing it
//! is *about* is the project, not the conversation it happened to be given
//! in. An environment is a clone with its own agent process and its own
//! agent home, so an answer an agent persisted would live in that one
//! world; with eleven environments restored at startup, an answer given
//! once would have to be given eleven times, and again for every
//! environment `issue_start` makes tomorrow.
//!
//! So the IDE keeps the answer. It already mediates every permission
//! request, from every environment, in one process, which makes a standing
//! answer it holds in force the moment it is given — in the environments
//! running now and in the ones born after. And it is agent-agnostic: ACP
//! serves Gemini and Copilot too, and neither has Claude Code's settings
//! file.
//!
//! Three properties are deliberate.
//!
//! - **The user answers and the IDE writes.** Nothing an agent can say
//!   reaches this book; the only way in is a click on the card. An agent
//!   could not write it even if something here forgot to check, because
//!   the book lives beside the rest of the workspace's state under
//!   `$XDG_STATE_HOME` — outside the checkout, which is the only thing an
//!   agent can write at all, and outside the read-only stub its cwd
//!   actually is (`taste_acp::sandbox::ensure_workspace_stub`). That is
//!   what protects it: not [`crate::policy::write_allowed`], which bounds
//!   writes THROUGH the IDE, but the fact that the file is not in a place
//!   any agent can reach. CLAUDE.md's "configuration authority is
//!   execution authority" is the adjacent rule, and it points the same
//!   way: a permissions file an agent can write is a permissions file an
//!   agent can widen.
//! - **The grain is the tool.** For the read set — `ide_search`,
//!   `environment`, the rest of [`Effect::Read`] — the tool is the
//!   right grain and is what the complaint was about: a read is a read
//!   whatever its arguments. For `ide_exec` it is plainly not, and the
//!   caller is expected to refuse a standing *allow* there rather than
//!   leaving a shell with no gate; see `taste_mcp::protocol::may_stand`.
//!   A standing *deny* is refused for nothing, because refusing is never a
//!   widening — that is the asymmetry, and it is the reason for it.
//! - **It is a list, so it can be taken back.** [`StandingAnswers::list`]
//!   is what the chat's settings shade shows and
//!   [`StandingAnswers::forget`] is the undo. A policy with no way to see
//!   or revoke it is a trap.
//!
//! Persisted with the rest of the workspace's state
//! ([`crate::state::WorkspaceState`]), which is per workspace root and
//! therefore per project, and shared by every environment in it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// What the user said, for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StandingAnswer {
    Allow,
    Deny,
}

impl StandingAnswer {
    pub fn as_str(self) -> &'static str {
        match self {
            StandingAnswer::Allow => "allow",
            StandingAnswer::Deny => "deny",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "allow" => Some(StandingAnswer::Allow),
            "deny" => Some(StandingAnswer::Deny),
            _ => None,
        }
    }

    /// How a settings row says it.
    pub fn detail(self) -> &'static str {
        match self {
            StandingAnswer::Allow => "allowed without asking",
            StandingAnswer::Deny => "refused without asking",
        }
    }
}

/// One settled question: which tool, what was said, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingRecord {
    /// The IDE MCP tool's bare name (`ide_search`), which is the grain.
    pub tool: String,
    pub answer: StandingAnswer,
    /// RFC 3339, for "answered three days ago".
    pub since: Option<String>,
}

/// The project's book of standing answers, readable and writable from
/// anywhere.
///
/// A cloneable handle on the [`crate::Workspace`], like
/// [`crate::review::ReviewBoard`] and for the same reason: the readers and
/// the writers are in different crates and on different threads — a chat
/// pane answers a card on the GTK main thread, every other chat pane reads
/// the answer while deciding whether to raise a card of its own, and the
/// settings shade lists the lot. One handle means they cannot disagree.
///
/// Remembering is split in two on purpose. [`StandingAnswers::remember`]
/// moves the in-memory answer and nothing else, because the next request
/// the agent makes must see it and a GTK click handler may not block on a
/// file (CLAUDE.md → Rules of the road); [`StandingAnswers::persist`] is
/// the write, and the caller runs it wherever blocking IO is allowed.
#[derive(Clone)]
pub struct StandingAnswers {
    inner: Arc<Mutex<Book>>,
}

struct Book {
    root: PathBuf,
    /// State-file directory override. `None` — always, outside tests — is
    /// the XDG location. Tests point it at a tempdir so they neither read
    /// the developer's real state nor race each other over `XDG_STATE_HOME`.
    base: Option<PathBuf>,
    /// Filled from the state file on first use, so a book nobody asks
    /// never touches the disk.
    answers: Option<BTreeMap<String, StandingRecord>>,
}

impl StandingAnswers {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Book {
                root: root.into(),
                base: None,
                answers: None,
            })),
        }
    }

    /// A book whose state file lives under `base` rather than the XDG state
    /// directory.
    #[doc(hidden)]
    pub fn with_base_for_tests(base: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        let book = Self::new(root);
        if let Ok(mut inner) = book.inner.lock() {
            inner.base = Some(base.into());
        }
        book
    }

    /// Fill the book from state the caller has already read.
    ///
    /// The window reads the workspace's state file once at startup, and
    /// this takes the answers out of it rather than reading the file
    /// again — which matters because the alternative is a lazy read, and
    /// the first thing that would trigger it is a permission request
    /// arriving on the GTK main thread. Called with the default state by a
    /// probe, so a screenshot run answers nothing out of a real project's
    /// policy.
    ///
    /// Does nothing once the book has been filled: whatever is in memory is
    /// what every chat in this window has been answering from, and
    /// replacing it underneath them would be a second source of truth.
    pub fn hydrate_from(&self, state: &crate::state::WorkspaceState) {
        let Ok(mut book) = self.inner.lock() else {
            return;
        };
        if book.answers.is_some() {
            return;
        }
        book.answers = Some(Book::records(state));
    }

    /// What this project has already said about `tool`, if anything.
    /// `None` means "ask" — the absence of an answer is not an answer.
    ///
    /// Answered from memory, which is why this is callable on every
    /// permission request. It is also why the handle rather than the file
    /// is what the panes share: a second handle sees the file as of its own
    /// first read, exactly as [`crate::review::ReviewBoard`] does. Two
    /// windows on one workspace therefore each keep their own view until
    /// one is restarted — the same bargain that state file has everywhere
    /// else, and the reason one window holds the supervision claim.
    pub fn answer(&self, tool: &str) -> Option<StandingAnswer> {
        let mut book = self.inner.lock().ok()?;
        book.hydrate();
        book.answers.as_ref()?.get(tool).map(|record| record.answer)
    }

    /// Every settled question, in tool order — the settings shade's list.
    pub fn list(&self) -> Vec<StandingRecord> {
        let Ok(mut book) = self.inner.lock() else {
            return Vec::new();
        };
        book.hydrate();
        book.answers
            .as_ref()
            .map(|answers| answers.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Settle a question. Returns whether anything changed, so a caller can
    /// skip the write and the receipt when it did not.
    ///
    /// In memory only — see the type's own note. Follow it with
    /// [`StandingAnswers::persist`] off the main thread.
    pub fn remember(&self, tool: &str, answer: StandingAnswer) -> bool {
        let Ok(mut book) = self.inner.lock() else {
            return false;
        };
        book.hydrate();
        let answers = book.answers.get_or_insert_with(BTreeMap::new);
        if answers.get(tool).map(|record| record.answer) == Some(answer) {
            return false;
        }
        answers.insert(
            tool.to_string(),
            StandingRecord {
                tool: tool.to_string(),
                answer,
                since: Some(crate::state::now_rfc3339()),
            },
        );
        true
    }

    /// Take an answer back: the question is open again and the next call
    /// asks. In memory only, exactly as [`StandingAnswers::remember`] is.
    pub fn forget(&self, tool: &str) -> bool {
        let Ok(mut book) = self.inner.lock() else {
            return false;
        };
        book.hydrate();
        book.answers
            .as_mut()
            .is_some_and(|answers| answers.remove(tool).is_some())
    }

    /// Write the book to the workspace's state file. **Blocking IO** — call
    /// it from `spawn_blocking`, never from a GTK handler.
    pub fn persist(&self) -> Result<()> {
        let book = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("the standing-answer book is poisoned"))?;
        book.persist()
    }
}

impl Book {
    fn hydrate(&mut self) {
        if self.answers.is_some() {
            return;
        }
        let state = self.load();
        self.answers = Some(Self::records(&state));
    }

    fn records(state: &crate::state::WorkspaceState) -> BTreeMap<String, StandingRecord> {
        state
            .standing
            .iter()
            .map(|entry| {
                (
                    entry.tool.clone(),
                    StandingRecord {
                        tool: entry.tool.clone(),
                        answer: entry.answer,
                        since: entry.since.clone(),
                    },
                )
            })
            .collect()
    }

    /// Read-modify-write of the one field this owns, exactly as every other
    /// writer of the workspace state file does it.
    fn persist(&self) -> Result<()> {
        let mut state = self.load();
        state.standing = self
            .answers
            .as_ref()
            .map(|answers| {
                answers
                    .values()
                    .map(|record| crate::state::StandingEntry {
                        tool: record.tool.clone(),
                        answer: record.answer,
                        since: record.since.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
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
}

impl std::fmt::Debug for StandingAnswers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StandingAnswers")
    }
}

/// A book over a directory that is not a real workspace, for tests and for
/// the headless paths that have no state file.
pub fn detached_book() -> StandingAnswers {
    StandingAnswers::new(std::path::Path::new("/nonexistent/taste-ide-detached"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The requirement the whole design is for (David, 2026-09-13: "Ensure
    /// that any 'don't ask again' policies get encoded for the project —
    /// including all environments"). A book is a handle on the project's
    /// state file, so an answer given through one is in force through every
    /// other — the environments running now, and the ones opened later.
    #[test]
    fn an_answer_given_in_one_environment_is_in_force_in_another() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");

        // Two chats, in two environments, of one project — which is two
        // clones of the workspace's ONE handle, because that is what
        // `Workspace::standing` hands every pane. The sharing is the
        // mechanism: an answer given in the chat that was asked is in force
        // in the chat that was not, with no file read in between and no
        // window in which the two could disagree.
        let workspace = StandingAnswers::with_base_for_tests(base.path(), root);
        let asked = workspace.clone();
        let other = workspace.clone();
        assert_eq!(other.answer("ide_search"), None, "nothing is settled yet");

        assert!(asked.remember("ide_search", StandingAnswer::Allow));
        assert_eq!(
            other.answer("ide_search"),
            Some(StandingAnswer::Allow),
            "a chat that has not asked yet already has the answer"
        );
        assert_eq!(other.answer("ide_exec"), None, "one tool, not all");
        asked.persist().unwrap();

        // ...and so is an environment that did not exist when it was given.
        // `issue_start` makes a clone and the IDE makes a pane for it; both
        // read this book, and a book opened now is born holding the policy.
        let born_later = StandingAnswers::with_base_for_tests(base.path(), root);
        assert_eq!(born_later.answer("ide_search"), Some(StandingAnswer::Allow));

        // Another project's answers are not this one's: the book is keyed
        // by workspace root, like every other thing in the state file.
        let elsewhere = StandingAnswers::with_base_for_tests(base.path(), "/work/other");
        assert_eq!(elsewhere.answer("ide_search"), None);
    }

    /// It has to outlive the IDE too: eleven environments restored at
    /// startup must not mean the question asked eleven more times.
    #[test]
    fn an_answer_survives_a_restart_and_can_be_taken_back() {
        let base = tempfile::tempdir().unwrap();
        let root = Path::new("/work/project");

        let book = StandingAnswers::with_base_for_tests(base.path(), root);
        book.remember("ide_search", StandingAnswer::Allow);
        book.remember("ide_exec", StandingAnswer::Deny);
        book.persist().unwrap();

        let restarted = StandingAnswers::with_base_for_tests(base.path(), root);
        assert_eq!(restarted.answer("ide_search"), Some(StandingAnswer::Allow));
        assert_eq!(restarted.answer("ide_exec"), Some(StandingAnswer::Deny));
        let listed: Vec<(String, StandingAnswer)> = restarted
            .list()
            .into_iter()
            .map(|record| (record.tool, record.answer))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("ide_exec".to_string(), StandingAnswer::Deny),
                ("ide_search".to_string(), StandingAnswer::Allow)
            ],
            "listed in tool order, so the settings rows do not shuffle"
        );
        assert!(restarted.list().iter().all(|r| r.since.is_some()));

        // The undo, and it survives too — a revoked answer that came back
        // on the next launch would be worse than never having offered one.
        assert!(restarted.forget("ide_search"));
        assert!(!restarted.forget("ide_search"), "already gone");
        restarted.persist().unwrap();
        let after = StandingAnswers::with_base_for_tests(base.path(), root);
        assert_eq!(after.answer("ide_search"), None);
        assert_eq!(after.answer("ide_exec"), Some(StandingAnswer::Deny));
    }

    /// Remembering the same thing twice changes nothing, so the receipt in
    /// the transcript and the write to disk both stay honest.
    #[test]
    fn remembering_the_same_answer_is_not_a_change() {
        let base = tempfile::tempdir().unwrap();
        let book = StandingAnswers::with_base_for_tests(base.path(), "/work/p");
        assert!(book.remember("ide_search", StandingAnswer::Allow));
        assert!(!book.remember("ide_search", StandingAnswer::Allow));
        // ...but changing one's mind is a change.
        assert!(book.remember("ide_search", StandingAnswer::Deny));
        assert_eq!(book.answer("ide_search"), Some(StandingAnswer::Deny));
    }

    #[test]
    fn the_wire_spelling_round_trips() {
        for answer in [StandingAnswer::Allow, StandingAnswer::Deny] {
            assert_eq!(StandingAnswer::parse(answer.as_str()), Some(answer));
        }
        assert_eq!(StandingAnswer::parse("maybe"), None);
    }
}
