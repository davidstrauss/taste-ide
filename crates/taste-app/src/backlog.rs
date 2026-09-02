//! The backlog: the workspace's issue queue, in the order the user put it
//! in, pinned under the environment panel.
//!
//! ENVIRONMENTS.md → "Issues: a ref, not a service". The queue used to be a
//! section of the console, one tab among an environment's log and shells,
//! which put a *workspace* fact inside the pane that is about the
//! *environment you are in*. It is a backlog — the list of what has not been
//! picked up yet — and a backlog belongs beside the fleet that picks things
//! up, not behind a tab in it.
//!
//! So it sits in the file-tree flank as the environment panel's sibling,
//! below it, collapsible. That placement is the whole argument for the
//! design:
//!
//! - **The two panels are one thought.** A row here says which environment
//!   claimed it; a row up there says what that environment is working on.
//!   Selecting a claimed issue selects its environment — the env↔issue link
//!   is navigable in both directions, and it is navigable because the two
//!   ends are eight pixels apart.
//! - **Collapsible, because it is not always the question.** The
//!   environment panel is permanent — it names where you are, and an
//!   indicator a panel can displace is not an indicator. The backlog is
//!   something you consult, so it folds away to its header and leaves the
//!   file tree the height.
//! - **The order is the user's to author.** Top first. Reordering writes the
//!   `order` file on `refs/taste/issues`
//!   ([`taste_git::GitWorkspace::issue_move`] for a step,
//!   [`taste_git::GitWorkspace::issue_reorder`] for a drag), which is one
//!   compare-and-swap over the whole list rather than N per-issue writes
//!   that can disagree about who is third.
//!
//! **How a row is moved: drag it, or ask its menu.** The rows carried six
//! hover buttons once, and every defect they had came from the same place —
//! a control that appears under the pointer, on a list that rebuilds itself
//! whenever anything writes. The rebuild disposed the very button mid-click,
//! so the reveal (`:hover`, `:focus-within`) died with it and the row that
//! swapped into that spot got the second click. Dragging has no such
//! problem: the gesture is the pointer's own, it ends before anything
//! rebuilds, and it says where the row is going by putting it there. The
//! menu is its keyboard-reachable twin, and it is summoned per row, built
//! per summoning, and dismissed before the write it starts — so nothing it
//! holds can be disposed under it. **Row identity travels as the issue id,
//! never a list index**, in the drag's payload and in the menu's closures
//! alike: an index means something different the instant the list moves,
//! which is precisely when these actions are used.
//!
//! **Every write is off the main thread and optimistic.** A move reorders
//! the rows on screen immediately and then does the git work in
//! `spawn_blocking`; the refresh that follows is what makes it true, and a
//! compare-and-swap that lost its race is re-read rather than re-applied —
//! the retry lives in `taste-git`, and what lands here is the winner's list.
//! A failure toasts AND puts the rows back itself: the refresh cannot be
//! relied on to do it, because a write that failed left git saying exactly
//! what it said before, and every reader of the queue is equality-guarded.
//!
//! Everything above [`BacklogPanel`] is pure and tested: what a row says,
//! which moves are available to it, where a drop lands it, and what the
//! header counts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_git::{Issue, IssueMove, IssueState};

use crate::envstrip::title_of;
use crate::fleet::{FleetRow, Light};

/// How many rows the panel shows before it scrolls inside itself. Smaller
/// than the environment panel's six: the fleet is the thing you must be
/// able to read at a glance, and the backlog is the thing you consult.
pub const VISIBLE_ROWS: i32 = 5;

/// One row's height, for the scroller's ceiling. Not a layout constraint —
/// rows size themselves — just the arithmetic behind "about five rows".
const ROW_HEIGHT: i32 = 30;

/// Who holds an issue, as a row draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The environment, when the fleet has a row for it. `None` means the
    /// assignee names something this workspace no longer has — which is a
    /// fact worth showing rather than hiding, so the label survives it.
    pub env: Option<EnvironmentId>,
    /// What to call it: the environment's display name, or the raw
    /// assignee string when there is nothing to look it up in.
    pub label: String,
    /// The claiming environment's traffic light, or [`Light::Unknown`] when
    /// the fleet does not have it. The absence of a status must not look
    /// like one.
    pub light: Light,
}

/// One issue, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub state: IssueState,
    /// The environment working on it, when someone claimed it.
    pub claim: Option<Claim>,
    /// When it last moved, in seconds since the epoch. In the tooltip
    /// rather than on the row: a backlog is read as an ordered list, and a
    /// column of ages would invite reading it as a sorted one.
    pub updated: i64,
}

impl Row {
    /// The row's tooltip: what it is, who has it, and what the state means.
    pub fn tooltip(&self) -> String {
        let mut text = format!("{} — {}", self.id, self.title);
        text.push_str(match self.state {
            IssueState::Open => "\nOpen.",
            IssueState::Closed => "\nClosed — its work is merged.",
        });
        match &self.claim {
            Some(claim) if claim.env.is_some() => {
                text.push_str(&format!("\nClaimed by {}.", claim.label));
            }
            Some(claim) => {
                text.push_str(&format!(
                    "\nClaimed by {}, which this workspace no longer has.",
                    claim.label
                ));
            }
            None => text.push_str("\nUnclaimed — any environment can pick it up."),
        }
        text.push_str(&format!(
            "\nLast changed {}.",
            crate::filetree::relative_age(self.updated)
        ));
        text
    }
}

/// Which of the four moves are available to the row at `index` of `len`.
///
/// A row already at the top cannot go up and is already at the top, so both
/// of its upward actions are dead; the same downward. They are computed
/// together rather than asked one at a time because the context menu shows
/// all four on every row: an item that vanishes teaches the reader a
/// different menu each time, where an insensitive one teaches them the row
/// they are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moves {
    pub up: bool,
    pub down: bool,
    pub top: bool,
    pub bottom: bool,
}

pub fn moves(index: usize, len: usize) -> Moves {
    let first = index == 0;
    let last = index + 1 >= len;
    Moves {
        up: !first,
        top: !first,
        down: !last,
        bottom: !last,
    }
}

