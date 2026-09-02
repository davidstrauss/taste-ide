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
//! - **The two panels are one thought, and each says its own half.** An
//!   environment says what it is working on; an issue says what state it is
//!   in. The queue used to draw the other half too — a chip naming the
//!   claiming environment, on a row that jumped the whole window at it when
//!   clicked — and that was the same sentence twice, eight pixels apart,
//!   with the second copy hiding a navigation nobody could see was there.
//!   The chip and the jump are both gone. Asked pointedly the question is
//!   still worth an answer, so the state glyph's tooltip names the
//!   environment; the row itself draws one status and it is the issue's.
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
use taste_git::{Issue, IssueMove, IssueState};

use crate::envstrip::title_of;
use crate::fleet::FleetRow;

/// How many rows the panel shows before it scrolls inside itself. Smaller
/// than the environment panel's six: the fleet is the thing you must be
/// able to read at a glance, and the backlog is the thing you consult.
pub const VISIBLE_ROWS: i32 = 5;

/// One row's height, for the scroller's ceiling. Not a layout constraint —
/// rows size themselves — just the arithmetic behind "about five rows".
const ROW_HEIGHT: i32 = 30;

/// Who holds an issue — kept for the state glyph's tooltip, and for
/// nothing else.
///
/// The row used to draw this: a dot in the environment's own traffic-light
/// colour and its name, in a chip at the end of every claimed row. It is
/// gone, and the deletion is the point of this panel's design. **An
/// environment says what it is working on; an issue says what state it is
/// in.** Both directions of the env↔issue link were on screen at once, and
/// the queue's own column — "which world has this" — is the one that is
/// not the queue's question. Asked pointedly, though, it is still worth an
/// answer, and a tooltip is exactly that: hovering the state glyph names
/// the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// What to call it: the environment's display name, or the raw
    /// assignee string when there is nothing to look it up in.
    pub label: String,
    /// The fleet has a row for it. `false` means the assignee names
    /// something this workspace no longer has — a fact worth saying rather
    /// than hiding.
    pub present: bool,
}

/// One issue, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    /// One of the four, derived by `taste-git` from what is written down
    /// and who holds it. The only status this row draws.
    pub state: IssueState,
    /// The environment working on it, when someone claimed it. Reaches the
    /// screen through the state glyph's tooltip alone.
    pub claim: Option<Claim>,
    /// When it last moved, in seconds since the epoch. In the tooltip
    /// rather than on the row: a backlog is read as an ordered list, and a
    /// column of ages would invite reading it as a sorted one.
    pub updated: i64,
    /// Why it was declined, when the trail says so — the first line of the
    /// comment the decline wrote. A state that means "somebody decided
    /// against this" is worth nothing without the decision, and the
    /// decision is already on the ref.
    pub note: Option<String>,
}

impl Row {
    /// The row's tooltip: which issue this is, and when it last moved.
    ///
    /// Identity only. The state — and the environment behind it — belongs
    /// to the glyph ([`Row::state_tooltip`]), which is the thing a reader
    /// points at when that is the question. Titles ellipsize in a 180px
    /// flank, so this is also how a truncated one is read in full.
    pub fn tooltip(&self) -> String {
        format!(
            "{} — {}\nLast changed {}.",
            self.id,
            self.title,
            crate::filetree::relative_age(self.updated)
        )
    }

    /// The state glyph's own tooltip: the state, named, and for an active
    /// issue the environment that made it one.
    ///
    /// This is where the claiming environment survives the chip's deletion.
    /// It is on the glyph rather than the row because the glyph IS the
    /// state — pointing at it is the question, and answering it over the
    /// whole row would put a second tooltip on top of the title's.
    pub fn state_tooltip(&self) -> String {
        match (self.state, &self.claim) {
            (IssueState::Active, Some(claim)) if claim.present => {
                format!("Active — {} is working on this.", claim.label)
            }
            (IssueState::Active, Some(claim)) => format!(
                "Active — claimed by {}, which this workspace no longer has.",
                claim.label
            ),
            // An issue cannot be active without a claim: the claim is what
            // makes it one. Said plainly rather than left to a fallthrough.
            (IssueState::Active, None) => "Active.".to_string(),
            (IssueState::Queued, _) => {
                "Queued — written down, and any environment can pick it up.".to_string()
            }
            (IssueState::Completed, _) => "Completed — its work is merged.".to_string(),
            (IssueState::Declined, _) => match &self.note {
                Some(note) => format!("Declined — {note}"),
                None => "Declined — it will not be done. The record stays.".to_string(),
            },
        }
    }
}

