//! A results listing: the bottom panel a document pane opens under a
//! query (docs/SEARCH.md rule 2). Files, terminals and transcripts are not
//! lists of rows, so they keep their content and gain this — a header that
//! says how many and whether the search is still running, and grouped rows
//! that go to the hit when activated. One widget, three homes (the
//! editor's, the console's, the chat's), the intervention-panel shape.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;

use crate::hover::FullTextOnHover;
use crate::search::{Query, Step};

/// What activating a row does; the home interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    File {
        path: std::path::PathBuf,
        line: u32,
    },
    /// A row of a terminal's scrollback: which page, and the row to scroll to.
    Terminal {
        page: usize,
        row: i64,
    },
    /// A row of the devcontainer log.
    Log {
        line: u32,
    },
    /// A transcript row, by index in the chat's list.
    Transcript {
        row: i32,
    },
}

#[derive(Debug, Clone)]
pub struct Item {
    /// Pango markup: the matching line, matches in bold.
    pub primary: String,
    /// Plain text: where it is (`filetree.rs:1175`, `Claude Code`).
    pub secondary: String,
    pub target: Target,
}

pub struct Group {
    pub title: String,
    pub items: Vec<Item>,
}

type ActivateHook = Box<dyn Fn(&Target)>;

pub struct ResultsPanel {
    pub widget: gtk::Revealer,
    title: gtk::Label,
    rule: gtk::LevelBar,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    items: RefCell<Vec<Option<Target>>>,
    selected: Cell<Option<usize>>,
    on_activate: RefCell<Option<ActivateHook>>,
    /// The selection moved onto a hit — by stepping or by a click. The home
    /// reveals it in the document: selects the text, scrolls the row.
    on_select: RefCell<Option<ActivateHook>>,
    /// The search box, once attached: a step takes the keyboard only when
    /// the box does not have it.
    search: RefCell<Option<Weak<crate::search::Search>>>,
    /// This listing's section is the Tab stop the search is on.
    current: Cell<bool>,
}

const MAX_HEIGHT: i32 = 240;

impl ResultsPanel {
    pub fn new() -> Rc<Self> {
        // `results-title` carries the padding the lit state used to add, so
        // lighting a stop changes colour and nothing else (David,
        // 2026-09-08: "Highlighting 'no messages' for a section shouldn't
        // adjust the layout at all").
        let title = gtk::Label::builder()
            .css_classes(["caption-heading", "results-title"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build()
            .full_text_on_hover();
        let rule = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::Center)
            .css_classes(["search-rule"])
            .tooltip_text("Still searching")
            .visible(false)
            .build();
        rule.set_size_request(48, 4);
        // No close button: the listing is the query's and goes when the
        // query does (Escape in the search box). A dismissal of its own
        // made a panel Tab then skipped and a count the box still showed
        // (David, 2026-09-06: "remove the 'close' button from them all").
        // The header's insets are the rows' (10), and it carries its own
        // bottom margin because it is the whole panel when nothing is
        // listed.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(10);
        header.set_margin_end(10);
        header.append(&title);
        header.append(&rule);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "results-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(MAX_HEIGHT)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("results-panel");
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&header);
        column.append(&scroller);
        let widget = gtk::Revealer::builder()
            .child(&column)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(140)
            .reveal_child(false)
            .build();
        widget.set_widget_name("results");

        let panel = Rc::new(Self {
            widget,
            title,
            rule,
            list: list.clone(),
            scroller,
            items: RefCell::new(Vec::new()),
            selected: Cell::new(None),
            search: RefCell::new(None),
            current: Cell::new(false),
            on_activate: RefCell::new(None),
            on_select: RefCell::new(None),
        });
        {
            // One place the selection is announced from, whether a step
            // put it there or a click did.
            let weak = Rc::downgrade(&panel);
            panel.list.connect_row_selected(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                let Some(row) = row else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let target = panel.items.borrow().get(index as usize).cloned().flatten();
                let Some(target) = target else { return };
                panel.selected.set(Some(index as usize));
                let hook = panel.on_select.borrow();
                if let Some(hook) = hook.as_ref() {
                    hook(&target);
                }
                drop(hook);
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            list.connect_row_activated(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                let index = row.index();
                if index >= 0 {
                    panel.selected.set(Some(index as usize));
                    panel.activate_selected();
                }
            });
        }
        panel
    }

    /// The search's Tab stop is (or is no longer) this listing's section.
    /// With rows, the selected row shows it; with none, the title itself is
    /// the placeholder that lights up, so a stop on an empty section is
    /// seen to have been taken (David, 2026-09-07: "add in a placeholder
    /// 'no results' that's selected to have consistency with the tab
    /// advancement").
    pub fn set_current(&self, current: bool) {
        self.current.set(current);
        self.sync_placeholder();
    }

