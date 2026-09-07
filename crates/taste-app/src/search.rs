//! The one search box, and the query it broadcasts. docs/SEARCH.md is the
//! design; this is its spine.
//!
//! The box lives in the title bar. Every surface that can answer a query
//! subscribes here and answers in place — the tree and the backlog filter
//! their rows, the editor, console and chat open results listings — and
//! reports back what it found and whether it is still looking, so the box
//! can say "37 hits · searching" and draw one thin rule of progress. A new
//! keystroke raises the previous query's stop flag; sources check it and
//! stop, because a search nobody is waiting for should not finish.
//!
//! The keyboard model (SEARCH.md → Keyboard): Ctrl+F focuses the box and
//! remembers where focus came from; Escape clears and returns it; Down
//! and Up step through the results of the panel that was focused before
//! the box — the tree, the editor, the console or the chat — Tab and
//! Shift+Tab move the stepping to the next panel that has results; Enter
//! activates the current one. Panels register a stepper for that.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

pub use taste_core::search::Query;

thread_local! {
    static CURRENT: RefCell<Query> = RefCell::new(Query::default());
}

/// The query as last published, for widgets built on demand (the pages
/// menu) that have no subscription of their own.
pub fn current_query() -> Query {
    CURRENT.with(|q| q.borrow().clone())
}

/// The panes that can be stepped through, in the window's reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Panel {
    Tree,
    Editor,
    Console,
    Chat,
}

impl Panel {
    pub const ORDER: [Panel; 4] = [Panel::Tree, Panel::Editor, Panel::Console, Panel::Chat];

    pub fn label(self) -> &'static str {
        match self {
            Panel::Tree => "files",
            Panel::Editor => "editor",
            Panel::Console => "console",
            Panel::Chat => "chat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Next,
    Prev,
    Activate,
}

/// What one source has found so far, and whether it is still looking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub hits: usize,
    pub done: usize,
    pub total: usize,
    pub running: bool,
}

