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
//! Workspace-wide and a handle, like the standing answers: the chat pane
//! asks on the GTK thread and the MCP server delivers from tokio.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::environment::EnvironmentId;

/// What a nudged agent's next tool result ends with.
pub const NOTE: &str = "[The user has a message waiting for you. Finish the step you are on, \
                        then end your turn so it can be sent.]";

/// The environments whose agent has been asked for a turn and not yet
/// told.
#[derive(Debug, Clone, Default)]
pub struct Nudges(Arc<Mutex<BTreeSet<EnvironmentId>>>);

impl Nudges {
    /// Ask `env`'s agent for a turn, at its next tool result.
    pub fn ask(&self, env: &EnvironmentId) {
        self.0.lock().unwrap().insert(env.clone());
    }

    /// Take the ask back, told or not: the turn it was for has ended.
    pub fn withdraw(&self, env: &EnvironmentId) {
        self.0.lock().unwrap().remove(env);
    }

    /// Whether `env`'s agent is waiting to be told.
    pub fn is_asked(&self, env: &EnvironmentId) -> bool {
        self.0.lock().unwrap().contains(env)
    }

    /// Whether to tell `env`'s agent now; true once per ask.
    pub fn take(&self, env: &EnvironmentId) -> bool {
        self.0.lock().unwrap().remove(env)
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
}
