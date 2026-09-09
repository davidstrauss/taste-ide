//! The intervention panel: a card that opens from the bottom of the
//! subpanel whose rows it is about, and never a modal.
//!
//! Two of them stand in the left column. The files' opens under the file
//! list — a discard to confirm, a stash to name, the checked files' bulk
//! ops, the Staged view's commit composer. The backlog's opens under its
//! list — a new issue, an edit of an existing one, the console's questions
//! about an environment (rename, destroy, reject), because an environment
//! is a backlog row. One slot for the whole column put a question about a
//! file under the Logs section and a question about an issue above the
//! backlog rather than in it (David, 2026-09-06: "The intervention panels
//! need to be at the bottom of the relevant subpanel. For staging dirty
//! files, for example, it should pop up from the bottom of the files
//! subpanel. For adding a new/editing an existing backlog issue, it should
//! pop up at the bottom of the backlog").
//!
//! The panel is only the shell — a header with the title and, when the
//! flow can be cancelled, an X — and a content box the flow fills. What
//! closing means is the owner's to say (`set_on_dismiss`): the files'
//! panel gives the filter views their selection pane back, the backlog's
//! keeps a half-written issue for the next New issue.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::hover::FullTextOnHover;
use adw::prelude::*;

pub struct Panel {
    pub widget: gtk::Box,
    /// Kept here rather than read off the widget: `is_visible` walks up to
    /// the window, which has not been shown yet when a probe poses a flow,
    /// and a list that sized itself around a "closed" panel stayed that way.
    open: Cell<bool>,
    on_dismiss: RefCell<Option<Box<dyn Fn()>>>,
}

impl Panel {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            // The column's own inset, which is the rows' — a card that
            // opens under a list has to stand where the list stands.
            // These were 6 against the rows' 4, so both intervention
            // panels sat two pixels inside the column they belong to
            // (`near-miss.py`, which is the only way anyone was ever going
            // to see it). The bottom margin is vertical and not part of
            // that column.
            .margin_start(crate::filetree::SIDEBAR_ROW_MARGIN)
            .margin_end(crate::filetree::SIDEBAR_ROW_MARGIN)
            .margin_bottom(6)
            .visible(false)
            .build();
        Rc::new(Self {
            widget,
            open: Cell::new(false),
            on_dismiss: RefCell::new(None),
        })
    }

    /// What the header's X does. The owner decides what a dismissal means;
    /// the panel itself only knows how to go away (`close`).
    pub fn set_on_dismiss(&self, hook: impl Fn() + 'static) {
        *self.on_dismiss.borrow_mut() = Some(Box::new(hook));
    }

    /// Open with a title, replacing whatever was up, and hand back the
    /// content box for the flow to fill. `closable` adds the X; a flow with
    /// no cancel of its own — the Staged view's resting pane — has none.
    pub fn open(self: &Rc<Self>, title: &str, closable: bool) -> gtk::Box {
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_start(10);
        header.set_margin_end(if closable { 6 } else { 10 });
        let label = gtk::Label::builder()
            .label(title)
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build()
            .full_text_on_hover();
        header.append(&label);
        if closable {
            let close = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Cancel")
                .css_classes(["flat", "circular"])
                .build();
            let weak = Rc::downgrade(self);
            close.connect_clicked(move |_| {
                let Some(panel) = weak.upgrade() else { return };
                let hook = panel.on_dismiss.borrow();
                match hook.as_ref() {
                    Some(hook) => hook(),
                    None => {
                        drop(hook);
                        panel.close();
                    }
                }
            });
            header.append(&close);
        }
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(4);
        content.set_margin_bottom(10);
        content.set_margin_start(10);
        content.set_margin_end(10);
        self.widget.append(&header);
        self.widget.append(&content);
        self.widget.set_visible(true);
        self.open.set(true);
        content
    }

    /// Open as a BAR: the same card, one row, no header and no X.
    ///
    /// The backlog's interventions want this shape (David, 2026-09-09:
    /// "just have a single row with icon-based interventions … no close
    /// box or separate row with the selection count", and separately: "the
    /// toolbar for backlog interventions still doesn't match the one from
    /// files"). Matching means being the same component, not resembling
    /// it, so it is a second way in here rather than a lookalike box
    /// somewhere else — the card, its margins and its corners are stated
    /// once.
    ///
    /// Nothing to dismiss, either: what put the bar up is a check or a
    /// selection, and unchecking is how it goes away.
    pub fn open_bar(self: &Rc<Self>) -> gtk::Box {
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        content.set_margin_top(4);
        content.set_margin_bottom(4);
        content.set_margin_start(6);
        content.set_margin_end(6);
        self.widget.append(&content);
        self.widget.set_visible(true);
        self.open.set(true);
        content
    }

    /// Take the panel down, whatever it held; the list above gets its
    /// height back.
    pub fn close(&self) {
        self.open.set(false);
        self.widget.set_visible(false);
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
    }

    /// Whether a flow is up — what the list above sizes itself around.
    pub fn is_open(&self) -> bool {
        self.open.get()
    }
}