/// The reason a decline gave, off the issue's own comment trail.
///
/// `issue_decline` writes `Declined: <reason>`, so the last comment that
/// starts that way is the decision — read back rather than stored a second
/// time on the issue. First line only: a tooltip is one answer, and the
/// whole comment is in the issue.
fn decline_note(issue: &Issue) -> Option<String> {
    let note = issue
        .comments
        .iter()
        .rev()
        .find_map(|comment| comment.body.trim().strip_prefix("Declined:"))?
        .lines()
        .next()?
        .trim();
    (!note.is_empty()).then(|| note.to_string())
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
/// `fleet` is what turns an assignee slug into something a person reads —
/// the environment's display name, from the one assembly every other
/// surface renders, so the tooltip here and the panel above cannot disagree
/// about what a world is called. That is all the fleet is consulted for
/// now: the row draws no environment.
pub fn rows(issues: &[Issue], fleet: &[FleetRow]) -> Vec<Row> {
    issues
        .iter()
        .map(|issue| Row {
            id: issue.id.clone(),
            title: issue.title.clone(),
            state: issue.state(),
            updated: issue.updated,
            note: decline_note(issue),
            claim: issue.assignee.as_ref().map(|assignee| {
                match fleet.iter().find(|row| row.env.as_str() == assignee) {
                    Some(row) => Claim {
                        label: title_of(row),
                        present: true,
                    },
                    None => Claim {
                        label: assignee.clone(),
                        present: false,
                    },
                }
            }),
        })
        .collect()
}

/// The header's count, which is the queue's whole summary in one caption.
///
/// What is left to do leads, because the backlog is what is left to do; a
/// header that said "6" of a queue with two live items in it would be
/// answering a question nobody asked. Completed and declined are counted
/// separately — calling a decline "done" is the one thing the fourth state
/// exists to stop — and the declined half appears only when there is one,
/// so the ordinary queue's caption is unchanged.
pub fn summary(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "empty".to_string();
    }
    let live = rows.iter().filter(|row| !row.state.is_resolved()).count();
    let done = rows
        .iter()
        .filter(|row| row.state == IssueState::Completed)
        .count();
    let declined = rows
        .iter()
        .filter(|row| row.state == IssueState::Declined)
        .count();
    let mut text = live.to_string();
    if done > 0 {
        text.push_str(&format!(" · {done} done"));
    }
    if declined > 0 {
        text.push_str(&format!(" · {declined} declined"));
    }
    text
}

/// The glyph in a row's leading column — the only status a row draws.
///
/// Three of the four are the same checkbox, because three of the four are
/// the same object at different points of its life: empty, part-filled,
/// ticked. Declined leaves the family on purpose. It is not a checkbox
/// outcome at all — nothing was ticked and nothing is pending — so it gets
/// the glyph that means "not this": a circle with a line through it.
pub fn state_icon(state: IssueState) -> &'static str {
    match state {
        IssueState::Queued => "checkbox-symbolic",
        // A dash in the box, not a spinner: this panel runs no permanent
        // animation, and in a still frame a half-drawn ring reads as
        // breakage rather than as progress.
        IssueState::Active => "checkbox-mixed-symbolic",
        IssueState::Completed => "checkbox-checked-symbolic",
        IssueState::Declined => "action-unavailable-symbolic",
    }
}