    fn sync_placeholder(&self) {
        let empty = self.items.borrow().iter().all(Option::is_none);
        if self.current.get() && empty {
            self.title.add_css_class("results-current");
        } else {
            self.title.remove_css_class("results-current");
        }
    }

    /// Tab from a row of this listing moves to the next panel with results
    /// (search.rs), instead of to GTK's next focusable widget.
    pub fn attach_search(&self, search: &Rc<crate::search::Search>) {
        *self.search.borrow_mut() = Some(Rc::downgrade(search));
        crate::search::Search::tab_switches_panels(&self.list, search);
    }

    /// A count alone: the title line, nothing under it. For the sidebar's
    /// filtering panels — the files, Ports, Logs, the backlog — which
    /// answer a query by hiding rows and have no hits to list, but wear
    /// this so the eye knows they answered, with zero as an answer too
    /// (David, 2026-09-06: "show the banner at the bottom of each of those
    /// panels with the count of results, whether zero or more. That will
    /// signal to the user that the panel is search-responsive").
    pub fn show_count(&self, query: &Query, subject: &str, hits: usize, running: bool) {
        self.show(query, subject, Vec::new(), running, 0, 1);
        self.set_count_title(query, subject, hits, running);
    }

    /// A clause after the title: "· ≈6 by meaning" beside a literal count.
    pub fn note(&self, note: &str) {
        if note.is_empty() {
            return;
        }
        let text = self.title.label();
        self.title.set_label(&format!("{text} · {note}"));
    }

    fn set_count_title(&self, query: &Query, subject: &str, hits: usize, running: bool) {
        let mut title = if hits == 0 && !running {
            format!("No matches for “{}” in {subject}", query.text.trim())
        } else {
            format!(
                "{hits} match{} for “{}” in {subject}",
                if hits == 1 { "" } else { "es" },
                query.text.trim()
            )
        };
        if running {
            title.push_str(" · searching…");
        }
        self.title.set_label(&title);
    }

    pub fn set_on_activate(&self, hook: impl Fn(&Target) + 'static) {
        *self.on_activate.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_select(&self, hook: impl Fn(&Target) + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(hook));
    }

    /// The next hit, wrapping to the first after the last: what a second
    /// click on a row with matches does.
    pub fn step_cycle(&self) -> bool {
        if !self.is_open() {
            return false;
        }
        if self.step(Step::Next) {
            return true;
        }
        let first = self.items.borrow().iter().position(|item| item.is_some());
        first.is_some_and(|index| self.select(index, true))
    }

    /// Take the listing down — the query cleared, or the document it
    /// listed went away.
    pub fn hide(&self) {
        self.widget.set_reveal_child(false);
    }

    /// Rows drawn, at most. Every hit is counted in the title; past this
    /// many, one row says how many more there are. A listing is read from
    /// the top, and a thousand rows built on a keystroke is a frame lost to
    /// rows nobody will scroll to.
    const MAX_ROWS: usize = 200;

    /// Progress alone: the rule and the title's count, without rebuilding
    /// a single row. A source that reports as it goes calls this between
    /// its `show`s.
    pub fn set_progress(&self, running: bool, done: usize, total: usize) {
        self.rule.set_visible(running);
        if running {
            self.rule
                .set_value((done as f64 / total.max(1) as f64).clamp(0.0, 1.0));
        }
    }

