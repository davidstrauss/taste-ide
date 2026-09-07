//! Waking the coordinator — the primary environment's chat — when an
//! environment is flagged for review, and intervening when it does not
//! answer.
//!
//! The coordinator's brief (taste-mcp's `initialize` instructions) says
//! what to do with a flagged environment; this is the IDE telling it that
//! there is one, so the review is offered rather than waited for (David,
//! 2026-09-06: "Wake up the chat to review the state of envs if they
//! become available for review"). Two things gate and follow the wake-up:
//!
//! - **An exhausted allowance stops it.** While the API is refusing turns
//!   the IDE starts nothing on its own; the toast says why and offers the
//!   wake as a button, because spending an exhausted allowance is the
//!   user's call ("Require user intervention to continue running if
//!   session allowances are exhausted").
//! - **An unanswered wake-up is handed to the user.** If the coordinator
//!   has no live agent, or has not finished a turn within
//!   [`ANSWER_DEADLINE`], a toast names the environment and opens the
//!   chat ("The IDE should intervene if the orchestrator agent isn't
//!   responsive"). The IDE cannot review in its place; what it can do is
//!   make sure nothing waits in silence.

use std::rc::Rc;
use std::time::Duration;

use taste_core::environment::EnvironmentId;
use taste_core::orchestration::ChatState;

use crate::chat::ChatPane;
use crate::chats::Chats;

/// How long the coordinator gets to finish a review turn before the IDE
/// hands the review to the user. A real review reads a branch and runs
/// nothing heavier than a diff; ten minutes is generous for that and short
/// enough that a wedged agent is noticed within the hour.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(10 * 60);

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
            let chats = chats.clone();
            let toasts_again = toasts.clone();
            let env = env.clone();
            toast.connect_button_clicked(move |_| {
                wake(&coordinator, &chats, &toasts_again, &env);
            });
            toasts.add_toast(toast);
        }
        None => wake(&coordinator, chats, toasts, env),
    }
}

/// Send the prompt and start the clock on the answer.
fn wake(
    coordinator: &Rc<ChatPane>,
    chats: &Rc<Chats>,
    toasts: &adw::ToastOverlay,
    env: &EnvironmentId,
) {
    let turns_before = coordinator.chat_facts(EnvironmentId::primary()).turns;
    match coordinator.submit_prompt(review_prompt(env)) {
        Ok(outcome) => {
            tracing::info!("coordinator woken for {env} (queued: {})", outcome.queued);
            watch(coordinator, chats, toasts, env, turns_before);
        }
        Err(reason) => {
            tracing::warn!("coordinator not woken for {env}: {reason}");
            hand_over(
                chats,
                toasts,
                &format!("{env} is ready for review, and the coordinator could not be woken: {reason}"),
            );
        }
    }
}

/// After the deadline, look at the coordinator and hand over if the review
/// did not happen. "Did not happen" is read off the chat's own facts: no
/// turn has ended since the wake-up, or it is sitting on a permission
/// prompt only the user can answer, or it has no agent at all.
fn watch(
    coordinator: &Rc<ChatPane>,
    chats: &Rc<Chats>,
    toasts: &adw::ToastOverlay,
    env: &EnvironmentId,
    turns_before: u64,
) {
    let coordinator = Rc::downgrade(coordinator);
    let chats = chats.clone();
    let toasts = toasts.clone();
    let env = env.clone();
    gtk::glib::timeout_add_local_once(ANSWER_DEADLINE, move || {
        let Some(coordinator) = coordinator.upgrade() else { return };
        let facts = coordinator.chat_facts(EnvironmentId::primary());
        let minutes = ANSWER_DEADLINE.as_secs() / 60;
        let why = match facts.state {
            ChatState::Idle if facts.turns > turns_before => return,
            ChatState::Idle => "it went idle without a turn",
            ChatState::AwaitingPermission => "it is waiting on a permission only you can give",
            ChatState::Streaming => "it is still mid-turn",
            ChatState::Starting => "its session never came up",
            ChatState::Disconnected => "it has no live agent",
        };
        hand_over(
            &chats,
            &toasts,
            &format!("The coordinator has not finished reviewing {env} after {minutes} minutes: {why}"),
        );
    });
}

/// The intervention: a toast that does not fade, naming the environment,
/// with the coordinator's chat one click away.
fn hand_over(chats: &Rc<Chats>, toasts: &adw::ToastOverlay, message: &str) {
    let toast = adw::Toast::new(message);
    toast.set_timeout(0);
    toast.set_button_label(Some("Open chat"));
    let chats = chats.clone();
    toast.connect_button_clicked(move |_| chats.show(&EnvironmentId::primary()));
    toasts.add_toast(toast);
}
