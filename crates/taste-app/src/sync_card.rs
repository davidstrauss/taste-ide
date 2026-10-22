//! **A sync count's hover card.**
//!
//! The file tree's ⇊, ↓, and ↑ each say what they would do, to which ref,
//! with which numbers, and by which gesture. As tooltip sentences that was
//! a paragraph to read before every press (David, 2026-10-19: "I want all
//! of these tooltips to be less narrative … pop up a more flexible GTK
//! widget with a nice information presentation"). So each is a card: the
//! action as its heading, the facts as label and value, a warning when one
//! of them is a loss, and the gesture last, small.
//!
//! It is drawn INSIDE the window, on a layer over everything ([`Layer`]),
//! not as a tooltip: a tooltip, like a popover, is a surface of its own,
//! and sharing the window in a call shows the window without it (David,
//! 2026-10-19: "I would like these popups to show when I share this
//! 'window' in something like Zoom or Google Meet"). It comes up after a
//! tooltip's pause on hover and goes when the button is pressed.
//!
//! **It lists the commits it counts**, and can be entered (David,
//! 2026-10-22: "If I move my mouse over the 'tooltip,' I would like to be
//! able to (1) scroll the commit listing if it's more than ~10 and (2) see
//! more commit details as a second 'tooltip' that opens to the right"). So
//! the pointer leaving the button does not take the card down at once: it
//! lingers ([`LINGER`]) for the pointer to cross onto it, where the list
//! scrolls and a commit's row, hovered, opens its detail card beside the
//! card — to the right, or the left where the right has no room. Both go
//! once the pointer has left the button and both cards for a linger.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// What a card shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncCard {
    /// The action, with its ref: "Rebase onto origin/main".
    pub title: String,
    /// The numbers it acts on, as label and value: ("New there", "1 change").
    pub facts: Vec<(String, String)>,
    /// A loss the action would cause, said in the warning colour.
    pub warning: Option<String>,
    /// How to do it: "Hold 2 s to rebase · a click only fast-forwards".
    pub gesture: Option<String>,
    /// The commits the count counts, listed under the facts.
    pub commits: taste_git::CommitList,
}

impl SyncCard {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    pub fn fact(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push((name.into(), value.into()));
        self
    }

    pub fn warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }

    pub fn warning_if(self, warning: Option<String>) -> Self {
        match warning {
            Some(warning) => self.warning(warning),
            None => self,
        }
    }

    pub fn gesture(mut self, gesture: impl Into<String>) -> Self {
        self.gesture = Some(gesture.into());
        self
    }

    pub fn commits(mut self, commits: taste_git::CommitList) -> Self {
        self.commits = commits;
        self
    }

    /// The card, built: a few labels in a grid on a tooltip's dark
    /// ground, made each time it shows, and the commits' rows, each of which
    /// calls `on_row` with its commit when the pointer comes onto it.
    fn widget(
        &self,
        on_row: &Rc<dyn Fn(&gtk::Widget, Option<&taste_git::CommitLine>)>,
    ) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("osd");
        card.add_css_class("sync-card");
        // `TASTE_PROBE_CARD` shoots it by this name, whole: drawn on the
        // window's layer, it runs past the pane a pane's shot is cut to.
        card.set_widget_name("sync-card");
        let title = gtk::Label::builder()
            .label(&self.title)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        card.append(&title);
        if !self.facts.is_empty() {
            let grid = gtk::Grid::builder()
                .column_spacing(12)
                .row_spacing(2)
                .build();
            for (row, (name, value)) in self.facts.iter().enumerate() {
                let name = gtk::Label::builder()
                    .label(name)
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .build();
                let value = gtk::Label::builder()
                    .label(value)
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(44)
                    .build();
                grid.attach(&name, 0, row as i32, 1, 1);
                grid.attach(&value, 1, row as i32, 1, 1);
            }
            card.append(&grid);
        }
        if let Some(list) = commit_rows(&self.commits, on_row) {
            card.append(&list);
        }
        if let Some(warning) = &self.warning {
            card.append(
                &gtk::Label::builder()
                    .label(warning)
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(44)
                    .css_classes(["warning"])
                    .build(),
            );
        }
        if let Some(gesture) = &self.gesture {
            card.append(
                &gtk::Label::builder()
                    .label(gesture)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }
        card.upcast()
    }
}