/// How the glyph is drawn. Active is the only one at full strength — it is
/// the only state that is *happening* — and everything else is dimmed, the
/// settled two along with their titles, so a finished row recedes as a
/// whole rather than fading its text and keeping a bright mark.
///
/// Weight rather than hue: this flank already spends colour on traffic
/// lights, and a fifth colour meaning a fifth thing is how a panel stops
/// being readable at a glance.
fn state_classes(state: IssueState) -> Vec<&'static str> {
    match state {
        IssueState::Active => vec!["backlog-state"],
        _ => vec!["backlog-state", "dim-label"],
    }
}

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
    /// The open context menu, so a rebuild can close it before the row it
    /// is anchored to is disposed under it. The file tree tracks its own
    /// for the same reason and it is the same hazard: this list rebuilds
    /// whenever anything writes.
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
    /// A write is in flight: a second one is refused rather than queued
    /// behind the first, since both would be compare-and-swaps on one ref.
    writing: Cell<bool>,
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
            open_menu: RefCell::new(None),
            writing: Cell::new(false),
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
        panel.render();
        panel
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
        self.list.select_row(gtk::ListBoxRow::NONE);
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

        // The state, and only the state. Its tooltip is where the claiming
        // environment lives now that the row draws none: hovering the
        // glyph is the pointed question, and this is the answer to it.
        box_.append(
            &gtk::Image::builder()
                .icon_name(state_icon(row.state))
                .css_classes(state_classes(row.state))
                .pixel_size(13)
                .valign(gtk::Align::Center)
                .tooltip_text(row.state_tooltip())
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
        // A settled row recedes; a declined one is struck through as well.
        // The strike is what stops "dim" from having to mean two different
        // endings at once — completed and declined are both quiet, and
        // only one of them says the work never happened.
        //
        // Pango attributes rather than CSS: the title is the user's own
        // text and never markup, and an attribute list cannot be escaped
        // out of by an issue called `<b>`.
        if row.state.is_resolved() {
            label.add_css_class("dim-label");
        }
        if row.state == IssueState::Declined {
            let attrs = gtk::pango::AttrList::new();
            attrs.insert(gtk::pango::AttrInt::new_strikethrough(true));
            label.set_attributes(Some(&attrs));
        }
        box_.append(&label);

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

        // Not activatable, and that is the other half of dropping the claim
        // column. A click used to aim every pane in the window at the
        // environment holding the issue — a jump with no affordance,
        // reachable by clicking a row that looked exactly like the
        // unclaimed rows around it. Selecting an issue selects the issue.
        // Where an environment is, and what it is working on, is the
        // Environments panel's sentence to say.
        let widget = gtk::ListBoxRow::builder()
            .child(&box_)
            .activatable(false)
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

    /// The row's menu: the four moves, then Edit, Decline and Delete in a
    /// section of their own — the items that change what an issue *is* do
    /// not belong in the same group as the ones that only change where it
    /// sits in a list.
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

        // Whether this issue has already ended, either way. Read off the
        // stored resolution rather than the derived state, because they
        // answer this one identically — `Queued` and `Active` are both
        // `Resolution::Open` — and the stored field is the one the write
        // below will actually be compare-and-swapping.
        let resolved = self
            .issues
            .borrow()
            .get(index)
            .is_some_and(|issue| issue.resolution.is_resolved());

        let edit_section = gio::Menu::new();
        edit_section.append(Some("Edit…"), Some("row.edit"));
        // Decline sits above Delete because the two are the same gesture
        // with opposite consequences, and the choice should be one item
        // apart: declining KEEPS the record — the issue, its body, its
        // comments, and a new one saying it was decided against — while
        // deleting takes the id away and with it any way to find out that
        // the idea was ever had. It asks nothing, because unlike a delete
        // it is undoable: reopening is an edit away. Hence no ellipsis
        // either, where Delete earns one by stopping to confirm.
        edit_section.append(Some("Decline"), Some("row.decline"));
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
            // Insensitive on an issue that already ended: declining a
            // completed one is meaningless, and declining a declined one
            // twice is a second comment saying what the first said. It
            // stays in the menu rather than vanishing, for the same reason
            // the dead moves do — an item that disappears teaches a
            // different menu each time.
            let panel = self.clone();
            let id = id.to_string();
            add_action("decline", !resolved, Box::new(move || panel.decline(&id)));
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

    /// Decline it: the issue stays, and gains a comment saying it was
    /// decided against.
    ///
    /// The author is `primary` for the same reason a filed issue's reporter
    /// is: this menu is in the user's own window, and attributing their
    /// decision to an agent's environment would be a lie the issue carries
    /// forever.
    ///
    /// Nothing to revert: a decline changes a state, not an order, so the
    /// rows it would put back are the rows already on screen.
    fn decline(self: &Rc<Self>, id: &str) {
        let id = id.to_string();
        self.write(None, move |git| {
            git.issue_decline(&id, "primary", None).map(|_| ())
        });
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
    use taste_core::environment::EnvironmentId;
    use taste_core::state::WorkspaceState;
    use taste_devcontainer::SupervisorState;
    use taste_git::Resolution;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn issue(id: &str, title: &str, resolution: Resolution, assignee: Option<&str>) -> Issue {
        Issue {
            id: id.into(),
            title: title.into(),
            resolution,
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

    /// A row draws its state and nothing else — but the environment behind
    /// an Active one is still answerable, on the glyph, in the name the
    /// panel above uses for it. The join is the fleet's own assembly, never
    /// a second read of podman.
    #[test]
    fn a_row_shows_its_state_and_names_the_environment_only_when_asked() {
        let rows = rows(
            &[
                issue(
                    "i-0001",
                    "The parser drops commas",
                    Resolution::Open,
                    Some("calm-1"),
                ),
                issue("i-0002", "Rename the strip", Resolution::Open, None),
                issue(
                    "i-0003",
                    "Ship the gauge",
                    Resolution::Completed,
                    Some("spry-2"),
                ),
            ],
            &fleet(),
        );

        // Claimed and open is Active — derived, not read off a field.
        assert_eq!(rows[0].state, IssueState::Active);
        assert_eq!(rows[1].state, IssueState::Queued);
        assert_eq!(rows[2].state, IssueState::Completed);

        // The row itself says which issue this is and when it moved. No
        // environment and no state sentence: those belong to the glyph.
        let row = rows[0].tooltip();
        assert!(row.starts_with("i-0001 — The parser drops commas"), "{row}");
        assert!(
            !row.contains("calm-1"),
            "the row draws no environment: {row}"
        );

        // The glyph is where the claim survives, in the name the panel
        // above uses — a renamed environment included.
        let glyph = rows[0].state_tooltip();
        assert!(
            glyph.starts_with("Active — calm-1 is working on this"),
            "{glyph}"
        );
        assert!(rows[1].state_tooltip().starts_with("Queued"));
        assert!(rows[2].state_tooltip().starts_with("Completed"));
        assert_eq!(rows[2].claim.as_ref().unwrap().label, "the refactor");
    }

    /// An assignee the fleet does not have is a fact, not a blank: the
    /// label survives, and the glyph says the world behind it is gone.
    #[test]
    fn a_claim_by_an_environment_that_is_gone_still_says_who_had_it() {
        let rows = rows(
            &[issue(
                "i-0001",
                "Left behind",
                Resolution::Open,
                Some("gone-9"),
            )],
            &fleet(),
        );
        let claim = rows[0].claim.as_ref().unwrap();
        assert_eq!(claim.label, "gone-9");
        assert!(!claim.present);
        assert!(rows[0].state_tooltip().contains("no longer has"));
    }

    /// Declined is not completed, and the tooltip carries the decision off
    /// the issue's own comment trail rather than storing it a second time.
    #[test]
    fn a_declined_row_reads_the_decision_off_the_trail() {
        let mut declined = issue(
            "i-0005",
            "Gold-plate the gauge",
            Resolution::Declined,
            Some("calm-1"),
        );
        declined.comments = vec![
            taste_git::Comment {
                seq: 1,
                author: "calm-1".into(),
                created: 0,
                body: "Started on this.".into(),
            },
            taste_git::Comment {
                seq: 2,
                author: "primary".into(),
                created: 0,
                body: "Declined: out of scope for the alpha\nand for the beta".into(),
            },
        ];
        let listed = rows(&[declined], &fleet());
        assert_eq!(listed[0].state, IssueState::Declined);
        assert_eq!(
            listed[0].state_tooltip(),
            "Declined — out of scope for the alpha",
            "the first line of the decision, not the whole comment"
        );

        // Nothing on the trail: the state still says what it means.
        let bare = rows(
            &[issue("i-0006", "Never mind", Resolution::Declined, None)],
            &[],
        );
        assert!(bare[0].state_tooltip().contains("will not be done"));
    }

    /// The order that arrives is the order that renders. The ref decides
    /// what "top" means; a second surface sorting it is how the list on
    /// screen and the list in git come to disagree.
    #[test]
    fn the_rows_keep_the_order_they_arrive_in() {
        let ordered = [
            issue("i-0009", "Last filed, first wanted", Resolution::Open, None),
            issue("i-0001", "Filed first", Resolution::Open, None),
            issue("i-0004", "In between", Resolution::Completed, None),
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

    /// The header counts what is left to do, and never calls a decline
    /// "done" — which is the confusion the fourth state exists to prevent.
    #[test]
    fn the_header_counts_the_work_that_is_left() {
        let open = |n: usize| {
            (0..n)
                .map(|i| issue(&format!("i-{i:04}"), "x", Resolution::Open, None))
                .collect::<Vec<_>>()
        };
        assert_eq!(summary(&rows(&[], &[])), "empty");
        assert_eq!(summary(&rows(&open(3), &[])), "3");

        // Claiming does not change the count: an active issue is still work
        // that is left.
        let mut claimed = open(3);
        claimed[0].assignee = Some("calm-1".into());
        assert_eq!(summary(&rows(&claimed, &[])), "3");

        let mut mixed = open(3);
        mixed.push(issue("i-0009", "done", Resolution::Completed, None));
        mixed.push(issue("i-0010", "done", Resolution::Completed, None));
        assert_eq!(summary(&rows(&mixed, &[])), "3 · 2 done");
        mixed.push(issue("i-0011", "not doing it", Resolution::Declined, None));
        assert_eq!(summary(&rows(&mixed, &[])), "3 · 2 done · 1 declined");
    }

    /// Four states, four glyphs, all different — the leading column is the
    /// whole of what a row says now, so two states sharing a mark would be
    /// two states the panel cannot tell apart.
    #[test]
    fn the_state_glyphs_are_distinct() {
        let all = [
            IssueState::Queued,
            IssueState::Active,
            IssueState::Completed,
            IssueState::Declined,
        ];
        let icons: Vec<&str> = all.iter().map(|state| state_icon(*state)).collect();
        let mut unique = icons.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len(), "{icons:?}");
        // Only the state that is happening is at full strength.
        assert!(!state_classes(IssueState::Active).contains(&"dim-label"));
        for state in [
            IssueState::Queued,
            IssueState::Completed,
            IssueState::Declined,
        ] {
            assert!(state_classes(state).contains(&"dim-label"), "{state:?}");
        }
    }
}
