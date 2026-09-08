//! Waking the coordinator — the primary environment's chat — when there
//! is something for it to do, and restarting it when it does not answer.
//!
//! Two things wake it, and they are the two the brief gives it authority
//! over ([`Errand`]): an environment flagged for review (David,
//! 2026-09-06: "Wake up the chat to review the state of envs if they
//! become available for review"), and a new item on the backlog (David,
//! 2026-09-08: "Wake up the coodinator agent whenever a new item is added
//! to the backlog. It can decide what to do").
//!
//! The coordinator's brief (taste-mcp's `initialize` instructions) already
//! says what to do in both cases; this is the IDE telling it that there is
//! a case, so the work is offered rather than waited for. Two things gate
//! and follow the wake-up:
//!
//! - **An exhausted allowance stops it.** While the API is refusing turns
//!   the IDE starts nothing on its own; a toast says why and offers the
//!   wake as a button, because spending an exhausted allowance is the
//!   user's call ("Require user intervention to continue running if
//!   session allowances are exhausted"). This is the one toast here.
//! - **An unanswered wake-up restarts the coordinator.** If it has no live
//!   agent, or has not finished a turn within [`ANSWER_DEADLINE`], the IDE
//!   respawns it with its conversation (`session/load`), notes the restart
//!   in the transcript, and asks again — with a pointer at the backlog and
//!   the review list, which are the high-level state a fresh session picks
//!   things up from. No toast: the user may be asleep, and the chat is
//!   where the story is told ("You should only pop up the toast noting
//!   that the orchestrator was restarted; do the restart automatically" →
//!   "I actually don't even need a toast for the agent restart. Just note
//!   it in the chat"). A chat sitting on a permission prompt is not
//!   restarted — only the user can answer it, and the card is already in
//!   the transcript. After [`MAX_RESTARTS`] the IDE stops and says so in
//!   the chat, rather than restarting forever.

use std::rc::Rc;
use std::time::Duration;

use taste_core::environment::EnvironmentId;
use taste_core::orchestration::ChatState;

use crate::chat::ChatPane;
use crate::chats::Chats;

/// How long the coordinator gets to finish a review turn before the IDE
/// restarts it. A real review reads a branch and runs nothing heavier than
/// a diff; ten minutes is generous for that and short enough that a wedged
/// agent is noticed within the hour.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// Restarts per wake-up before the IDE gives up and leaves a note.
pub const MAX_RESTARTS: u32 = 2;

/// What the coordinator is being woken about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Errand {
    /// An environment says its issue is done and wants a review.
    Review(EnvironmentId),
    /// A new item on the backlog, for triage.
    Filed { id: String, title: String },
}

impl Errand {
    /// What is waiting, in a few words — for the transcript's notes and the
    /// one toast here, which are sentences about it rather than to it.
    fn subject(&self) -> String {
        match self {
            Errand::Review(env) => format!("the review of {env}"),
            Errand::Filed { id, .. } => format!("issue {id}"),
        }
    }

    /// The words the coordinator is woken with. They restate the step of
    /// the brief that applies, with this errand's names in it, so the
    /// agent does not have to look anything up to begin.
    fn prompt(&self) -> String {
        match self {
            Errand::Review(env) => format!(
                "Environment {env} is flagged for review: its agent says issue {env} is done \
                 and published to agents/{env}. Review it now. Call review_list for where it \
                 stands and whether the branch merges cleanly; read the branch against the \
                 user's branch in your checkout (git log and git diff over agents/{env}). If \
                 it passes, merge it into the user's branch and complete issue {env}; if not, \
                 send the agent what to fix with chat_send. Tell the user, briefly, what you \
                 found and did. Never push — the remote is the user's."
            ),
            Errand::Filed { id, title } => format!(
                "Issue {id} was just filed on the backlog: \"{title}\". It is yours to \
                 triage — you were not the one who wrote it. Read it and the queue around \
                 it (issue_list), then do whichever of these it actually calls for, and no \
                 more: move it (issue_reorder) if it outranks what sits above it; start it \
                 (issue_start) if it is ready, nothing it depends on is unfinished, and \
                 there is room under the cap; link or decline it (issue_link, issue_update) \
                 if it duplicates or is obsoleted by something already on the queue; or \
                 leave it where it is. Tell the user in one or two lines what you did and \
                 why — including when the answer was to leave it alone. Do not file \
                 anything in reply."
            ),
        }
    }