/// The commits' rows, in a list that scrolls past about ten of them, and
/// a last line for any the list does not carry. `None` with none.
fn commit_rows(
    commits: &taste_git::CommitList,
    on_row: &Rc<dyn Fn(&gtk::Widget, Option<&taste_git::CommitLine>)>,
) -> Option<gtk::Widget> {
    if commits.commits.is_empty() {
        return None;
    }
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let now = glib::DateTime::now_utc().map_or(0, |now| now.to_unix());
    for commit in &commits.commits {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["commit-row"])
            .build();
        row.append(
            &gtk::Label::builder()
                .label(&commit.summary)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(44)
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .label(commit_age(now, commit.when))
                .xalign(1.0)
                .css_classes(["caption", "dim-label", "numeric"])
                .build(),
        );
        let motion = gtk::EventControllerMotion::new();
        // The row weakly: its own controller holding it would keep every
        // card ever shown alive.
        {
            let (on_row, commit, row) = (on_row.clone(), commit.clone(), row.downgrade());
            motion.connect_enter(move |_, _, _| {
                if let Some(row) = row.upgrade() {
                    on_row(row.upcast_ref(), Some(&commit));
                }
            });
        }
        {
            let (on_row, row) = (on_row.clone(), row.downgrade());
            motion.connect_leave(move |_| {
                if let Some(row) = row.upgrade() {
                    on_row(row.upcast_ref(), None);
                }
            });
        }
        row.add_controller(motion);
        rows.append(&row);
    }
    if commits.more > 0 {
        rows.append(
            &gtk::Label::builder()
                .label(format!("and {} more", count(commits.more, "commit")))
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .margin_top(2)
                .build(),
        );
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(LIST_HEIGHT)
        .child(&rows)
        .build();
    scroller.add_css_class("commit-list");
    Some(scroller.upcast())
}

/// How tall the commit list grows before it scrolls: about ten rows.
const LIST_HEIGHT: i32 = 240;

/// A commit's age as its row says it: minutes, hours, then days, then the
/// date once it is two months old.
fn commit_age(now: i64, when: i64) -> String {
    let seconds = (now - when).max(0);
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=172_799 => format!("{} h ago", seconds / 3600),
        172_800..=5_183_999 => format!("{} d ago", seconds / 86_400),
        _ => commit_date(when, "%-d %b %Y"),
    }
}

/// `when` in this machine's time zone, in `format`.
fn commit_date(when: i64, format: &str) -> String {
    glib::DateTime::from_unix_local(when)
        .ok()
        .and_then(|date| date.format(format).ok())
        .map(|date| date.to_string())
        .unwrap_or_default()
}

/// A commit's detail card: its summary as the heading, its message, and
/// who wrote it when, under which id. The message scrolls when it is long.
fn detail_widget(commit: &taste_git::CommitLine) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("osd");
    card.add_css_class("sync-card");
    card.set_widget_name("commit-card");
    card.append(
        &gtk::Label::builder()
            .label(&commit.summary)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(52)
            .css_classes(["heading"])
            .build(),
    );
    if !commit.body.is_empty() {
        let body = gtk::Label::builder()
            .label(reflow(&commit.body))
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(52)
            .build();
        card.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_width(true)
                .propagate_natural_height(true)
                .max_content_height(DETAIL_HEIGHT)
                .child(&body)
                .build(),
        );
    }
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(2)
        .build();
    let facts = [
        ("Author", commit.author.clone()),
        ("Date", commit_date(commit.when, "%-d %b %Y, %H:%M")),
        ("Commit", commit.id.clone()),
    ];
    for (row, (name, value)) in facts.into_iter().enumerate() {
        grid.attach(
            &gtk::Label::builder()
                .label(name)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
            0,
            row as i32,
            1,
            1,
        );
        let value = gtk::Label::builder()
            .label(&value)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        if name == "Commit" {
            value.add_css_class("monospace");
            value.add_css_class("caption");
        }
        grid.attach(&value, 1, row as i32, 1, 1);
    }
    card.append(&grid);
    card.upcast()
}

