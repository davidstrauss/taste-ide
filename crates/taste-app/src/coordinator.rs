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
//! - **A SILENT wake-up restarts the coordinator.** If it has no live
//!   agent, or has said nothing at all for [`ANSWER_DEADLINE`], the IDE
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

/// How long a woken coordinator may be **silent** before the IDE restarts
/// it. Silent, not busy: see [`verdict`].
///
/// A real review reads a branch and runs nothing heavier than a diff; ten
/// minutes of nothing at all is generous for that and short enough that a
/// wedged agent is noticed within the hour.
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

/// What the deadline finding the chat in this state means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// A turn ended after the wake-up: the errand was taken.
    Answered,
    /// Only the user can move this on.
    NeedsTheUser,
    /// Something is happening in there. Wait again; do not interrupt it.
    StillAlive,
    /// Nothing has happened for the whole deadline.
    Restart(&'static str),
}

/// Read the chat's own facts and say what the deadline means.
///
/// **The deadline measures silence, not elapsed time**, and getting that
/// wrong is what made the coordinator restart mid-conversation every ten
/// minutes (David, 2026-09-08: "the main chat gets restarted every 10
/// minutes because it's not properly watching for turn-taking/chat
/// activity"). Two mistakes, both here:
///
/// - `Streaming` was a restart reason on its own. A turn that is still
///   producing output is the healthiest thing this function can see, and
///   respawning the agent through `session/load` in the middle of one
///   throws away the turn — including a turn the USER is having, since the
///   coordinator's chat is the one the user talks to.
/// - "The errand was answered" was only asked in the `Idle` arm. So a
///   coordinator that answered and then got on with the user's next
///   question was Streaming at the deadline, and the watcher restarted it
///   for work it had already done.
///
/// So: a completed turn ends the watch whatever the chat is doing now, and
/// anything at all in the chat within the deadline — a chunk, a prompt, a
/// turn ending, all of which `ChatPane::touch` records — re-arms it. What
/// restarts the coordinator is a chat that has said nothing for ten
/// minutes, which is the wedge this was built for and the only thing a
/// respawn actually fixes. `idle_for_secs` of `None` means nothing has
/// EVER happened in there, which is not aliveness.
fn verdict(facts: &taste_core::orchestration::ChatFacts, turns_before: u64) -> Verdict {
    if facts.turns > turns_before {
        return Verdict::Answered;
    }
    if facts.state == ChatState::AwaitingPermission {
        // Only the user can answer this, and the card asking is already in
        // the transcript; a restart would lose it.
        return Verdict::NeedsTheUser;
    }
    if facts
        .idle_for_secs
        .is_some_and(|secs| secs < ANSWER_DEADLINE.as_secs())
    {
        return Verdict::StillAlive;
    }
    Verdict::Restart(match facts.state {
        ChatState::Streaming => "it went quiet mid-turn",
        ChatState::Starting => "its session never came up",
        ChatState::Disconnected => "it had no live agent",
        ChatState::Idle | ChatState::AwaitingPermission => "it went idle without finishing a turn",
    })
}