/// Where a drag lands: the index `from` ends up at when it is dropped on
/// the row at `onto`, on that row's lower half if `below`.
///
/// The subtraction is the whole of it, and it is the classic place to get a
/// reordering wrong. The insertion point is expressed in the list *with the
/// dragged row still in it*, but the move takes the row out first — so
/// every position after it shifts down by one, and a drag downward has to
/// account for the hole it left behind. `None` means the row did not go
/// anywhere: dropped on itself, or on the gap it already occupies. A drag
/// that lands where it started is not a write.
pub fn drop_index(from: usize, onto: usize, below: bool) -> Option<usize> {
    let insert_at = if below { onto + 1 } else { onto };
    let to = if from < insert_at {
        insert_at.saturating_sub(1)
    } else {
        insert_at
    };
    (to != from).then_some(to)
}

/// The queue's rows, in the order it arrives in — which is the ref's own
/// order ([`taste_git::GitWorkspace::ordered_issues`]), never re-sorted
/// here. A second surface deciding what "top" means is how the list on
/// screen and the list in git come to disagree.
///
/// `fleet` is what turns an assignee slug into something a person reads: an
/// environment's display name and its traffic light, from the one assembly
/// every other surface renders.
pub fn rows(issues: &[Issue], fleet: &[FleetRow]) -> Vec<Row> {
    issues
        .iter()
        .map(|issue| Row {
            id: issue.id.clone(),
            title: issue.title.clone(),
            state: issue.state,
            updated: issue.updated,
            claim: issue.assignee.as_ref().map(|assignee| {
                match fleet.iter().find(|row| row.env.as_str() == assignee) {
                    Some(row) => Claim {
                        env: Some(row.env.clone()),
                        label: title_of(row),
                        light: row.light(),
                    },
                    None => Claim {
                        env: None,
                        label: assignee.clone(),
                        light: Light::Unknown,
                    },
                }
            }),
        })
        .collect()
}

/// The header's count, which is the queue's whole summary in one caption.
///
/// Closed issues are counted but not led with: the backlog is what is left
/// to do, and a header that said "6" of a queue with two open items in it
/// would be answering a question nobody asked.
pub fn summary(rows: &[Row]) -> String {
    let open = rows.iter().filter(|row| !row.state.is_closed()).count();
    match (rows.len(), open) {
        (0, _) => "empty".to_string(),
        (total, open) if open == total => format!("{open}"),
        (total, open) => format!("{open} · {} done", total - open),
    }
}

/// The glyph in a row's leading column: the checkbox that says whether this
/// is still work.
pub fn state_icon(state: IssueState) -> &'static str {
    match state {
        IssueState::Open => "checkbox-symbolic",
        IssueState::Closed => "checkbox-checked-symbolic",
    }
}

/// How the panel asks the window to aim the panes at a claiming
/// environment. The other half of the env↔issue link.
pub type SelectHook = Box<dyn Fn(EnvironmentId)>;
/// How the panel asks for the queue to be re-read after it wrote to it.
pub type RefreshHook = Box<dyn Fn()>;
/// How the panel says something went wrong, in the window's own toast.
pub type ToastHook = Box<dyn Fn(String)>;

/// What the composer is being used for. One surface, two jobs — filing a
/// new issue and retitling an existing one — because they ask for the same
/// two fields and a second composer would be a second set of bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Composing {
    New,
    Editing(String),
}

pub struct BacklogPanel {
    /// The panel itself: a permanent child at the very bottom of the
    /// file-tree pane, below the environment panel.
    pub widget: gtk::Box,
    revealer: gtk::Revealer,
    disclosure_icon: gtk::Image,
    count: gtk::Label,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    /// The inline composer, which is the whole of "no modals in the files
    /// area" for this panel.
    composer: gtk::Box,
    composer_title: gtk::Entry,
    composer_body: gtk::TextView,
    composer_heading: gtk::Label,
    composer_submit: gtk::Button,
    composing: RefCell<Option<Composing>>,
    /// The workspace root — the main checkout, whose ref the queue lives
    /// on. Every write re-discovers from it inside `spawn_blocking`, so
    /// nothing that is not `Send` ever crosses a thread.
    root: std::path::PathBuf,
    issues: RefCell<Vec<Issue>>,
    fleet: RefCell<Vec<FleetRow>>,
    /// What the list was built from — the rebuild guard, so a fleet tick
    /// that moved a disk figure does not rebuild a list that would read
    /// identically.
    shown: RefCell<Vec<Row>>,
    /// The ids on screen, in screen order, so a click resolves to a row
    /// without asking the list what index means.
    listed: RefCell<Vec<String>>,
    /// The row whose delete is asking for confirmation, if any. Inline,
    /// on the row — the files area takes no modal dialogs, and an issue is
    /// small enough that "are you sure" belongs where the pointer already
    /// is.
    confirming: RefCell<Option<String>>,
    /// Suppresses the selection hook while the panel is rebuilding.
    selecting: Cell<bool>,
    /// The open context menu, so a rebuild can close it before the row it
    /// is anchored to is disposed under it. The file tree tracks its own
    /// for the same reason and it is the same hazard: this list rebuilds
    /// whenever anything writes.
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
    /// A write is in flight: a second one is refused rather than queued
    /// behind the first, since both would be compare-and-swaps on one ref.
    writing: Cell<bool>,
    on_select: RefCell<Option<SelectHook>>,
    on_refresh: RefCell<Option<RefreshHook>>,
    on_toast: RefCell<Option<ToastHook>>,
}

