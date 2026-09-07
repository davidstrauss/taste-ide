//! Waking the coordinator — the primary environment's chat — when an
//! environment is flagged for review, and restarting it when it does not
//! answer.
//!
//! The coordinator's brief (taste-mcp's `initialize` instructions) says
//! what to do with a flagged environment; this is the IDE telling it that
//! there is one, so the review is offered rather than waited for (David,
//! 2026-09-06: "Wake up the chat to review the state of envs if they
//! become available for review"). Two things gate and follow the wake-up:
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

/// The words the coordinator is woken with. They restate the brief's review
/// step with this environment's names in it, so the agent does not have to
/// look anything up to begin.
pub fn review_prompt(env: &EnvironmentId) -> String {
    format!(
        "Environment {env} is flagged for review: its agent says issue {env} is done and \
         published to agents/{env}. Review it now. Call review_list for where it stands \
         and whether the branch merges cleanly; read the branch against the user's \
         branch in your checkout (git log and git diff over agents/{env}). If it passes, \
         merge it into the user's branch and complete issue {env}; if not, send the agent \
         what to fix with chat_send. Tell the user, briefly, what you found and did. \
         Never push — the remote is the user's."
    )
}

/// The words a restarted coordinator picks up with: what happened, where
/// the state is, and the review it owes.
fn resume_prompt(env: &EnvironmentId, why: &str) -> String {
    format!(
        "The IDE restarted you: {why}. If your conversation is above, it is yours to \
         continue. Either way the backlog (issue_list) is the state of what is going on \
         and review_list is what waits for review — read those before anything else. \
         Then: {}",
        review_prompt(env)
    )
}

/// An environment was flagged: wake the coordinator, or say why not.
pub fn wake_for_review(chats: &Rc<Chats>, toasts: &adw::ToastOverlay, env: &EnvironmentId) {
    let primary = EnvironmentId::primary();
    // No agent in the primary means nobody to wake: the user reviews alone,
    // as they always could, and the fleet row already says the environment
    // is ready.
    let Some(coordinator) = chats.pane_for(&primary) else {
        tracing::info!("{env} is flagged for review and there is no coordinator chat to wake");
        return;
    };
    match chats.allowance_exhausted() {
        Some(reopens) => {
            let toast = adw::Toast::new(&format!(
                "{env} is ready for review, but the session allowance is exhausted \
                 (reopens {reopens}) — the coordinator was not woken"
            ));
            toast.set_timeout(0);
            toast.set_button_label(Some("Wake anyway"));
            let env = env.clone();
            toast.connect_button_clicked(move |_| wake(&coordinator, &env));
            toasts.add_toast(toast);
        }
        None => wake(&coordinator, env),
    }
}

/// Send the prompt and start the clock on the answer.
fn wake(coordinator: &Rc<ChatPane>, env: &EnvironmentId) {
    let turns_before = coordinator.chat_facts(EnvironmentId::primary()).turns;
    match coordinator.submit_prompt(review_prompt(env)) {
        Ok(outcome) => {
            tracing::info!("coordinator woken for {env} (queued: {})", outcome.queued);
            watch(coordinator, env, turns_before, 0);
        }
        Err(reason) => {
            tracing::warn!("coordinator not woken for {env}: {reason}");
            restart(
                coordinator,
                env,
                &format!("it could not be woken ({reason})"),
                1,
            );
        }
    }
}

/// After the deadline, look at the coordinator and restart it if the
/// review did not happen. "Did not happen" is read off the chat's own
/// facts: no turn has ended since the wake-up, or it has no agent at all.
fn watch(coordinator: &Rc<ChatPane>, env: &EnvironmentId, turns_before: u64, restarts: u32) {
    let coordinator = Rc::downgrade(coordinator);
    let env = env.clone();
    gtk::glib::timeout_add_local_once(ANSWER_DEADLINE, move || {
        let Some(coordinator) = coordinator.upgrade() else {
            return;
        };
        let facts = coordinator.chat_facts(EnvironmentId::primary());
        let minutes = ANSWER_DEADLINE.as_secs() / 60;
        let why = match facts.state {
            ChatState::Idle if facts.turns > turns_before => return,
            ChatState::AwaitingPermission => {
                // Only the user can answer this, and the card asking is
                // already in the transcript; a restart would lose it.
                coordinator.note(&format!(
                    "the review of {env} is waiting on the permission above"
                ));
                return;
            }
            ChatState::Idle => "it went idle without finishing a turn",
            ChatState::Streaming => "it was still mid-turn",
            ChatState::Starting => "its session never came up",
            ChatState::Disconnected => "it had no live agent",
        };
        let why = format!("{why} {minutes} minutes after being asked to review {env}");
        if restarts >= MAX_RESTARTS {
            coordinator.note(&format!(
                "not restarted again: {why}, after {restarts} restarts — the review of \
                 {env} needs you"
            ));
            return;
        }
        restart(&coordinator, &env, &why, restarts + 1);
    });
}

/// Respawn with the conversation, note it in the chat, ask again once the
/// session is ready, and watch again.
fn restart(coordinator: &Rc<ChatPane>, env: &EnvironmentId, why: &str, restarts: u32) {
    tracing::warn!("restarting the coordinator (restart {restarts}): {why}");
    coordinator.respawn_keeping_conversation(why);
    let prompt = resume_prompt(env, why);
    coordinator.on_ready_once(Box::new(move |pane| {
        if let Err(reason) = pane.submit_prompt(prompt) {
            pane.note(&format!(
                "the review prompt was not taken after the restart: {reason}"
            ));
        }
    }));
    let turns_before = coordinator.chat_facts(EnvironmentId::primary()).turns;
    watch(coordinator, env, turns_before, restarts);
}
