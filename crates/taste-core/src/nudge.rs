//! Asking an agent for a turn without stopping it.
//!
//! A message sent during a turn waits for the turn to end
//! (`crate::queued`), and ACP's only way to end one sooner is to cancel it,
//! which throws away the step in hand. Agents here do nearly all their work
//! through the IDE's own tools, so the IDE asks through those instead: the
//! user's Ask for a Turn on a queued message marks the environment, and the
//! next result of one of its tools carries [`NOTE`] — once — asking the
//! agent to finish its step and end the turn (David, 2026-10-11: "is there
//! any way to nudge the agent to let it know I need a turn?").
//!
//! Any message waiting is also what every wait in the IDE's tools wakes
//! for — the `wait` tool, a task's run, a command's output — so an agent
//! waiting on a build hears the user at once rather than at the end of its
//! sleep (David, 2026-10-17: "The user sending a message should implicitly
//! be something the agent waits on and activates for if in any wait
//! loop").
//!
//! Claude Code needs neither: the IDE installs a hook of Claude Code's own
//! (`PostToolUse`, its documented `additionalContext`) that, after each of
//! the agent's tool calls — its own tools as well as the IDE's — takes the
//! messages waiting behind the turn ([`TAKE_TOOL`]) and puts them beside
//! the tool's result, for the agent's next step. The message is read within
//! the turn, as Claude Code's own editor integrations deliver it, and never
//! sent again after it. A message with an attachment, or one sent while the
//! agent makes no further tool call, still waits for the turn to end; other
//! agents keep the ask above.
//!
//! Workspace-wide and a handle, like the standing answers: the chat pane
//! asks on the GTK thread and the MCP server delivers from tokio.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::environment::EnvironmentId;

/// What a nudged agent's next tool result ends with.
pub const NOTE: &str = "[The user has a message waiting for you. Finish the step you are on, \
                        then end your turn so it can be sent.]";

/// The IDE's tool a Claude Code hook calls after each of the agent's tool
/// calls (`taste_acp::relocate`'s hook script): the user's messages waiting
/// behind the turn, taken for the agent to read within it, which is how a
/// message reaches Claude Code at its next step, as it does in Claude
/// Code's own editor integrations, rather than after the turn ends. Not in
/// any environment's tool listing — the agent is told its messages, not
/// asked to fetch them — and callable on every environment's socket all the
/// same (David, 2026-10-26: "Is there a better way to tell Claude to read a
/// pending message or end its turn?").
pub const TAKE_TOOL: &str = "take_user_messages";

/// What a wait that a message ended says, in place of what it waited for.
pub const WOKEN: &str = "The user sent you a message while you waited. End your turn now: it \
                         arrives as your next prompt.";

#[derive(Debug, Default)]
struct Inner {
    /// Asked for a turn, and not yet told.
    asked: BTreeSet<EnvironmentId>,
    /// A message waiting behind the running turn.
    waiting: BTreeSet<EnvironmentId>,
}

/// The environments whose agent has a message waiting, and those whose
/// agent has been asked for a turn and not yet told.
#[derive(Debug, Clone, Default)]
pub struct Nudges(Arc<Mutex<Inner>>);

impl Nudges {
    /// Ask `env`'s agent for a turn, at its next tool result.
    pub fn ask(&self, env: &EnvironmentId) {
        self.0.lock().unwrap().asked.insert(env.clone());
    }

    /// Take the ask back, told or not, and the message with it: the turn
    /// it was for has ended, and the message has gone.
    pub fn withdraw(&self, env: &EnvironmentId) {
        let mut inner = self.0.lock().unwrap();
        inner.asked.remove(env);
        inner.waiting.remove(env);
    }

    /// Whether `env`'s agent is waiting to be told.
    pub fn is_asked(&self, env: &EnvironmentId) -> bool {
        self.0.lock().unwrap().asked.contains(env)
    }

    /// Whether to tell `env`'s agent now; true once per ask.
    pub fn take(&self, env: &EnvironmentId) -> bool {
        self.0.lock().unwrap().asked.remove(env)
    }

    /// A message is waiting for `env`'s agent behind its turn.
    pub fn message_waiting(&self, env: &EnvironmentId) {
        self.0.lock().unwrap().waiting.insert(env.clone());
    }

    /// Whether a wait of `env`'s agent should end now: a message is
    /// waiting, or the user asked for a turn.
    pub fn should_wake(&self, env: &EnvironmentId) -> bool {
        let inner = self.0.lock().unwrap();
        inner.waiting.contains(env) || inner.asked.contains(env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_is_told_once_and_can_be_taken_back() {
        let nudges = Nudges::default();
        let env = EnvironmentId::parse("i-0004").unwrap();
        let other = EnvironmentId::parse("i-0005").unwrap();
        assert!(!nudges.take(&env));
        nudges.ask(&env);
        assert!(nudges.is_asked(&env));
        assert!(!nudges.take(&other));
        assert!(nudges.take(&env));
        assert!(!nudges.take(&env), "once");
        nudges.ask(&env);
        nudges.clone().withdraw(&env);
        assert!(!nudges.take(&env), "a clone is the same handle");
    }

    /// A waiting message wakes a wait whether or not a turn was asked for,
    /// and goes with the turn it waited behind.
    #[test]
    fn a_message_waiting_wakes_any_wait_until_its_turn_ends() {
        let nudges = Nudges::default();
        let env = EnvironmentId::parse("i-0004").unwrap();
        assert!(!nudges.should_wake(&env));
        nudges.message_waiting(&env);
        assert!(nudges.should_wake(&env));
        assert!(!nudges.take(&env), "a message alone is not an ask");
        nudges.withdraw(&env);
        assert!(!nudges.should_wake(&env));
        nudges.ask(&env);
        assert!(nudges.should_wake(&env));
    }
}