/// After the deadline, look at the coordinator and restart it only if the
/// errand did not happen AND nothing else did either ([`verdict`]).
fn watch(coordinator: &Rc<ChatPane>, errand: &Errand, turns_before: u64, restarts: u32) {
    let weak = Rc::downgrade(coordinator);
    let errand = errand.clone();
    gtk::glib::timeout_add_local_once(ANSWER_DEADLINE, move || {
        let Some(coordinator) = weak.upgrade() else {
            return;
        };
        let facts = coordinator.chat_facts(EnvironmentId::primary());
        let minutes = ANSWER_DEADLINE.as_secs() / 60;
        let subject = errand.subject();
        match verdict(&facts, turns_before) {
            Verdict::Answered => {}
            Verdict::NeedsTheUser => {
                coordinator.note(&format!("{subject} is waiting on the permission above"));
            }
            // The same deadline again, and the same restart count: waiting
            // for a chat that is working is not a failed attempt at
            // anything. The prompt is already in its queue, so the errand
            // lands when the conversation next comes up for air.
            Verdict::StillAlive => watch(&coordinator, &errand, turns_before, restarts),
            Verdict::Restart(why) => {
                let why = format!(
                    "{why}, with nothing happening in it for {minutes} minutes \
                     after being asked about {subject}"
                );
                if restarts >= MAX_RESTARTS {
                    coordinator.note(&format!(
                        "not restarted again: {why}, after {restarts} restarts \
                         — {subject} needs you"
                    ));
                    return;
                }
                restart(&coordinator, &errand, &why, restarts + 1);
            }
        }
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
    use taste_core::orchestration::ChatFacts;

    fn facts(state: ChatState, turns: u64, idle_for_secs: Option<u64>) -> ChatFacts {
        ChatFacts {
            chat: EnvironmentId::primary(),
            agent: "claude".into(),
            model: None,
            session: Some("s-1".into()),
            state,
            idle_for_secs,
            turns,
            usage: None,
            orchestrator: true,
        }
    }

    const DEADLINE: u64 = ANSWER_DEADLINE.as_secs();

    /// The bug: a coordinator mid-turn at the deadline was respawned, and
    /// the coordinator's chat is the one the USER talks to — so a
    /// conversation got its agent killed under it every ten minutes.
    #[test]
    fn a_chat_that_is_still_producing_output_is_never_restarted() {
        // Mid-turn, a chunk arrived a second ago.
        assert_eq!(
            verdict(&facts(ChatState::Streaming, 4, Some(1)), 4),
            Verdict::StillAlive
        );
        // ...and mid-turn is not special: an idle chat that said something
        // recently is alive too (a queued prompt about to go out).
        assert_eq!(
            verdict(&facts(ChatState::Idle, 4, Some(30)), 4),
            Verdict::StillAlive
        );
    }

    /// The other half: the errand WAS answered, and the chat has moved on
    /// to the user's next question. A turn ending ends the watch whatever
    /// the chat is doing at the deadline.
    #[test]
    fn a_finished_turn_ends_the_watch_in_any_state() {
        for state in [
            ChatState::Idle,
            ChatState::Streaming,
            ChatState::Starting,
            ChatState::Disconnected,
            ChatState::AwaitingPermission,
        ] {
            assert_eq!(
                verdict(&facts(state, 5, Some(0)), 4),
                Verdict::Answered,
                "{state:?} after a completed turn"
            );
        }
    }

    /// What the deadline is actually for: nothing has happened at all.
    #[test]
    fn silence_for_the_whole_deadline_restarts_it() {
        assert!(matches!(
            verdict(&facts(ChatState::Streaming, 4, Some(DEADLINE)), 4),
            Verdict::Restart("it went quiet mid-turn")
        ));
        assert!(matches!(
            verdict(&facts(ChatState::Idle, 4, Some(DEADLINE + 90)), 4),
            Verdict::Restart("it went idle without finishing a turn")
        ));
        // Nothing has ever happened in there, which is not aliveness.
        assert!(matches!(
            verdict(&facts(ChatState::Disconnected, 0, None), 0),
            Verdict::Restart("it had no live agent")
        ));
        assert!(matches!(
            verdict(&facts(ChatState::Starting, 0, None), 0),
            Verdict::Restart("its session never came up")
        ));
    }

    /// A permission card outranks the activity check: the note names what
    /// is stuck, and only the user can unstick it.
    #[test]
    fn a_permission_question_is_put_to_the_user_not_restarted_around() {
        assert_eq!(
            verdict(&facts(ChatState::AwaitingPermission, 4, Some(1)), 4),
            Verdict::NeedsTheUser
        );
        assert_eq!(
            verdict(
                &facts(ChatState::AwaitingPermission, 4, Some(DEADLINE * 6)),
                4
            ),
            Verdict::NeedsTheUser
        );
    }

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