impl BacklogPanel {
    pub fn new(root: std::path::PathBuf) -> Rc<Self> {
        // The header is the panel when it is collapsed, so it carries all
        // three things: what this is, how much of it there is, and the one
        // action that makes more.
        let disclosure_icon = gtk::Image::builder()
            .icon_name("pan-down-symbolic")
            .css_classes(["dim-label"])
            .pixel_size(14)
            .build();
        let disclosure = gtk::Button::builder()
            .child(&disclosure_icon)
            .css_classes(["flat", "circular", "backlog-disclose"])
            .tooltip_text("Show or hide the backlog")
            .build();
        let title = gtk::Label::builder()
            .label("Backlog")
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .build();
        let count = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "numeric"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        let add = gtk::Button::builder()
            // Built by hand rather than set by name so it can be dimmed,
            // exactly as the environment panel's + is: at full strength a
            // white glyph is the brightest thing in the flank.
            .child(
                &gtk::Image::builder()
                    .icon_name("list-add-symbolic")
                    .css_classes(["dim-label"])
                    .pixel_size(14)
                    .build(),
            )
            .css_classes(["flat", "circular", "env-new"])
            .tooltip_text("Write a new issue. It goes to the top of nothing — new issues land in id order at the bottom until you move them.")
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .css_classes(["env-panel-header"])
            .build();
        header.append(&disclosure);
        header.append(&title);
        header.append(&count);
        header.append(&add);

        // --- the composer: no modals in the files area ---------------------
        let composer_heading = gtk::Label::builder()
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .build();
        // Both fields wear `.composer-field` (main.rs): one wash, one
        // radius, one focus ring. They ask for two halves of one issue, so
        // they are peers — a themed entry above a `.card` slab was two
        // widgets that happened to be adjacent.
        let composer_title = gtk::Entry::builder()
            .placeholder_text("Title")
            .activates_default(false)
            .css_classes(["composer-field"])
            .build();
        let composer_body = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            // The body's inset is stated here, once, and matches the
            // padding the theme gives the entry above it — the two are
            // only siblings if their text starts on the same line.
            .top_margin(7)
            .bottom_margin(7)
            .left_margin(9)
            .right_margin(9)
            .height_request(52)
            .build();
        let body_frame = gtk::ScrolledWindow::builder()
            .child(&composer_body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["composer-field"])
            .height_request(52)
            .build();
        let composer_cancel = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat"])
            .build();
        let composer_submit = gtk::Button::builder()
            .label("File")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let composer_actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        composer_actions.append(&composer_cancel);
        composer_actions.append(&composer_submit);
        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["card", "backlog-composer"])
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(6)
            .visible(false)
            .build();
        composer.append(&composer_heading);
        composer.append(&composer_title);
        composer.append(&body_frame);
        composer.append(&composer_actions);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "backlog-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(VISIBLE_ROWS * ROW_HEIGHT)
            .build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&composer);
        body.append(&scroller);
        let revealer = gtk::Revealer::builder()
            .child(&body)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(140)
            .reveal_child(true)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("backlog-panel");
        // A probe target of its own: `filetree.backlog` (ui_probe.rs).
        widget.set_widget_name("backlog");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&header);
        widget.append(&revealer);

        let panel = Rc::new(Self {
            widget,
            revealer: revealer.clone(),
            disclosure_icon,
            count: count.clone(),
            scroller,
            list: list.clone(),
            composer,
            composer_title: composer_title.clone(),
            composer_body: composer_body.clone(),
            composer_heading,
            composer_submit: composer_submit.clone(),
            composing: RefCell::new(None),
            root,
            issues: RefCell::new(Vec::new()),
            fleet: RefCell::new(Vec::new()),
            shown: RefCell::new(Vec::new()),
            listed: RefCell::new(Vec::new()),
            confirming: RefCell::new(None),
            selecting: Cell::new(false),
            open_menu: RefCell::new(None),
            writing: Cell::new(false),
            on_select: RefCell::new(None),
            on_refresh: RefCell::new(None),
            on_toast: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&panel);
            disclosure.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.set_expanded(!panel.revealer.reveals_child());
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            add.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.open_composer(Composing::New);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_cancel.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.close_composer();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_submit.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.submit_composer();
                }
            });
        }
        {
            // An issue needs a title; the button says so by being dead
            // until there is one.
            let submit = composer_submit.clone();
            composer_title.connect_changed(move |entry| {
                submit.set_sensitive(!entry.text().trim().is_empty());
            });
        }
        {
            // Enter in the title files it — the composer is two fields and
            // a body most issues do not need.
            let weak = Rc::downgrade(&panel);
            composer_title.connect_activate(move |entry| {
                if entry.text().trim().is_empty() {
                    return;
                }
                if let Some(panel) = weak.upgrade() {
                    panel.submit_composer();
                }
            });
        }
        {
            // Escape closes the composer, from either field.
            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&panel);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    if let Some(panel) = weak.upgrade() {
                        panel.close_composer();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
            panel.composer.add_controller(keys);
        }
        {
            // Selecting a claimed issue selects its environment. The link
            // is navigable both ways, and this is the way that starts here.
            let weak = Rc::downgrade(&panel);
            list.connect_row_activated(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                if panel.selecting.get() {
                    return;
                }
                let index = row.index();
                if index < 0 {
                    return;
                }
                let id = panel.listed.borrow().get(index as usize).cloned();
                let Some(id) = id else { return };
                let env = panel
                    .shown
                    .borrow()
                    .iter()
                    .find(|row| row.id == id)
                    .and_then(|row| row.claim.as_ref())
                    .and_then(|claim| claim.env.clone());
                if let Some(env) = env {
                    if let Some(hook) = panel.on_select.borrow().as_ref() {
                        hook(env);
                    }
                }
            });
        }

        panel.render();
        panel
    }

    pub fn set_on_select(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_refresh(&self, hook: impl Fn() + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_toast(&self, hook: impl Fn(String) + 'static) {
        *self.on_toast.borrow_mut() = Some(Box::new(hook));
    }

    /// The queue, in the ref's own order. Equality-guarded: this lands on
    /// every git-status change, and a queue that did not move costs one
    /// comparison.
    pub fn set_issues(self: &Rc<Self>, issues: &[Issue]) {
        if self.issues.borrow().as_slice() == issues {
            return;
        }
        *self.issues.borrow_mut() = issues.to_vec();
        self.render();
    }

    /// The fleet, for the claim column. Only the facts a claim renders are
    /// consulted, so a tick that moved a spend figure rebuilds nothing.
    pub fn set_fleet(self: &Rc<Self>, fleet: &[FleetRow]) {
        if self.fleet.borrow().as_slice() == fleet {
            return;
        }
        *self.fleet.borrow_mut() = fleet.to_vec();
        self.render();
    }

    /// Fold the panel away, or bring it back.
    pub fn set_expanded(self: &Rc<Self>, expanded: bool) {
        self.revealer.set_reveal_child(expanded);
        self.disclosure_icon.set_icon_name(Some(if expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }));
        if !expanded {
            self.close_composer();
        }
    }

    // --- rendering -------------------------------------------------------

    fn render(self: &Rc<Self>) {
        let rows = rows(&self.issues.borrow(), &self.fleet.borrow());
        if *self.shown.borrow() == rows && !self.list_is_empty_but_should_not_be(&rows) {
            return;
        }
        // Everything below disposes every row. An open menu is anchored to
        // one of them, so it goes first.
        self.close_context_menu();
        self.count.set_label(&summary(&rows));

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if rows.is_empty() {
            let empty = gtk::Label::builder()
                .label("Nothing written down")
                .css_classes(["dim-label", "caption"])
                .xalign(0.0)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(12)
                .margin_end(12)
                .wrap(true)
                .tooltip_text(
                    "Issues are how work outlives a conversation: write one and any \
                     environment can pick it up. An agent that finishes one cannot close \
                     it until its branch is merged.",
                )
                .build();
            let row = gtk::ListBoxRow::builder()
                .child(&empty)
                .activatable(false)
                .selectable(false)
                .build();
            self.list.append(&row);
        }
        let mut listed: Vec<String> = Vec::new();
        for row in rows.iter() {
            self.list.append(&self.build_row(row));
            listed.push(row.id.clone());
        }
        let count = listed.len() as i32;
        self.scroller
            .set_min_content_height(count.clamp(1, VISIBLE_ROWS) * ROW_HEIGHT);
        *self.listed.borrow_mut() = listed;
        *self.shown.borrow_mut() = rows;
        self.selecting.set(true);
        self.list.select_row(gtk::ListBoxRow::NONE);
        self.selecting.set(false);
    }

    /// The first render has an empty `shown` and an empty list, which the
    /// equality guard would take for "nothing to do".
    fn list_is_empty_but_should_not_be(&self, rows: &[Row]) -> bool {
        self.list.first_child().is_none() && !rows.is_empty()
    }

    fn build_row(self: &Rc<Self>, row: &Row) -> gtk::ListBoxRow {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        box_.set_margin_top(2);
        box_.set_margin_bottom(2);
        box_.set_margin_start(8);
        box_.set_margin_end(4);

        box_.append(
            &gtk::Image::builder()
                .icon_name(state_icon(row.state))
                .css_classes(["dim-label"])
                .pixel_size(13)
                .valign(gtk::Align::Center)
                .build(),
        );

        let label = gtk::Label::builder()
            .label(&row.title)
            .xalign(0.0)
            .hexpand(true)
            // A long title must not widen the tree: the pane's minimum
            // decides whether GNOME will tile this window.
            .ellipsize(gtk::pango::EllipsizeMode::End)
            // Small enough that a long title cannot widen the flank — the
            // pane's minimum decides whether GNOME will tile this window —
            // and the label hexpands, so on a real pane it takes whatever
            // room is going.
            .max_width_chars(12)
            .build();
        if row.state.is_closed() {
            label.add_css_class("dim-label");
        }
        box_.append(&label);

        // The claim: who is working on this, dim, with that environment's
        // own traffic light — the same dot the panel above draws, from the
        // same mapping, so the two panels cannot disagree about whether an
        // environment is up.
        if let Some(claim) = &row.claim {
            let claim_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .valign(gtk::Align::Center)
                .css_classes(["backlog-claim"])
                .build();
            claim_box.append(
                &gtk::Box::builder()
                    .css_classes(["env-dot", claim.light.css()])
                    .valign(gtk::Align::Center)
                    .build(),
            );
            claim_box.append(
                &gtk::Label::builder()
                    .label(&claim.label)
                    .css_classes(["caption", "dim-label"])
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .max_width_chars(8)
                    .build(),
            );
            box_.append(&claim_box);
        }

        // Asking to delete is the one thing that changes a row's shape, and
        // it is inline because the files area takes no modal dialogs (the
        // intervention-panel convention).
        //
        // It arrives from the context menu, which is dismissed by the time
        // this is drawn. That is the fix for the worst defect the hover
        // strip had: the confirmation used to appear in the exact slot the
        // delete BUTTON had just occupied, wearing the same trash glyph, so
        // a second click that had not moved — muscle memory, or a
        // double-click — destroyed the issue having asked nothing.
        if self.confirming.borrow().as_deref() == Some(row.id.as_str()) {
            let actions = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .valign(gtk::Align::Center)
                .css_classes(["backlog-confirm"])
                .build();
            actions.append(
                &gtk::Label::builder()
                    .label("Delete?")
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let cancel = self.icon_button("edit-undo-symbolic", "Keep it", &["flat"]);
            {
                let weak = Rc::downgrade(self);
                cancel.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        *panel.confirming.borrow_mut() = None;
                        panel.rerender();
                    }
                });
            }
            let confirm = self.icon_button(
                "user-trash-symbolic",
                "Delete this issue for good. Closing is how work ends; deleting is how a \
                 mistake is unmade.",
                &["flat", "destructive-action"],
            );
            {
                let weak = Rc::downgrade(self);
                let id = row.id.clone();
                confirm.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        *panel.confirming.borrow_mut() = None;
                        panel.delete(&id);
                    }
                });
            }
            actions.set_sensitive(!self.writing.get());
            actions.append(&cancel);
            actions.append(&confirm);
            box_.append(&actions);
        }

        let widget = gtk::ListBoxRow::builder()
            .child(&box_)
            // Only a claimed issue has somewhere to go; an unclaimed one
            // activating to nothing would be a row that lies about being a
            // link.
            .activatable(row.claim.as_ref().is_some_and(|c| c.env.is_some()))
            .build();

        // --- reordering, gesture one: drag the row where you want it ------
        //
        // Move semantics, and only within this list: the payload is an
        // issue id, which means nothing to anything outside this panel, and
        // the only things that accept it are the other rows of this
        // backlog. Escape cancels, because that is GTK's own contract for a
        // drag in flight and nothing here overrides it.
        //
        // Every closure below holds the row WEAKLY. A controller is owned
        // by the widget it is added to, so a strong capture here is a
        // reference cycle — and this list rebuilds itself on every write,
        // which would make it a leak of one row per gesture per write.
        {
            let source = gtk::DragSource::builder()
                .actions(gtk::gdk::DragAction::MOVE)
                .build();
            let id = row.id.clone();
            source.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&id.to_value()))
            });
            let dragged = widget.downgrade();
            source.connect_drag_begin(move |source, _| {
                // The row is what is under the pointer, so the row is what
                // should follow it.
                let Some(row) = dragged.upgrade() else { return };
                source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 0);
                row.add_css_class("dragging");
            });
            let dragged = widget.downgrade();
            source.connect_drag_end(move |_, _, _| {
                if let Some(row) = dragged.upgrade() {
                    row.remove_css_class("dragging");
                }
            });
            let dragged = widget.downgrade();
            source.connect_drag_cancel(move |_, _, _| {
                if let Some(row) = dragged.upgrade() {
                    row.remove_css_class("dragging");
                }
                false
            });
            widget.add_controller(source);
        }

        // --- and where it would land ---------------------------------------
        //
        // Which half of the row the pointer is in decides which side of it
        // the drop lands on, and the indicator says so before the button
        // comes up. A list whose drop point is a guess is a list that gets
        // reordered wrong once and then distrusted.
        {
            let target =
                gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
            {
                let weak = Rc::downgrade(self);
                let onto = widget.downgrade();
                target.connect_motion(move |_, _, y| {
                    if let Some(panel) = weak.upgrade() {
                        panel.clear_drop_marks();
                    }
                    if let Some(row) = onto.upgrade() {
                        row.add_css_class(mark_for(&row, y));
                    }
                    gtk::gdk::DragAction::MOVE
                });
            }
            {
                let weak = Rc::downgrade(self);
                target.connect_leave(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        panel.clear_drop_marks();
                    }
                });
            }
            {
                let weak = Rc::downgrade(self);
                let onto_id = row.id.clone();
                let onto = widget.downgrade();
                target.connect_drop(move |_, value, _, y| {
                    let (Some(panel), Some(row)) = (weak.upgrade(), onto.upgrade()) else {
                        return false;
                    };
                    panel.clear_drop_marks();
                    let Ok(dragged) = value.get::<String>() else {
                        return false;
                    };
                    panel.drop_onto(&dragged, &onto_id, mark_for(&row, y) == "drop-below");
                    true
                });
            }
            widget.add_controller(target);
        }

        // --- reordering, gesture two: the row's own menu --------------------
        //
        // The keyboard-reachable twin of the drag, and the home of Edit and
        // Delete. Built fresh each time it is summoned, against the row's
        // CURRENT position — which is what lets an item honestly say that
        // Move Up is unavailable here.
        {
            let context = gtk::GestureClick::builder().button(3).build();
            let weak = Rc::downgrade(self);
            let id = row.id.clone();
            let anchor = widget.downgrade();
            context.connect_pressed(move |_, _, x, y| {
                if let (Some(panel), Some(row)) = (weak.upgrade(), anchor.upgrade()) {
                    panel.show_context_menu(&row, &id, Some((x, y)));
                }
            });
            widget.add_controller(context);
        }
        {
            // Menu key and Shift+F10, on the focused row: an action
            // reachable only by pointer is not reachable.
            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(self);
            let id = row.id.clone();
            let anchor = widget.downgrade();
            keys.connect_key_pressed(move |_, key, _, state| {
                let asked = key == gtk::gdk::Key::Menu
                    || (key == gtk::gdk::Key::F10
                        && state.contains(gtk::gdk::ModifierType::SHIFT_MASK));
                if !asked {
                    return glib::Propagation::Proceed;
                }
                if let (Some(panel), Some(row)) = (weak.upgrade(), anchor.upgrade()) {
                    panel.show_context_menu(&row, &id, None);
                }
                glib::Propagation::Stop
            });
            widget.add_controller(keys);
        }
        widget.set_tooltip_text(Some(&row.tooltip()));
        widget
    }

    /// The row's menu: the four moves, then Edit and Delete in a section of
    /// their own — a destructive item does not belong in the same group as
    /// the ones that only change an order.
    ///
    /// Built per summoning, and every item's closure holds the ISSUE ID.
    /// The list is rebuilt by every write and by every refresh, so an index
    /// captured here would name a different row by the time it was used —
    /// which is exactly the defect the buttons this replaced had. The
    /// position is looked up now, and only to decide what is available.
    fn show_context_menu(
        self: &Rc<Self>,
        anchor: &gtk::ListBoxRow,
        id: &str,
        at: Option<(f64, f64)>,
    ) {
        use gtk::gio;

        let at_index = self.issues.borrow().iter().position(|issue| issue.id == id);
        let Some(index) = at_index else { return };
        let available = moves(index, self.issues.borrow().len());

        let actions = gio::SimpleActionGroup::new();
        let add_action = |name: &str, enabled: bool, callback: Box<dyn Fn() + 'static>| {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(enabled);
            action.connect_activate(move |_, _| callback());
            actions.add_action(&action);
        };

        let menu = gio::Menu::new();
        let move_section = gio::Menu::new();
        for (name, label, direction, live) in [
            ("move-top", "Move to Top", IssueMove::Top, available.top),
            ("move-up", "Move Up", IssueMove::Up, available.up),
            ("move-down", "Move Down", IssueMove::Down, available.down),
            (
                "move-bottom",
                "Move to Bottom",
                IssueMove::Bottom,
                available.bottom,
            ),
        ] {
            move_section.append(Some(label), Some(&format!("row.{name}")));
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                name,
                live,
                Box::new(move || panel.move_issue(&id, direction)),
            );
        }
        menu.append_section(None, &move_section);

        let edit_section = gio::Menu::new();
        edit_section.append(Some("Edit…"), Some("row.edit"));
        edit_section.append(Some("Delete…"), Some("row.delete"));
        menu.append_section(None, &edit_section);
        {
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                "edit",
                true,
                Box::new(move || panel.open_composer(Composing::Editing(id.clone()))),
            );
        }
        {
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                "delete",
                true,
                Box::new(move || {
                    *panel.confirming.borrow_mut() = Some(id.clone());
                    panel.rerender();
                }),
            );
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        // A probe target of its own: `filetree.backlog-menu` (ui_probe.rs).
        // A popover is its own native surface, so a shot of the pane
        // BEHIND it does not contain it — the menu has to be named to be
        // photographed.
        popover.set_widget_name("backlog-menu");
        popover.insert_action_group("row", Some(&actions));
        popover.set_parent(anchor);
        popover.set_has_arrow(false);
        if let Some((x, y)) = at {
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        }
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        // Tracked so a rebuild can close it before the row it hangs off is
        // disposed under it — the file tree tracks its own for the same
        // reason, and this list rebuilds far more often.
        *self.open_menu.borrow_mut() = Some(popover.downgrade());
        popover.popup();
    }

    /// Take down an open row menu. A rebuild disposes the row it is
    /// anchored to, and a popover whose anchor died is a GTK warning at
    /// best and a menu acting on a vanished row at worst.
    fn close_context_menu(&self) {
        if let Some(popover) = self.open_menu.borrow_mut().take().and_then(|w| w.upgrade()) {
            popover.popdown();
        }
    }

    /// Clear every row's drop indicator. Cheap (the list is capped at what
    /// fits in a flank) and unconditional, because a drag that left one
    /// behind would point at a gap the row is not going to land in.
    fn clear_drop_marks(&self) {
        let mut child = self.list.first_child();
        while let Some(row) = child {
            row.remove_css_class("drop-above");
            row.remove_css_class("drop-below");
            child = row.next_sibling();
        }
    }

    /// Commit a drag: `dragged` lands on the side of `onto` the pointer was
    /// nearest. A drop on itself, or into the gap it already fills, is not
    /// a write — [`drop_index`] is what decides that, and it says `None`.
    fn drop_onto(self: &Rc<Self>, dragged: &str, onto: &str, below: bool) {
        let issues = self.issues.borrow();
        let (Some(from), Some(at)) = (
            issues.iter().position(|issue| issue.id == dragged),
            issues.iter().position(|issue| issue.id == onto),
        ) else {
            return;
        };
        drop(issues);
        let Some(to) = drop_index(from, at, below) else {
            return;
        };
        let was = self.reorder_to(dragged, to);
        let id = dragged.to_string();
        self.write(was, move |git| git.issue_reorder(&id, to).map(|_| ()));
    }

    fn icon_button(&self, icon: &str, tooltip: &str, classes: &[&str]) -> gtk::Button {
        gtk::Button::builder()
            .child(&gtk::Image::builder().icon_name(icon).pixel_size(12).build())
            .css_classes(classes.to_vec())
            .valign(gtk::Align::Center)
            .tooltip_text(tooltip)
            .build()
    }

    /// Rebuild the list even though the model did not move — the delete
    /// confirmation and the in-flight guard are row state, not issue state.
    fn rerender(self: &Rc<Self>) {
        self.shown.borrow_mut().clear();
        self.render();
    }

    // --- the composer ----------------------------------------------------

    fn open_composer(self: &Rc<Self>, what: Composing) {
        self.set_expanded(true);
        match &what {
            Composing::New => {
                self.composer_heading.set_label("New issue");
                self.composer_submit.set_label("File");
                self.composer_title.set_text("");
                self.composer_body.buffer().set_text("");
            }
            Composing::Editing(id) => {
                let issues = self.issues.borrow();
                let Some(issue) = issues.iter().find(|issue| &issue.id == id) else {
                    return;
                };
                self.composer_heading
                    .set_label(&format!("Edit {}", issue.id));
                self.composer_submit.set_label("Save");
                self.composer_title.set_text(&issue.title);
                self.composer_body.buffer().set_text(&issue.body);
            }
        }
        self.composer_submit
            .set_sensitive(!self.composer_title.text().trim().is_empty());
        *self.composing.borrow_mut() = Some(what);
        self.composer.set_visible(true);
        self.composer_title.grab_focus();
    }

    fn close_composer(self: &Rc<Self>) {
        self.composer.set_visible(false);
        *self.composing.borrow_mut() = None;
    }

    fn submit_composer(self: &Rc<Self>) {
        let Some(what) = self.composing.borrow().clone() else {
            return;
        };
        let title = self.composer_title.text().trim().to_string();
        if title.is_empty() {
            return;
        }
        let buffer = self.composer_body.buffer();
        let body = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        self.close_composer();
        match what {
            Composing::New => self.create(title, body),
            Composing::Editing(id) => self.edit(id, title, body),
        }
    }

    // --- writes ----------------------------------------------------------
    //
    // Every one of these is the same shape: go insensitive, do the git work
    // in `spawn_blocking` against a workspace discovered on that thread,
    // then ask for a refresh. The compare-and-swap retry lives in
    // `taste-git` and re-reads the winner's list, so what lands here is
    // always what is really on the ref — which is why the optimistic half
    // is safe: the refresh is the correction.

    fn move_issue(self: &Rc<Self>, id: &str, direction: IssueMove) {
        // Optimistic: reorder the rows on screen now. The refresh that
        // follows is what makes it true, and a lost race comes back as the
        // winner's order rather than as a flicker of this one.
        let issues = self.issues.borrow();
        let Some(at) = issues.iter().position(|issue| issue.id == id) else {
            return;
        };
        let to = match direction {
            IssueMove::Up => at.saturating_sub(1),
            IssueMove::Down => (at + 1).min(issues.len().saturating_sub(1)),
            IssueMove::Top => 0,
            IssueMove::Bottom => issues.len().saturating_sub(1),
        };
        drop(issues);
        if to == at {
            return;
        }
        let was = self.reorder_to(id, to);
        let id = id.to_string();
        self.write(was, move |git| git.issue_move(&id, direction).map(|_| ()));
    }

    /// Move a row in the list we are already showing, so the gesture lands
    /// before the git write does. Nothing is persisted here — this is the
    /// half-second before the refresh — so it answers with the order it
    /// replaced, which is what a failed write has to be put back to.
    fn reorder_to(self: &Rc<Self>, id: &str, to: usize) -> Option<Vec<Issue>> {
        let mut issues = self.issues.borrow_mut();
        let at = issues.iter().position(|issue| issue.id == id)?;
        if at == to || to >= issues.len() {
            return None;
        }
        let was = issues.clone();
        let issue = issues.remove(at);
        issues.insert(to, issue);
        drop(issues);
        self.render();
        Some(was)
    }

    fn delete(self: &Rc<Self>, id: &str) {
        let id = id.to_string();
        self.write(None, move |git| git.issue_delete(&id));
    }

    fn create(self: &Rc<Self>, title: String, body: String) {
        self.write(None, move |git| {
            // The reporter is the user's own checkout: this composer is in
            // the user's window, and attributing it to an agent's
            // environment would be a lie the issue carries forever.
            git.issue_create(&title, &body, &[], "primary").map(|_| ())
        });
    }

    fn edit(self: &Rc<Self>, id: String, title: String, body: String) {
        self.write(None, move |git| {
            let target = git.issue_target_branch();
            let change = taste_git::IssueChange {
                title: Some(title),
                body: Some(body),
                ..Default::default()
            };
            git.issue_update(&id, &change, &target, "primary")
                .map(|_| ())
        });
    }

    /// The one write path. Off the main thread, one at a time, and every
    /// outcome ends in a refresh.
    ///
    /// `revert_to` is the order the rows had before an optimistic reorder
    /// moved them, and a failure puts it back **here** rather than trusting
    /// the refresh to. The refresh cannot do it: a write that failed left
    /// git saying exactly what it said before, and every reader of the
    /// queue — this panel and the console that feeds it — is
    /// equality-guarded, so nothing announces and the row stays where the
    /// user's gesture optimistically put it. Forever, and wrongly.
    fn write<F>(self: &Rc<Self>, revert_to: Option<Vec<Issue>>, op: F)
    where
        F: FnOnce(&taste_git::GitWorkspace) -> anyhow::Result<()> + Send + 'static,
    {
        if self.writing.get() {
            // Refusing is right — two compare-and-swaps on one ref is how
            // an order gets decided by a race — but refusing SILENTLY is
            // not: this used to swallow a filed issue whole, composer
            // already closed, with nothing on screen to say so.
            if let Some(toast) = self.on_toast.borrow().as_ref() {
                toast(
                    "The backlog is still saving the last change — try again in a moment.".into(),
                );
            }
            if let Some(order) = revert_to {
                *self.issues.borrow_mut() = order;
                self.render();
            }
            return;
        }
        self.writing.set(true);
        self.rerender();
        let root = self.root.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = taste_git::GitWorkspace::discover(&root)
                    .ok_or_else(|| anyhow::anyhow!("this workspace is not a git repository"))?;
                op(&git)
            });
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(e) => Err(anyhow::anyhow!("the write did not finish: {e}")),
            };
            let Some(panel) = weak.upgrade() else { return };
            panel.writing.set(false);
            if let Err(e) = outcome {
                if let Some(toast) = panel.on_toast.borrow().as_ref() {
                    toast(format!("{e:#}"));
                }
                // The rows moved on a promise this write did not keep.
                if let Some(order) = revert_to {
                    *panel.issues.borrow_mut() = order;
                }
            }
            // Always: the ref is the truth, and the optimistic rows are
            // only ever a guess at it.
            if let Some(refresh) = panel.on_refresh.borrow().as_ref() {
                refresh();
            }
            panel.rerender();
        });
    }

    /// TASTE_PROBE_CHECK only: open one row's context menu, as a
    /// right-click would.
    ///
    /// What is fabricated is the summoning, and only that: the menu, its
    /// sections, and which of its items this row can actually use are the
    /// real ones, built by the real code path. A drag cannot be
    /// photographed mid-flight, so the menu is what a still frame can show
    /// of what the rows DO — and it is the half a keyboard uses anyway.
    pub fn seed_menu_for_probe(self: &Rc<Self>, id: &str) {
        let index = self.listed.borrow().iter().position(|row| row == id);
        let Some(row) = index.and_then(|i| self.list.row_at_index(i as i32)) else {
            return;
        };
        self.show_context_menu(&row, id, None);
    }

    /// TASTE_PROBE_CHECK only: open the composer with something typed in
    /// it, so the two fields can be LOOKED at side by side.
    ///
    /// Through the same door the `+` button uses, and through the real
    /// setters: a fixture that poked the widgets directly would keep
    /// photographing a composer the app had stopped building that way.
    pub fn seed_composer_for_probe(self: &Rc<Self>) {
        self.open_composer(Composing::New);
        self.composer_title
            .set_text("Relocation waits for the container");
        self.composer_body.buffer().set_text(
            "Opening a chat in a stopped environment must not try to \
             relocate: the agent starts outside and moves in when the \
             container comes up.",
        );
        self.composer_submit.set_sensitive(true);
    }
}