    /// The words a restarted coordinator picks up with: what happened,
    /// where the state is, and the errand it still owes.
    fn resume_prompt(&self, why: &str) -> String {
        format!(
            "The IDE restarted you: {why}. If your conversation is above, it is yours to \
             continue. Either way the backlog (issue_list) is the state of what is going on \
             and review_list is what waits for review — read those before anything else. \
             Then: {}",
            self.prompt()
        )
    }

    /// Why the user is being told the coordinator was not woken.
    fn toast(&self, reopens: &str) -> String {
        let what = match self {
            Errand::Review(env) => format!("{env} is ready for review"),
            Errand::Filed { id, title } => format!("{id} was filed (\"{title}\")"),
        };
        format!(
            "{what}, but the session allowance is exhausted (reopens {reopens}) — the \
             coordinator was not woken"
        )
    }
}

/// An environment was flagged for review: wake the coordinator about it.
pub fn wake_for_review(chats: &Rc<Chats>, toasts: &adw::ToastOverlay, env: &EnvironmentId) {
    wake_for(chats, toasts, Errand::Review(env.clone()));
}

/// An item was filed on the backlog: wake the coordinator about it, unless
/// the coordinator is the one that filed it.
///
/// Its own filing is not news to it, and answering it would be a turn
/// spent to learn nothing — or a loop, if the reply filed another. The
/// user's own filings come through here too, with no environment on them,
/// and those are exactly the ones worth waking for.
pub fn wake_for_filed(
    chats: &Rc<Chats>,
    toasts: &adw::ToastOverlay,
    id: &str,
    title: &str,
    by: Option<&EnvironmentId>,
) {
    if !filing_wakes(by) {
        tracing::debug!("{id} was filed by the coordinator itself; not waking it");
        return;
    }
    wake_for(
        chats,
        toasts,
        Errand::Filed {
            id: id.to_string(),
            title: title.to_string(),
        },
    );
}

/// Whether a filing is news to the coordinator. Its own are not.
fn filing_wakes(by: Option<&EnvironmentId>) -> bool {
    by != Some(&EnvironmentId::primary())
}

/// Wake the coordinator about an errand, or say why not.
fn wake_for(chats: &Rc<Chats>, toasts: &adw::ToastOverlay, errand: Errand) {
    let primary = EnvironmentId::primary();
    // No agent in the primary means nobody to wake: the user works alone,
    // as they always could, and the fleet row and the backlog already say
    // what happened.
    let Some(coordinator) = chats.pane_for(&primary) else {
        tracing::info!(
            "nothing to wake about {}: there is no coordinator chat",
            errand.subject()
        );
        return;
    };
    match chats.allowance_exhausted() {
        Some(reopens) => {
            let toast = adw::Toast::new(&errand.toast(&reopens));
            toast.set_timeout(0);
            toast.set_button_label(Some("Wake anyway"));
            toast.connect_button_clicked(move |_| wake(&coordinator, &errand));
            toasts.add_toast(toast);
        }
        None => wake(&coordinator, &errand),
    }
}

/// Send the prompt and start the clock on the answer.
fn wake(coordinator: &Rc<ChatPane>, errand: &Errand) {
    let turns_before = coordinator.chat_facts(EnvironmentId::primary()).turns;
    match coordinator.submit_prompt(errand.prompt()) {
        Ok(outcome) => {
            tracing::info!(
                "coordinator woken about {} (queued: {})",
                errand.subject(),
                outcome.queued
            );
            watch(coordinator, errand, turns_before, 0);
        }
        Err(reason) => {
            tracing::warn!("coordinator not woken about {}: {reason}", errand.subject());
            restart(
                coordinator,
                errand,
                &format!("it could not be woken ({reason})"),
                1,
            );
        }
    }
}

