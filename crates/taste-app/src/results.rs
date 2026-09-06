//! A results listing: the bottom panel a document pane opens under a
//! query (docs/SEARCH.md rule 2). Files, terminals and transcripts are not
//! lists of rows, so they keep their content and gain this — a header that
//! says how many and whether the search is still running, and grouped rows
//! that go to the hit when activated. One widget, three homes (the
//! editor's, the console's, the chat's), the intervention-panel shape.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::search::{Query, Step};

/// What activating a row does; the home interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    File {
        path: std::path::PathBuf,
        line: u32,
    },
    Commit {
        id: String,
        message: String,
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
    on_close: RefCell<Option<Box<dyn Fn()>>>,
}

const MAX_HEIGHT: i32 = 240;

impl ResultsPanel {
    pub fn new() -> Rc<Self> {
        let title = gtk::Label::builder()
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let rule = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::Center)
            .css_classes(["search-rule"])
            .tooltip_text("Still searching")
            .visible(false)
            .build();
        rule.set_size_request(48, 4);
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close the results (Escape in the search box clears the query)")
            .css_classes(["flat", "circular"])
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_start(10);
        header.set_margin_end(6);
        header.append(&title);
        header.append(&rule);
        header.append(&close);

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
            on_activate: RefCell::new(None),
            on_close: RefCell::new(None),
        });
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
        {
            let weak = Rc::downgrade(&panel);
            close.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.hide();
                    if let Some(hook) = panel.on_close.borrow().as_ref() {
                        hook();
                    }
                }
            });
        }
        panel
    }

    pub fn set_on_activate(&self, hook: impl Fn(&Target) + 'static) {
        *self.on_activate.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_close(&self, hook: impl Fn() + 'static) {
        *self.on_close.borrow_mut() = Some(Box::new(hook));
    }

    pub fn hide(&self) {
        self.widget.set_reveal_child(false);
    }

    /// Show groups of hits. `running` keeps the rule up with `done/total`;
    /// an empty listing says so in words (rule 5: zero is an answer).
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
        for group in groups {
            if group.items.is_empty() {
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
                let primary = gtk::Label::builder()
                    .use_markup(true)
                    .label(&item.primary)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(20)
                    .build();
                let secondary = gtk::Label::builder()
                    .label(&item.secondary)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .ellipsize(gtk::pango::EllipsizeMode::Start)
                    .max_width_chars(20)
                    .build();
                let lines = gtk::Box::new(gtk::Orientation::Vertical, 0);
                lines.set_margin_top(3);
                lines.set_margin_bottom(3);
                lines.set_margin_start(10);
                lines.set_margin_end(10);
                lines.append(&primary);
                lines.append(&secondary);
                let row = gtk::ListBoxRow::builder().child(&lines).build();
                self.list.append(&row);
                targets.push(Some(item.target));
            }
        }
        if targets.is_empty() && !running {
            let empty = gtk::Label::builder()
                .label("Nothing here matches. Tab moves to the next panel that has results.")
                .css_classes(["dim-label", "caption"])
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(40)
                .margin_start(10)
                .margin_end(10)
                .margin_top(4)
                .margin_bottom(8)
                .build();
            let row = gtk::ListBoxRow::builder()
                .child(&empty)
                .selectable(false)
                .activatable(false)
                .build();
            self.list.append(&row);
            targets.push(None);
        }
        *self.items.borrow_mut() = targets;
        self.selected.set(None);
        if let Some(index) = keep {
            self.select(index);
        }
        self.scroller.set_max_content_height(MAX_HEIGHT);
        self.widget.set_reveal_child(true);
    }

    fn select(&self, index: usize) -> bool {
        let items = self.items.borrow();
        if index >= items.len() || items[index].is_none() {
            return false;
        }
        drop(items);
        if let Some(row) = self.list.row_at_index(index as i32) {
            self.list.select_row(Some(&row));
            row.grab_focus();
            self.selected.set(Some(index));
            return true;
        }
        false
    }

    /// Down, Up and Enter from the search box.
    pub fn step(&self, step: Step) -> bool {
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
                // Skip headings.
                for _ in 0..count {
                    if index >= count {
                        return false;
                    }
                    if self.select(index) {
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

/// `path:line`, relative to the root when it is under it.
pub fn place(root: &std::path::Path, path: &std::path::Path, line: u32) -> String {
    let shown = path.strip_prefix(root).unwrap_or(path);
    format!("{}:{line}", shown.display())
}

/// Keep GTK from ever being asked to show a broken markup string: fall
/// back to escaped text when the markup does not parse.
pub fn safe_markup(markup: &str, plain: &str) -> String {
    match gtk::pango::parse_markup(markup, '\0') {
        Ok(_) => markup.to_string(),
        Err(_) => glib::markup_escape_text(plain).to_string(),
    }
}