/// A commit message's body as running text: git's hard wrap at about 72
/// columns undone, so a paragraph wraps to the card rather than breaking
/// mid-sentence where an editor broke it. A blank line still ends a
/// paragraph, and a line that starts a list item or is indented keeps its
/// own line.
fn reflow(body: &str) -> String {
    let mut out = String::new();
    let mut previous_blank = true;
    for line in body.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            out.push_str("\n\n");
            previous_blank = true;
            continue;
        }
        let starts_own_line = line.starts_with(char::is_whitespace)
            || ["- ", "* ", "• "]
                .iter()
                .any(|marker| line.starts_with(marker))
            || line
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !previous_blank {
            out.push(if starts_own_line { '\n' } else { ' ' });
        }
        out.push_str(trimmed);
        previous_blank = false;
    }
    while out.ends_with('\n') {
        out.pop();
    }
    // Blank lines run together come out as one.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// How tall a commit's message grows in its detail card before it scrolls.
const DETAIL_HEIGHT: i32 = 320;

/// How long the pointer rests on a button before its card comes up: a
/// tooltip's pause, so the cards come and go as tooltips do.
const PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a card stays once the pointer has left its button and both
/// cards: time to cross the gap between the button and the card, or the
/// card and a commit's detail beside it.
const LINGER: std::time::Duration = std::time::Duration::from_millis(300);

/// How often a card that is up looks again at what it shows: the counts
/// under it can change while the pointer rests there (a fetch landing), and
/// its button can go.
const RECHECK: std::time::Duration = std::time::Duration::from_millis(500);

/// Air between a card and its button, and between a card and the commit
/// detail beside it.
const GAP: f64 = 4.0;

/// What one button's card shows, written by whoever knows the numbers;
/// `None` shows nothing.
pub type CardSlot = Rc<RefCell<Option<SyncCard>>>;

/// The card up now: whose, from which slot, and what it showed.
struct Shown {
    target: glib::WeakRef<gtk::Widget>,
    slot: CardSlot,
    card: SyncCard,
    recheck: glib::SourceId,
}

/// The layer the cards are drawn on: over everything in the window, and
/// taking the pointer only where a card is ([`PassThrough`]). The window
/// makes it and lays it over its content ([`Layer::install`]).
pub struct Layer {
    pub layer: gtk::Fixed,
    shown: RefCell<Option<Shown>>,
    /// The card on the layer now, and where it was put.
    card: RefCell<Option<(gtk::Widget, gtk::graphene::Rect)>>,
    /// The commit detail beside it, by the commit's id, and the row it is
    /// for, which wears `lit` while it shows.
    detail: RefCell<Option<(String, gtk::Widget, gtk::Widget)>>,
    /// Where the pointer is, of the three places that keep a card up.
    on_button: Cell<bool>,
    on_card: Cell<bool>,
    on_detail: Cell<bool>,
    /// The card's end, while the pointer is on none of them.
    linger: Cell<Option<glib::SourceId>>,
    /// The detail's end, while the pointer is on neither it nor its row.
    detail_linger: Cell<Option<glib::SourceId>>,
}

thread_local! {
    static LAYER: RefCell<Option<Rc<Layer>>> = const { RefCell::new(None) };
}

impl Layer {
    /// The layer, made and made the one cards are drawn on; the caller
    /// adds `layer` as an overlay over the window's content.
    pub fn install() -> Rc<Self> {
        let layer: gtk::Fixed = glib::Object::builder::<PassThrough>()
            .property("can-focus", false)
            .property("hexpand", true)
            .property("vexpand", true)
            .build()
            .upcast();
        let this = Rc::new(Self {
            layer,
            shown: RefCell::new(None),
            card: RefCell::new(None),
            detail: RefCell::new(None),
            on_button: Cell::new(false),
            on_card: Cell::new(false),
            on_detail: Cell::new(false),
            linger: Cell::new(None),
            detail_linger: Cell::new(None),
        });
        LAYER.with(|cell| *cell.borrow_mut() = Some(this.clone()));
        this
    }

