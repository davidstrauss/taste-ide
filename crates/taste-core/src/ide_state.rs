//! Shared IDE state: what the user is looking at.
//!
//! Written by the GTK side (editor tabs, selections), read by the MCP
//! server so agents see the user's context — open files, dirty state, the
//! current selection — the way IDE-integrated agents expect.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile {
    pub path: PathBuf,
    pub dirty: bool,
    /// The focused tab.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub path: PathBuf,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    /// Selected text, capped at capture time.
    pub text: String,
}

/// One answered agent request, with the fact an agent cannot see from its
/// side of the wire: WHY it got the outcome it did. ACP's permission reply
/// is an option id or `Cancelled` — "the user clicked Deny", "auto-approve
/// had nothing to approve with", and "your turn was stopped" all collapse
/// into the same wire shape. This record keeps them distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    /// HH:MM:SS UTC.
    pub when: String,
    /// What the agent asked (the tool-call title).
    pub call: String,
    /// What was sent: "approved", "denied", "cancelled".
    pub outcome: String,
    /// The human fact behind it.
    pub why: String,
}

const PERMISSION_LOG_CAP: usize = 100;

#[derive(Clone, Default)]
pub struct IdeState {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    open_files: Vec<OpenFile>,
    selection: Option<Selection>,
    permission_log: Vec<PermissionDecision>,
}

impl IdeState {
    pub fn set_open_files(&self, files: Vec<OpenFile>) {
        self.inner.write().unwrap().open_files = files;
    }

    pub fn open_files(&self) -> Vec<OpenFile> {
        self.inner.read().unwrap().open_files.clone()
    }

    pub fn set_selection(&self, selection: Option<Selection>) {
        self.inner.write().unwrap().selection = selection;
    }

    pub fn selection(&self) -> Option<Selection> {
        self.inner.read().unwrap().selection.clone()
    }

    /// Record how an agent request was answered. `outcome` is what went
    /// over the wire; `why` is the reason the wire cannot carry.
    pub fn record_permission(&self, call: &str, outcome: &str, why: &str) {
        let mut inner = self.inner.write().unwrap();
        if inner.permission_log.len() >= PERMISSION_LOG_CAP {
            inner.permission_log.remove(0);
        }
        inner.permission_log.push(PermissionDecision {
            when: crate::app_log::clock(),
            call: call.to_string(),
            outcome: outcome.to_string(),
            why: why.to_string(),
        });
    }

    /// The recent decisions, oldest first.
    pub fn permission_log(&self) -> Vec<PermissionDecision> {
        self.inner.read().unwrap().permission_log.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrips_across_clones() {
        let state = IdeState::default();
        let reader = state.clone();
        state.set_open_files(vec![OpenFile {
            path: PathBuf::from("/w/a.rs"),
            dirty: true,
            active: true,
        }]);
        state.set_selection(Some(Selection {
            path: PathBuf::from("/w/a.rs"),
            start_line: 3,
            end_line: 5,
            text: "fn x() {}".into(),
        }));
        assert_eq!(reader.open_files().len(), 1);
        assert!(reader.open_files()[0].dirty);
        assert_eq!(reader.selection().unwrap().start_line, 3);
    }

    #[test]
    fn permission_log_keeps_order_and_caps() {
        let state = IdeState::default();
        for i in 0..(PERMISSION_LOG_CAP + 5) {
            state.record_permission(&format!("call {i}"), "denied", "test");
        }
        let log = state.permission_log();
        assert_eq!(log.len(), PERMISSION_LOG_CAP);
        assert_eq!(
            log.last().unwrap().call,
            format!("call {}", PERMISSION_LOG_CAP + 4)
        );
        assert_eq!(log[0].call, "call 5");
    }
}