    /// Show groups of hits. `running` keeps the rule up with `done/total`;
    /// an empty listing is its title saying so (rule 5: zero is an answer).
    pub fn show(
        &self,
        query: &Query,
        subject: &str,
        groups: Vec<Group>,
        running: bool,
        done: usize,
        total: usize,
    ) {
        let hits: usize = groups.iter().map(|g| g.items.len()).sum();
        self.set_count_title(query, subject, hits, running);
        self.rule.set_visible(running);
        if running {
            self.rule
                .set_value((done as f64 / total.max(1) as f64).clamp(0.0, 1.0));
        }

        let keep = self.selected.get();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut targets: Vec<Option<Target>> = Vec::new();
        let mut drawn = 0usize;
        let mut undrawn = 0usize;
        for group in groups {
            if group.items.is_empty() {
                continue;
            }
            if drawn >= Self::MAX_ROWS {
                undrawn += group.items.len();
                continue;
            }
            // An untitled group has no heading row: the listing's title
            // already says what these are (David: "Drop the 'matches'
            // subhead").
            if group.title.is_empty() {
                for item in group.items {
                    if drawn >= Self::MAX_ROWS {
                        undrawn += 1;
                        continue;
                    }
                    drawn += 1;
                    targets.push(Some(item.target.clone()));
                    self.list.append(&item_row(&item));
                }
                continue;
            }
            let heading = gtk::Label::builder()
                .label(&group.title)
                .css_classes(["caption-heading", "dim-label"])
                .xalign(0.0)
                .margin_start(10)
                .margin_top(6)
                .margin_bottom(2)
                .build();
            let row = gtk::ListBoxRow::builder()
                .child(&heading)
                .selectable(false)
                .activatable(false)
                .build();
            self.list.append(&row);
            targets.push(None);
            for item in group.items {
                if drawn >= Self::MAX_ROWS {
                    undrawn += 1;
                    continue;
                }
                drawn += 1;
                targets.push(Some(item.target.clone()));
                self.list.append(&item_row(&item));
            }
        }
        if undrawn > 0 {
            let more = gtk::Label::builder()
                .label(format!("… {undrawn} more; narrow the query to reach them"))
                .css_classes(["dim-label", "caption"])
                .xalign(0.0)
                .margin_start(10)
                .margin_end(10)
                .margin_top(4)
                .margin_bottom(8)
                .build();
            let row = gtk::ListBoxRow::builder()
                .child(&more)
                .selectable(false)
                .activatable(false)
                .build();
            self.list.append(&row);
            targets.push(None);
        }
        // Nothing to list: the title is the whole panel (David,
        // 2026-09-06: "For these 'no results' panels, just show the title
        // area").
        self.scroller.set_visible(!targets.is_empty());
        *self.items.borrow_mut() = targets;
        self.sync_placeholder();
        self.selected.set(None);
        // A refresh moves the selection under whoever has the keyboard; it
        // never takes it. It used to, and the second keystroke that changed
        // the hits pulled the cursor out of the box (David, 2026-09-07:
        // "When I started typing 'pinned' into the search box, it switched
        // to the results listing as I typed 'i'").
        if let Some(index) = keep {
            self.select(index, false);
        }
        self.scroller.set_max_content_height(MAX_HEIGHT);
        self.widget.set_reveal_child(true);
    }

    fn select(&self, index: usize, take_focus: bool) -> bool {
        let items = self.items.borrow();
        if index >= items.len() || items[index].is_none() {
            return false;
        }
        drop(items);
        if let Some(row) = self.list.row_at_index(index as i32) {
            // `row-selected` records the index and announces the hit.
            self.list.select_row(Some(&row));
            if take_focus {
                row.grab_focus();
            }
            return true;
        }
        false
    }

    /// Down, Up and Enter from the search box.
    pub fn step(&self, step: Step) -> bool {
        // A listing that is down has nothing to step through, whatever it
        // listed before it went.
        if !self.is_open() {
            return false;
        }
        let count = self.items.borrow().len();
        if count == 0 {
            return false;
        }
        match step {
            Step::Activate => self.activate_selected(),
            Step::Next | Step::Prev => {
                let start = self.selected.get();
                let mut index = match (start, step) {
                    (None, Step::Next) => 0,
                    (None, _) => count.saturating_sub(1),
                    (Some(at), Step::Next) => at + 1,
                    (Some(at), _) => at.saturating_sub(1),
                };
                // From the box, the box keeps the keyboard and the
                // selection moves under it; arriving by Tab from another
                // listing, the keyboard comes along (filetree.rs does the
                // same).
                let take_focus = !self
                    .search
                    .borrow()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|search| search.box_has_focus());
                // Skip headings.
                for _ in 0..count {
                    if index >= count {
                        return false;
                    }
                    if self.select(index, take_focus) {
                        return true;
                    }
                    index = match step {
                        Step::Next => index + 1,
                        _ => {
                            if index == 0 {
                                return false;
                            }
                            index - 1
                        }
                    };
                }
                false
            }
        }
    }

    fn activate_selected(&self) -> bool {
        let Some(index) = self.selected.get() else {
            return false;
        };
        let target = self.items.borrow().get(index).cloned().flatten();
        let Some(target) = target else { return false };
        if let Some(hook) = self.on_activate.borrow().as_ref() {
            hook(&target);
        }
        true
    }

    pub fn is_open(&self) -> bool {
        self.widget.reveals_child()
    }
}

/// Keep GTK from ever being asked to show a broken markup string: fall
/// back to escaped text when the markup does not parse.
pub fn safe_markup(markup: &str, plain: &str) -> String {
    match gtk::pango::parse_markup(markup, '\0') {
        Ok(_) => markup.to_string(),
        Err(_) => glib::markup_escape_text(plain).to_string(),
    }
}

/// One hit's row: the line with the match in bold, and where it is.
fn item_row(item: &Item) -> gtk::ListBoxRow {
    let primary = gtk::Label::builder()
        .use_markup(true)
        .label(&item.primary)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(20)
        .build()
        .full_text_on_hover();
    let secondary = gtk::Label::builder()
        .label(&item.secondary)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .max_width_chars(20)
        .build()
        .full_text_on_hover();
    let lines = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lines.set_margin_top(3);
    lines.set_margin_bottom(3);
    lines.set_margin_start(10);
    lines.set_margin_end(10);
    lines.append(&primary);
    lines.append(&secondary);
    gtk::ListBoxRow::builder().child(&lines).build()
}
