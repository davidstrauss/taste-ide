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

#[derive(Clone, Default)]
pub struct IdeState {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    open_files: Vec<OpenFile>,
    selection: Option<Selection>,
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
}