/// Which edge of `row` a pointer at `y` is asking to drop against. The
/// halfway line, so every point in the list belongs to exactly one gap and
/// the indicator never has to guess.
fn mark_for(row: &gtk::ListBoxRow, y: f64) -> &'static str {
    if y * 2.0 >= f64::from(row.height()) {
        "drop-below"
    } else {
        "drop-above"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{assemble, EnvFacts, Spend};
    use taste_core::state::WorkspaceState;
    use taste_devcontainer::SupervisorState;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn issue(id: &str, title: &str, state: IssueState, assignee: Option<&str>) -> Issue {
        Issue {
            id: id.into(),
            title: title.into(),
            state,
            reporter: "primary".into(),
            assignee: assignee.map(str::to_string),
            created: 0,
            updated: 0,
            labels: Vec::new(),
            links: Vec::new(),
            body: String::new(),
            comments: Vec::new(),
        }
    }

    fn facts(slug: &str, state: SupervisorState) -> EnvFacts {
        EnvFacts {
            env: env(slug),
            state,
            authority: taste_core::ConfigAuthority::Project,
            pending_rebuild: false,
            chat: None,
            git: None,
            disk: None,
            spend: Spend::default(),
            shells: 0,
            review: taste_core::ReviewState::Working,
            working_on: Vec::new(),
        }
    }

    fn running() -> SupervisorState {
        SupervisorState::Running {
            container_id: "abc123".into(),
        }
    }

    fn fleet() -> Vec<FleetRow> {
        let mut state = WorkspaceState::default();
        state.set_environment_name(&env("spry-2"), Some("the refactor"));
        assemble(
            vec![
                facts("primary", running()),
                facts("calm-1", running()),
                facts("spry-2", SupervisorState::Stopped),
            ],
            &state,
            &[],
        )
    }

    /// The join: an assignee slug becomes the name the user reads and the
    /// light the panel above draws — from the fleet's own assembly, never
    /// from a second read of podman.
    #[test]
    fn a_claimed_row_names_the_environment_and_carries_its_light() {
        let rows = rows(
            &[
                issue(
                    "i-0001",
                    "The parser drops commas",
                    IssueState::Open,
                    Some("calm-1"),
                ),
                issue("i-0002", "Rename the strip", IssueState::Open, None),
                issue(
                    "i-0003",
                    "Ship the gauge",
                    IssueState::Closed,
                    Some("spry-2"),
                ),
            ],
            &fleet(),
        );

        let claimed = rows[0].claim.as_ref().unwrap();
        assert_eq!(claimed.env, Some(env("calm-1")));
        assert_eq!(claimed.label, "calm-1");
        assert_eq!(claimed.light, Light::Green);
        assert!(rows[0].tooltip().contains("Claimed by calm-1"));

        assert_eq!(rows[1].claim, None);
        assert!(rows[1].tooltip().contains("Unclaimed"));

        // A renamed environment shows the name the user gave it, and a
        // stopped one shows that it is stopped.
        let closed = rows[2].claim.as_ref().unwrap();
        assert_eq!(closed.label, "the refactor");
        assert_eq!(closed.light, Light::Red);
        assert!(rows[2].tooltip().contains("Closed"));
    }

    /// An assignee the fleet does not have is a fact, not a blank. The
    /// label survives; the light does not pretend to know anything.
    #[test]
    fn a_claim_by_an_environment_that_is_gone_still_says_who_had_it() {
        let rows = rows(
            &[issue(
                "i-0001",
                "Left behind",
                IssueState::Open,
                Some("gone-9"),
            )],
            &fleet(),
        );
        let claim = rows[0].claim.as_ref().unwrap();
        assert_eq!(claim.env, None, "nothing to select");
        assert_eq!(claim.label, "gone-9");
        assert_eq!(claim.light, Light::Unknown);
        assert!(rows[0].tooltip().contains("no longer has"));
    }

    /// The order that arrives is the order that renders. The ref decides
    /// what "top" means; a second surface sorting it is how the list on
    /// screen and the list in git come to disagree.
    #[test]
    fn the_rows_keep_the_order_they_arrive_in() {
        let ordered = [
            issue("i-0009", "Last filed, first wanted", IssueState::Open, None),
            issue("i-0001", "Filed first", IssueState::Open, None),
            issue("i-0004", "In between", IssueState::Closed, None),
        ];
        let rows = rows(&ordered, &[]);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["i-0009", "i-0001", "i-0004"]
        );
    }

    /// Where a drag lands, which is the arithmetic every reorderable list
    /// gets wrong once. The insertion point is read off the list WITH the
    /// dragged row still in it; the move takes it out first.
    #[test]
    fn a_drop_lands_in_the_gap_it_was_aimed_at() {
        // Four rows, a..d. Dragging a (0) below b (1): a comes out, b
        // slides up to 0, and a goes back at 1 — b, a, c, d.
        assert_eq!(drop_index(0, 1, true), Some(1));
        // Dragging d (3) above b (1) puts it at 1 — nothing before it
        // moved, so no correction applies.
        assert_eq!(drop_index(3, 1, false), Some(1));
        // The two ends, reached from the far one.
        assert_eq!(drop_index(3, 0, false), Some(0), "to the very top");
        assert_eq!(drop_index(0, 3, true), Some(3), "to the very bottom");
    }

    /// A drag that did not move the row is not a write. Three ways to
    /// express the same non-move, and all of them have to be silent: a
    /// spurious `issue_reorder` is a commit on the issues ref, and a commit
    /// is what other windows and the agent's own reads react to.
    #[test]
    fn a_drop_that_changes_nothing_is_not_a_write() {
        // Dropped on itself, either half.
        assert_eq!(drop_index(2, 2, false), None);
        assert_eq!(drop_index(2, 2, true), None);
        // Dropped into the gap it already fills: just below its own
        // predecessor, or just above its own successor.
        assert_eq!(drop_index(2, 1, true), None);
        assert_eq!(drop_index(2, 3, false), None);
    }

    /// The four moves, and the two rows where half of them are unavailable.
    /// They are computed together because the menu shows all four on every
    /// row: an item that vanishes teaches a different menu each time.
    #[test]
    fn the_ends_of_the_list_cannot_move_further_out() {
        let top = moves(0, 4);
        assert!(!top.up && !top.top, "already there");
        assert!(top.down && top.bottom);

        let middle = moves(1, 4);
        assert!(middle.up && middle.top && middle.down && middle.bottom);

        let bottom = moves(3, 4);
        assert!(bottom.up && bottom.top);
        assert!(!bottom.down && !bottom.bottom);

        // A queue of one is both ends at once, and nothing can move.
        let only = moves(0, 1);
        assert!(!only.up && !only.down && !only.top && !only.bottom);
    }

    /// The header counts what is left to do, and says how much is done
    /// without leading with it.
    #[test]
    fn the_header_counts_the_work_that_is_left() {
        let open = |n: usize| {
            (0..n)
                .map(|i| issue(&format!("i-{i:04}"), "x", IssueState::Open, None))
                .collect::<Vec<_>>()
        };
        assert_eq!(summary(&rows(&[], &[])), "empty");
        assert_eq!(summary(&rows(&open(3), &[])), "3");

        let mut mixed = open(3);
        mixed.push(issue("i-0009", "done", IssueState::Closed, None));
        mixed.push(issue("i-0010", "done", IssueState::Closed, None));
        assert_eq!(summary(&rows(&mixed, &[])), "3 · 2 done");
    }

    /// Two states, two glyphs, and they are different — the leading column
    /// is the whole of "is this still work".
    #[test]
    fn the_state_glyphs_are_distinct() {
        assert_ne!(state_icon(IssueState::Open), state_icon(IssueState::Closed));
    }
}
