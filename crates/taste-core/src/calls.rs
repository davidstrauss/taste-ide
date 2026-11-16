//! The IDE tool calls each environment's agent has in flight, so the
//! chat's Stop ends them too.
//!
//! Stop sends ACP's `session/cancel`, which is the agent's to act on: it
//! ends the turn, and may or may not tell the IDE's MCP server that a call
//! it had made is abandoned. Until that call answers, a turn whose tool is
//! still working does not end, and whatever the call started — a `cargo
//! build` through `ide_exec` — went on running in the container, using the
//! VM, with nobody waiting for it (David, 2026-11-15: "The stop button for
//! the agent is doing nothing"). So the server registers every tool call
//! it is answering here, by environment, and the chat's Stop ends them all
//! at once ([`Calls::stop`]): each answers at once that the user stopped
//! it, and what it started is stopped with it (the server's part,
//! `taste_mcp::server`).
//!
//! An MCP client's own `notifications/cancelled` ends one call the same way
//! ([`CallEnd::end`] with [`Ended::Cancelled`]), and is not answered, as
//! the protocol asks.
//!
//! Workspace-wide and a handle, like [`crate::nudge::Nudges`]: the chat
//! pane stops from the GTK thread, and the MCP server waits from tokio.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::environment::EnvironmentId;

/// Why a call ended before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The user's Stop, on the chat of the call's environment: the call
    /// answers that it was stopped.
    Stop,
    /// The client cancelled this one request (`notifications/cancelled`):
    /// it is not answered at all.
    Cancelled,
}

type Signal = tokio::sync::watch::Sender<Option<Ended>>;

#[derive(Default)]
struct Inner {
    next: u64,
    open: HashMap<u64, (EnvironmentId, Arc<Signal>)>,
}

/// Every tool call in flight, by environment.
#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Inner>>);

impl Calls {
    /// A call `env`'s agent has just made, open until the returned guard
    /// is dropped.
    pub fn begin(&self, env: &EnvironmentId) -> Call {
        let (signal, ended) = tokio::sync::watch::channel(None);
        let signal = Arc::new(signal);
        let id = {
            let mut inner = self.0.lock().unwrap();
            inner.next += 1;
            let id = inner.next;
            inner.open.insert(id, (env.clone(), signal.clone()));
            id
        };
        Call {
            calls: self.clone(),
            id,
            signal,
            ended,
        }
    }

    /// The user's Stop: end every call `env`'s agent has in flight. How
    /// many there were.
    pub fn stop(&self, env: &EnvironmentId) -> usize {
        let signals: Vec<Arc<Signal>> = self
            .0
            .lock()
            .unwrap()
            .open
            .values()
            .filter(|(of, _)| of == env)
            .map(|(_, signal)| signal.clone())
            .collect();
        for signal in &signals {
            end(signal, Ended::Stop);
        }
        signals.len()
    }

    /// How many calls `env`'s agent has in flight.
    pub fn in_flight(&self, env: &EnvironmentId) -> usize {
        self.0
            .lock()
            .unwrap()
            .open
            .values()
            .filter(|(of, _)| of == env)
            .count()
    }
}

/// The first reason a call ends is the one it keeps.
fn end(signal: &Signal, why: Ended) {
    signal.send_if_modified(|ended| {
        if ended.is_some() {
            return false;
        }
        *ended = Some(why);
        true
    });
}

/// One call in flight. Dropping it closes it: a call that answered, or
/// was ended, is no longer anything a Stop reaches.
pub struct Call {
    calls: Calls,
    id: u64,
    signal: Arc<Signal>,
    ended: tokio::sync::watch::Receiver<Option<Ended>>,
}

impl Call {
    /// Resolves when the call is ended, with why; never, while it is not.
    pub async fn ended(&mut self) -> Ended {
        let why = self
            .ended
            .wait_for(Option::is_some)
            .await
            .map(|why| why.unwrap_or(Ended::Stop));
        match why {
            Ok(why) => why,
            // The sender lives as long as this guard does.
            Err(_) => std::future::pending().await,
        }
    }

    /// What ends this one call from elsewhere: the connection's handle for
    /// its request id.
    pub fn handle(&self) -> CallEnd {
        CallEnd(self.signal.clone())
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.calls.0.lock().unwrap().open.remove(&self.id);
    }
}

/// Ends one call. Harmless once the call is over.
#[derive(Clone)]
pub struct CallEnd(Arc<Signal>);

impl CallEnd {
    pub fn end(&self, why: Ended) {
        end(&self.0, why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> EnvironmentId {
        EnvironmentId::parse(name).unwrap()
    }

    /// Stop ends the calls of its own environment, and no other's; a call
    /// that has answered is out of reach.
    #[tokio::test]
    async fn stop_ends_its_environments_calls_and_no_others() {
        let calls = Calls::default();
        let mut mine = calls.begin(&env("i-0001"));
        let mut theirs = calls.begin(&env("i-0002"));
        let answered = calls.begin(&env("i-0001"));
        drop(answered);
        assert_eq!(calls.in_flight(&env("i-0001")), 1);

        assert_eq!(calls.stop(&env("i-0001")), 1);
        assert_eq!(mine.ended().await, Ended::Stop);
        let still = tokio::time::timeout(std::time::Duration::from_millis(50), theirs.ended());
        assert!(still.await.is_err(), "another environment's call runs on");

        drop(mine);
        assert_eq!(calls.in_flight(&env("i-0001")), 0);
        assert_eq!(calls.stop(&env("i-0001")), 0, "nothing left to stop");
    }

    /// The first reason sticks: a client's cancel after a Stop does not
    /// turn a stopped call into one that goes unanswered.
    #[tokio::test]
    async fn the_first_reason_is_the_one_kept() {
        let calls = Calls::default();
        let mut call = calls.begin(&env("i-0001"));
        let handle = call.handle();
        calls.stop(&env("i-0001"));
        handle.end(Ended::Cancelled);
        assert_eq!(call.ended().await, Ended::Stop);

        // A handle outliving its call ends nothing, and does not fail.
        drop(call);
        handle.end(Ended::Cancelled);
    }
}