    pub fn current() -> Option<Rc<Self>> {
        LAYER.with(|cell| cell.borrow().clone())
    }

    /// `card` under `target`, centred on it and kept inside the window; above
    /// it when there is no room below.
    pub fn show(self: &Rc<Self>, target: &gtk::Widget, card: &SyncCard) {
        self.clear();
        self.place(target, card);
    }

    fn place(self: &Rc<Self>, target: &gtk::Widget, card: &SyncCard) -> bool {
        let Some(bounds) = target.compute_bounds(&self.layer) else {
            return false;
        };
        let on_row: Rc<dyn Fn(&gtk::Widget, Option<&taste_git::CommitLine>)> = {
            let weak = Rc::downgrade(self);
            Rc::new(move |row, commit| {
                if let Some(layer) = weak.upgrade() {
                    layer.on_row(row, commit);
                }
            })
        };
        let widget = card.widget(&on_row);
        let (w, h) = self.fit(&widget);
        let (layer_w, layer_h) = (
            f64::from(self.layer.width()),
            f64::from(self.layer.height()),
        );
        let x = (f64::from(bounds.x()) + f64::from(bounds.width()) / 2.0 - w / 2.0)
            .clamp(4.0, (layer_w - w - 4.0).max(4.0));
        let below = f64::from(bounds.y() + bounds.height()) + GAP;
        let y = if below + h <= layer_h {
            below
        } else {
            (f64::from(bounds.y()) - GAP - h).max(0.0)
        };
        self.layer.put(&widget, x, y);
        self.watch_pointer(&widget, false);
        *self.card.borrow_mut() = Some((
            widget,
            gtk::graphene::Rect::new(x as f32, y as f32, w as f32, h as f32),
        ));
        true
    }