/// After the deadline, look at the coordinator and restart it if the
/// errand did not happen. "Did not happen" is read off the chat's own
/// facts: no turn has ended since the wake-up, or it has no agent at all.
fn watch(coordinator: &Rc<ChatPane>, errand: &Errand, turns_before: u64, restarts: u32) {
    let coordinator = Rc::downgrade(coordinator);
    let errand = errand.clone();
    gtk::glib::timeout_add_local_once(ANSWER_DEADLINE, move || {
        let Some(coordinator) = coordinator.upgrade() else {
            return;
        };
        let facts = coordinator.chat_facts(EnvironmentId::primary());
        let minutes = ANSWER_DEADLINE.as_secs() / 60;
        let subject = errand.subject();
        let why = match facts.state {
            ChatState::Idle if facts.turns > turns_before => return,
            ChatState::AwaitingPermission => {
                // Only the user can answer this, and the card asking is
                // already in the transcript; a restart would lose it.
                coordinator.note(&format!("{subject} is waiting on the permission above"));
                return;
            }
            ChatState::Idle => "it went idle without finishing a turn",
            ChatState::Streaming => "it was still mid-turn",
            ChatState::Starting => "its session never came up",
            ChatState::Disconnected => "it had no live agent",
        };
        let why = format!("{why} {minutes} minutes after being asked about {subject}");
        if restarts >= MAX_RESTARTS {
            coordinator.note(&format!(
                "not restarted again: {why}, after {restarts} restarts — {subject} needs you"
            ));
            return;
        }
        restart(&coordinator, &errand, &why, restarts + 1);
    });
}

/// Respawn with the conversation, note it in the chat, ask again once the
/// session is ready, and watch again.
fn restart(coordinator: &Rc<ChatPane>, errand: &Errand, why: &str, restarts: u32) {
    tracing::warn!("restarting the coordinator (restart {restarts}): {why}");
    coordinator.respawn_keeping_conversation(why);
    let prompt = errand.resume_prompt(why);
    coordinator.on_ready_once(Box::new(move |pane| {
        if let Err(reason) = pane.submit_prompt(prompt) {
            pane.note(&format!(
                "the prompt was not taken after the restart: {reason}"
            ));
        }
    }));
    let turns_before = coordinator.chat_facts(EnvironmentId::primary()).turns;
    watch(coordinator, errand, turns_before, restarts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_other_filers_wake_the_coordinator() {
        // Its own filing is not news to it, and a reply that filed
        // something would wake it again.
        assert!(!filing_wakes(Some(&EnvironmentId::primary())));
        // Everyone else's is: a sub-agent's...
        assert!(filing_wakes(Some(&EnvironmentId::parse("i-0007").unwrap())));
        // ...and the user's, which carries no environment at all.
        assert!(filing_wakes(None));
    }

    #[test]
    fn a_filing_is_put_to_the_coordinator_as_triage() {
        let errand = Errand::Filed {
            id: "i-0012".into(),
            title: "The composer loses a half-typed follow-up".into(),
        };
        let prompt = errand.prompt();
        assert!(prompt.contains("i-0012"), "{prompt}");
        assert!(
            prompt.contains("The composer loses a half-typed follow-up"),
            "the title saves it a lookup: {prompt}"
        );
        assert!(
            prompt.contains("issue_reorder") && prompt.contains("issue_start"),
            "it is told the moves it has: {prompt}"
        );
        assert!(
            prompt.contains("Do not file anything in reply"),
            "the loop guard is in the words too, not only in the wiring: {prompt}"
        );
        assert_eq!(errand.subject(), "issue i-0012");
    }

    #[test]
    fn a_restart_carries_the_errand_it_still_owes() {
        for errand in [
            Errand::Review(EnvironmentId::parse("i-0007").unwrap()),
            Errand::Filed {
                id: "i-0012".into(),
                title: "Something".into(),
            },
        ] {
            let resumed = errand.resume_prompt("it had no live agent");
            assert!(resumed.contains("it had no live agent"), "{resumed}");
            assert!(
                resumed.ends_with(&errand.prompt()),
                "the errand survives the restart: {resumed}"
            );
        }
    }
}