type Listener = (&'static str, Box<dyn Fn(&Query, u64)>);
type Stepper = Box<dyn Fn(Step) -> bool>;

/// How long the box waits after a keystroke before the query goes out.
/// A quarter second: long enough that a word typed at speed is one query,
/// not five, short enough that a pause reads as "go" (David, 2026-09-06:
/// "Feel free to pause a tiny bit if we want to debounce the searching").
const SEARCH_DELAY_MS: u32 = 250;

/// A listener that takes longer than this to answer, on the main thread,
/// has cost the user a frame; it is named in the app log so the next
/// "typing blocks" report says which surface.
const LISTENER_BUDGET: std::time::Duration = std::time::Duration::from_millis(12);

pub struct Search {
    pub widget: gtk::Box,
    entry: gtk::SearchEntry,
    ghost: gtk::ToggleButton,
    summary: gtk::Label,
    rule: gtk::LevelBar,
    query: RefCell<Query>,
    generation: Cell<u64>,
    cancel: RefCell<Arc<AtomicBool>>,
    listeners: RefCell<Vec<Listener>>,
    status: RefCell<BTreeMap<&'static str, Status>>,
    steppers: RefCell<HashMap<Panel, Stepper>>,
    panel_hits: RefCell<HashMap<Panel, usize>>,
    stepping: Cell<Panel>,
    last_panel: Cell<Panel>,
    return_focus: RefCell<Option<glib::WeakRef<gtk::Widget>>>,
}

impl Search {
    pub fn new() -> Rc<Self> {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Search everything")
            .tooltip_text(
                "One query, every surface: file names and contents, definitions, the \
                 backlog, branches, commits, terminals and chats. Ctrl+F from anywhere; \
                 Escape clears; Down steps through the panel you came from, Tab moves \
                 to the next panel with results. An uppercase letter makes it \
                 case-sensitive.",
            )
            .search_delay(SEARCH_DELAY_MS)
            // A natural width, not a floor: `width_request(360)` put a
            // 360px minimum in the title bar, and with the header's buttons
            // beside it the WINDOW could not go below 653px — the gadget
            // rung, which exists for a 400px window, was unreachable and
            // its frame came out with the rows cut off the right edge.
            // The entry shrinks with the bar and grows to 360 when there
            // is room.
            .width_chars(8)
            .max_width_chars(34)
            .hexpand(false)
            .build();
        entry.set_widget_name("search");
        // Progress, as one thin rule under the field: the same drawing the
        // gauges use, in the accent colour because it is progress and not a
        // resource. Visible only while something is still looking.
        let rule = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::End)
            .margin_bottom(2)
            .margin_start(9)
            .margin_end(9)
            .can_target(false)
            .visible(false)
            .css_classes(["search-rule"])
            .build();
        rule.set_size_request(-1, 3);
        let overlay = gtk::Overlay::builder().child(&entry).build();
        overlay.add_overlay(&rule);
        let ghost = gtk::ToggleButton::builder()
            .icon_name("taste-ghost-symbolic")
            .tooltip_text(
                "Highlight without filtering: keep every row, dim the ones that do not \
                 match",
            )
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let summary = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "numeric"])
            .xalign(0.0)
            // A natural width the count settles into, not a floor: a
            // 14-character minimum here was the last 100px that kept the
            // title bar — and so the window — from reaching the gadget's
            // 400px. It ellipsizes when the bar is that narrow.
            .max_width_chars(14)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("search-box");
        widget.append(&overlay);
        widget.append(&ghost);
        widget.append(&summary);

        let search = Rc::new(Self {
            widget,
            entry: entry.clone(),
            ghost: ghost.clone(),
            summary,
            rule,
            query: RefCell::new(Query::default()),
            generation: Cell::new(0),
            cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
            listeners: RefCell::new(Vec::new()),
            status: RefCell::new(BTreeMap::new()),
            steppers: RefCell::new(HashMap::new()),
            panel_hits: RefCell::new(HashMap::new()),
            stepping: Cell::new(Panel::Tree),
            last_panel: Cell::new(Panel::Tree),
            return_focus: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&search);
            entry.connect_search_changed(move |entry| {
                if let Some(search) = weak.upgrade() {
                    search.set_text(&entry.text());
                }
            });
        }
        {
            let weak = Rc::downgrade(&search);
            ghost.connect_toggled(move |ghost| {
                let Some(search) = weak.upgrade() else { return };
                let mut query = search.query.borrow().clone();
                if query.ghost == ghost.is_active() {
                    return;
                }
                query.ghost = ghost.is_active();
                search.publish(query);
            });
        }
        {
            // Capture phase: Tab must not leave the field, and Down must
            // not move the entry's own cursor.
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(&search);
            keys.connect_key_pressed(move |_, key, _, state| {
                let Some(search) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                use gtk::gdk::Key;
                let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                match key {
                    Key::Escape => {
                        search.clear();
                        search.return_focus();
                        glib::Propagation::Stop
                    }
                    Key::Down => {
                        search.step(Step::Next);
                        glib::Propagation::Stop
                    }
                    Key::Up => {
                        search.step(Step::Prev);
                        glib::Propagation::Stop
                    }
                    Key::Tab | Key::ISO_Left_Tab => {
                        search.switch_panel(if shift || key == Key::ISO_Left_Tab {
                            -1
                        } else {
                            1
                        });
                        glib::Propagation::Stop
                    }
                    Key::Return | Key::KP_Enter => {
                        search.step(Step::Activate);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
            entry.add_controller(keys);
        }
        search
    }

    /// Every surface that answers the query registers here. The listener
    /// is called on the main thread with the query and its generation;
    /// slow work goes to the blocking pool with [`Search::cancel_token`].
    pub fn subscribe(&self, name: &'static str, listener: impl Fn(&Query, u64) + 'static) {
        self.listeners.borrow_mut().push((name, Box::new(listener)));
    }

    pub fn query(&self) -> Query {
        self.query.borrow().clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// The current query's stop flag. Raised when the next query arrives.
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancel.borrow().clone()
    }

    /// Whether `generation` is still the one being searched.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation
    }

    fn set_text(self: &Rc<Self>, text: &str) {
        let mut query = self.query.borrow().clone();
        if query.text.trim() == text.trim() {
            return;
        }
        query.text = text.trim().to_string();
        self.publish(query);
    }

    fn publish(self: &Rc<Self>, query: Query) {
        self.cancel.borrow().store(true, Ordering::Relaxed);
        *self.cancel.borrow_mut() = Arc::new(AtomicBool::new(false));
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        *self.query.borrow_mut() = query.clone();
        CURRENT.with(|q| *q.borrow_mut() = query.clone());
        self.status.borrow_mut().clear();
        self.panel_hits.borrow_mut().clear();
        self.ghost.set_sensitive(!query.is_empty());
        self.redraw();
        for (name, listener) in self.listeners.borrow().iter() {
            // Each surface answers synchronously here; anything slow in one
            // is a keystroke the user feels. Measured, and named when over
            // budget, because the fix is in the surface, not the box.
            let started = std::time::Instant::now();
            listener(&query, generation);
            let took = started.elapsed();
            if took > LISTENER_BUDGET {
                tracing::info!(
                    surface = name,
                    ms = took.as_millis(),
                    "search: a surface answered the query over budget on the main thread"
                );
            }
        }
    }

    /// Empty the box and the query.
    pub fn clear(self: &Rc<Self>) {
        if !self.entry.text().is_empty() {
            self.entry.set_text("");
        }
        self.set_text("");
    }

    /// A source says where it is. Sources are named so a slow one and a
    /// fast one both show; the summary sums them.
    pub fn report(&self, source: &'static str, status: Status) {
        self.status.borrow_mut().insert(source, status);
        self.redraw();
    }

    fn redraw(&self) {
        let status = self.status.borrow();
        let hits: usize = status.values().map(|s| s.hits).sum();
        let running: Vec<&Status> = status.values().filter(|s| s.running).collect();
        if self.query.borrow().is_empty() {
            self.summary.set_label("");
            self.rule.set_visible(false);
            return;
        }
        let stepping = self.stepping.get();
        let mut text = format!("{hits} hit{}", if hits == 1 { "" } else { "s" });
        if !running.is_empty() {
            text.push_str(" · searching");
        }
        text.push_str(&format!(" · {}", stepping.label()));
        self.summary.set_label(&text);
        if running.is_empty() {
            self.rule.set_visible(false);
        } else {
            let (done, total) = running.iter().fold((0usize, 0usize), |(d, t), s| {
                (d + s.done, t + s.total.max(1))
            });
            self.rule
                .set_value((done as f64 / total.max(1) as f64).clamp(0.0, 1.0));
            self.rule.set_visible(true);
        }
    }

    // --- focus and stepping --------------------------------------------

    /// Ctrl+F: take focus, remembering where it was so Escape can give it
    /// back, and step through the panel the user came from.
    pub fn focus(&self) {
        if let Some(window) = self
            .entry
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok())
        {
            if let Some(widget) = gtk::prelude::GtkWindowExt::focus(&window) {
                if !widget.is_ancestor(&self.widget) && widget != self.entry {
                    *self.return_focus.borrow_mut() = Some(widget.downgrade());
                }
            }
        }
        self.stepping.set(self.last_panel.get());
        self.entry.grab_focus();
        self.redraw();
    }

    fn return_focus(&self) {
        let target = self
            .return_focus
            .borrow_mut()
            .take()
            .and_then(|weak| weak.upgrade());
        if let Some(widget) = target {
            widget.grab_focus();
        }
    }

    /// Panes tell the box which of them focus is in, so Down from the box
    /// steps where the user was.
    pub fn note_panel(&self, panel: Panel) {
        self.last_panel.set(panel);
        if !self.entry.has_focus() {
            self.stepping.set(panel);
        }
    }

    /// A panel that can be stepped through registers how. The stepper
    /// returns whether it had somewhere to step.
    pub fn register_stepper(&self, panel: Panel, stepper: impl Fn(Step) -> bool + 'static) {
        self.steppers.borrow_mut().insert(panel, Box::new(stepper));
    }

    /// How many results a panel has: what Tab skips over.
    pub fn set_panel_hits(&self, panel: Panel, hits: usize) {
        self.panel_hits.borrow_mut().insert(panel, hits);
    }

    fn step(&self, step: Step) {
        let panel = self.stepping.get();
        if let Some(stepper) = self.steppers.borrow().get(&panel) {
            stepper(step);
        }
    }

    /// Tab: the next panel with results, in reading order, wrapping. A
    /// panel with none is skipped — the one the user came from is not
    /// (its listing says "no matches"), which is why stepping starts there.
    fn switch_panel(&self, direction: i32) {
        let order = Panel::ORDER;
        let current = order
            .iter()
            .position(|p| *p == self.stepping.get())
            .unwrap_or(0) as i32;
        let hits = self.panel_hits.borrow();
        let steppers = self.steppers.borrow();
        for offset in 1..=order.len() as i32 {
            let index = (current + direction * offset).rem_euclid(order.len() as i32);
            let candidate = order[index as usize];
            let has_results = hits.get(&candidate).copied().unwrap_or(0) > 0;
            if has_results && steppers.contains_key(&candidate) {
                self.stepping.set(candidate);
                drop(hits);
                drop(steppers);
                self.redraw();
                return;
            }
        }
    }

    /// TASTE_PROBE_CHECK only: pose a query as if typed.
    pub fn seed_for_probe(self: &Rc<Self>, text: &str) {
        self.entry.set_text(text);
        self.set_text(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panels_are_stepped_in_reading_order() {
        assert_eq!(
            Panel::ORDER,
            [Panel::Tree, Panel::Editor, Panel::Console, Panel::Chat]
        );
        assert_eq!(Panel::Tree.label(), "files");
    }
}