    /// `widget`'s size on the layer: its natural width, within the window,
    /// and its natural height at that width, within the window too. A fixed
    /// child is allocated its minimum, which for a wrapping label is its
    /// longest word — a card's values wrapped a word a line under any title
    /// shorter than they were — and for a scrolled list a row or two. Both
    /// are asked for outright.
    fn fit(&self, widget: &gtk::Widget) -> (f64, f64) {
        let (_, natural_w, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
        let natural_w = natural_w.min(self.layer.width() - 8).max(1);
        let (_, natural_h, _, _) = widget.measure(gtk::Orientation::Vertical, natural_w);
        let natural_h = natural_h.min(self.layer.height() - 8).max(1);
        widget.set_size_request(natural_w, natural_h);
        (f64::from(natural_w), f64::from(natural_h))
    }

    /// Keep the cards up while the pointer is on `widget` — the card, or
    /// with `detail` the commit's card beside it.
    fn watch_pointer(self: &Rc<Self>, widget: &gtk::Widget, detail: bool) {
        let motion = gtk::EventControllerMotion::new();
        {
            let weak = Rc::downgrade(self);
            motion.connect_enter(move |_, _, _| {
                let Some(layer) = weak.upgrade() else { return };
                if detail {
                    layer.on_detail.set(true);
                    if let Some(id) = layer.detail_linger.take() {
                        id.remove();
                    }
                } else {
                    layer.on_card.set(true);
                }
                layer.hold();
            });
        }
        {
            let weak = Rc::downgrade(self);
            motion.connect_leave(move |_| {
                let Some(layer) = weak.upgrade() else { return };
                if detail {
                    layer.on_detail.set(false);
                    layer.release_detail();
                } else {
                    layer.on_card.set(false);
                }
                layer.release();
            });
        }
        widget.add_controller(motion);
    }

    /// The pointer came onto a commit's row (`Some`), or left one: its
    /// detail opens beside the card, or goes after a linger unless the
    /// pointer reaches it.
    fn on_row(self: &Rc<Self>, row: &gtk::Widget, commit: Option<&taste_git::CommitLine>) {
        let Some(commit) = commit else {
            self.release_detail();
            return;
        };
        if let Some(id) = self.detail_linger.take() {
            id.remove();
        }
        if self
            .detail
            .borrow()
            .as_ref()
            .is_some_and(|(id, _, _)| *id == commit.id)
        {
            return;
        }
        self.drop_detail();
        let Some(card) = self.card.borrow().as_ref().map(|(_, rect)| *rect) else {
            return;
        };
        let Some(row_bounds) = row.compute_bounds(&self.layer) else {
            return;
        };
        let widget = detail_widget(commit);
        let (w, h) = self.fit(&widget);
        let (layer_w, layer_h) = (
            f64::from(self.layer.width()),
            f64::from(self.layer.height()),
        );
        let right = f64::from(card.x() + card.width()) + GAP;
        let left = f64::from(card.x()) - GAP - w;
        let x = if right + w <= layer_w - 4.0 {
            right
        } else if left >= 4.0 {
            left
        } else {
            right.min(layer_w - w - 4.0).max(4.0)
        };
        // Level with the row's top, the way a submenu opens beside its
        // item, and kept inside the window.
        let y = f64::from(row_bounds.y()).clamp(0.0, (layer_h - h).max(0.0));
        self.layer.put(&widget, x, y);
        self.watch_pointer(&widget, true);
        row.add_css_class("lit");
        *self.detail.borrow_mut() = Some((commit.id.clone(), widget, row.clone()));
    }

    /// The detail goes after a linger, unless the pointer reaches it or
    /// another row first.
    fn release_detail(self: &Rc<Self>) {
        if self.on_detail.get() || self.detail.borrow().is_none() {
            return;
        }
        if let Some(id) = self.detail_linger.take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(LINGER, move || {
            let Some(layer) = weak.upgrade() else { return };
            layer.detail_linger.set(None);
            if !layer.on_detail.get() {
                layer.drop_detail();
            }
        });
        self.detail_linger.set(Some(id));
    }

    fn drop_detail(&self) {
        if let Some(id) = self.detail_linger.take() {
            id.remove();
        }
        if let Some((_, widget, row)) = self.detail.borrow_mut().take() {
            row.remove_css_class("lit");
            self.layer.remove(&widget);
        }
        self.on_detail.set(false);
    }

    /// The pointer is on the button or a card: no linger runs.
    fn hold(&self) {
        if let Some(id) = self.linger.take() {
            id.remove();
        }
    }

    /// The pointer left one of the three: once it is on none of them, the
    /// cards go after a linger.
    fn release(self: &Rc<Self>) {
        if self.on_button.get() || self.on_card.get() || self.on_detail.get() {
            return;
        }
        if self.shown.borrow().is_none() || self.linger_running() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(LINGER, move || {
            let Some(layer) = weak.upgrade() else { return };
            layer.linger.set(None);
            if !(layer.on_button.get() || layer.on_card.get() || layer.on_detail.get()) {
                layer.hide();
            }
        });
        self.linger.set(Some(id));
    }

    fn linger_running(&self) -> bool {
        let id = self.linger.take();
        let running = id.is_some();
        self.linger.set(id);
        running
    }

    /// `target`'s card from `slot`, kept as the slot changes until it is
    /// hidden: drawn again when what it says changes — not while the
    /// pointer is on it, where a redraw would lose the list's place — and
    /// gone when it says nothing or its button is no longer on screen
    /// (review, 2026-10-20: a card stayed up with the numbers of before a
    /// fetch).
    fn show_slot(self: &Rc<Self>, target: &gtk::Widget, slot: &CardSlot) {
        self.hide();
        let Some(card) = slot.borrow().clone() else {
            return;
        };
        if !self.place(target, &card) {
            return;
        }
        let weak = Rc::downgrade(self);
        let recheck = glib::timeout_add_local(RECHECK, move || {
            let Some(layer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let entered = layer.on_card.get() || layer.on_detail.get();
            let mut shown = layer.shown.borrow_mut();
            let Some(now) = shown.as_mut() else {
                return glib::ControlFlow::Break;
            };
            let target = now.target.upgrade().filter(|target| target.is_mapped());
            let card = now.slot.borrow().clone();
            match (target, card) {
                (Some(target), Some(card)) => {
                    if card != now.card && !entered {
                        now.card = card.clone();
                        drop(shown);
                        layer.clear();
                        layer.place(&target, &card);
                    }
                    glib::ControlFlow::Continue
                }
                _ => {
                    // The source ends by returning Break; it must not also
                    // be removed by `hide`.
                    shown.take();
                    drop(shown);
                    layer.hide();
                    glib::ControlFlow::Break
                }
            }
        });
        *self.shown.borrow_mut() = Some(Shown {
            target: target.downgrade(),
            slot: slot.clone(),
            card,
            recheck,
        });
    }

    fn clear(&self) {
        self.drop_detail();
        self.card.borrow_mut().take();
        self.on_card.set(false);
        while let Some(child) = self.layer.first_child() {
            self.layer.remove(&child);
        }
    }

    pub fn hide(&self) {
        self.hold();
        if let Some(shown) = self.shown.borrow_mut().take() {
            shown.recheck.remove();
        }
        self.clear();
    }

    /// TASTE_PROBE_CHECK only: the first commit's detail opened beside the
    /// card up, as the pointer on its row opens it.
    pub fn probe_first_detail(self: &Rc<Self>, card: &SyncCard) {
        let Some(commit) = card.commits.commits.first() else {
            return;
        };
        let Some(widget) = self
            .card
            .borrow()
            .as_ref()
            .map(|(widget, _)| widget.clone())
        else {
            return;
        };
        let mut stack = vec![widget];
        while let Some(widget) = stack.pop() {
            if widget.has_css_class("commit-row") {
                self.on_row(&widget, Some(commit));
                return;
            }
            let mut child = widget.last_child();
            while let Some(c) = child {
                child = c.prev_sibling();
                stack.push(c);
            }
        }
    }

    /// Whether the card up is `target`'s.
    fn shows(&self, target: &gtk::Widget) -> bool {
        self.shown
            .borrow()
            .as_ref()
            .is_some_and(|shown| shown.target.upgrade().as_ref() == Some(target))
    }

    /// Hide the card if it is `target`'s, and only then: one button's
    /// pointer leaving must not take down the next one's card.
    fn hide_for(&self, target: &gtk::Widget) {
        if self.shows(target) {
            self.hide();
        }
    }
}

/// The cards' layer: a [`gtk::Fixed`] that holds no point of its own, so
/// the pointer reaches whatever is under it everywhere but on a card. A
/// fixed that took no pointer at all (`can-target` false) took none for
/// its children either, and a card has a list to scroll and rows to hover.
mod imp {
    use gtk::glib;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct PassThrough;

    #[glib::object_subclass]
    impl ObjectSubclass for PassThrough {
        const NAME: &'static str = "TastePassThrough";
        type Type = super::PassThrough;
        type ParentType = gtk::Fixed;
    }

    impl ObjectImpl for PassThrough {}

    impl WidgetImpl for PassThrough {
        fn contains(&self, _x: f64, _y: f64) -> bool {
            false
        }
    }

    impl FixedImpl for PassThrough {}
}

glib::wrapper! {
    pub struct PassThrough(ObjectSubclass<imp::PassThrough>)
        @extends gtk::Fixed, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// Give `button` a card on hover, and return where to put what it shows:
/// `None` shows nothing.
///
/// The pointer is watched from the button's parent, not the button: GTK
/// runs no controller of an insensitive widget, and several cards are for
/// buttons insensitive when they matter — "Nothing to pull", "Up to date",
/// "Read-only" (review, 2026-10-20). The button must have its parent when
/// this is called.
pub fn install(button: &gtk::Button) -> CardSlot {
    button.set_tooltip_text(None);
    let card: CardSlot = Rc::new(RefCell::new(None));
    let pending: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    let inside = Rc::new(Cell::new(false));
    let target: gtk::Widget = button.clone().upcast();
    // The pointer left the button: a card coming up does not, and one up
    // lingers for the pointer to cross onto it.
    let leave = {
        let (pending, target) = (pending.clone(), target.downgrade());
        move || {
            if let Some(id) = pending.take() {
                id.remove();
            }
            if let (Some(layer), Some(target)) = (Layer::current(), target.upgrade()) {
                if layer.shows(&target) {
                    layer.on_button.set(false);
                    layer.release();
                }
            }
        }
    };
    // The button was pressed: its card goes at once, as a tooltip does.
    let cancel = {
        let (pending, target) = (pending.clone(), target.downgrade());
        move || {
            if let Some(id) = pending.take() {
                id.remove();
            }
            if let (Some(layer), Some(target)) = (Layer::current(), target.upgrade()) {
                layer.hide_for(&target);
            }
        }
    };
    let host = button.parent().unwrap_or_else(|| target.clone());
    let motion = gtk::EventControllerMotion::new();
    let over = {
        let (card, pending, inside, leave) =
            (card.clone(), pending.clone(), inside.clone(), leave.clone());
        let (host, target) = (host.downgrade(), target.downgrade());
        move |x: f64, y: f64| {
            let (Some(host), Some(target)) = (host.upgrade(), target.upgrade()) else {
                return;
            };
            let now = target.is_mapped()
                && target.compute_bounds(&host).is_some_and(|b| {
                    b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32))
                });
            if now == inside.replace(now) {
                return;
            }
            if !now {
                leave();
                return;
            }
            // Back on the button with its card still up: it stays.
            if let Some(layer) = Layer::current().filter(|layer| layer.shows(&target)) {
                layer.on_button.set(true);
                layer.hold();
                return;
            }
            let (card, target, slot) = (card.clone(), target.downgrade(), pending.clone());
            let id = glib::timeout_add_local_once(PAUSE, move || {
                slot.take();
                let (Some(target), Some(layer)) = (target.upgrade(), Layer::current()) else {
                    return;
                };
                layer.show_slot(&target, &card);
                layer.on_button.set(true);
            });
            if let Some(old) = pending.replace(Some(id)) {
                old.remove();
            }
        }
    };
    {
        let over = over.clone();
        motion.connect_enter(move |_, x, y| over(x, y));
    }
    motion.connect_motion(move |_, x, y| over(x, y));
    {
        let (inside, leave) = (inside.clone(), leave.clone());
        motion.connect_leave(move |_| {
            if inside.replace(false) {
                leave();
            }
        });
    }
    host.add_controller(motion);
    // A press is an answer to the card, which goes, as a tooltip does.
    let press = gtk::GestureClick::new();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    press.connect_pressed(move |_, _, _, _| cancel());
    button.add_controller(press);
    card
}

/// `count` of `thing`, plural when it is not one: "1 commit", "2 commits".
pub fn count(count: usize, thing: &str) -> String {
    if count == 1 {
        format!("1 {thing}")
    } else {
        format!("{count} {thing}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_body_reads_as_paragraphs_and_keeps_its_lists() {
        let body = "The tab loaded the port as\n127.0.0.1; next dev refused\nits socket.\n\n\
                    - one item\n- two items\n  indented\n1. first\n\nLast.";
        assert_eq!(
            reflow(body),
            "The tab loaded the port as 127.0.0.1; next dev refused its socket.\n\n\
             - one item\n- two items\n  indented\n1. first\n\nLast."
        );
    }

    #[test]
    fn a_commit_age_counts_up_then_names_the_day() {
        let now = 10_000_000;
        assert_eq!(commit_age(now, now - 30), "just now");
        assert_eq!(commit_age(now, now - 600), "10 min ago");
        assert_eq!(commit_age(now, now - 7_200), "2 h ago");
        assert_eq!(commit_age(now, now - 3 * 86_400), "3 d ago");
        assert!(!commit_age(now, now - 90 * 86_400).contains("ago"));
    }

    #[test]
    fn a_card_is_built_up_in_order() {
        let card = SyncCard::new("Rebase onto origin/main")
            .fact("New there", count(1, "change"))
            .fact("Yours", count(4, "commit"))
            .gesture("Hold 2 s to rebase");
        assert_eq!(
            card.facts,
            vec![
                ("New there".to_string(), "1 change".to_string()),
                ("Yours".to_string(), "4 commits".to_string())
            ]
        );
        assert_eq!(card.warning, None);
    }
}
