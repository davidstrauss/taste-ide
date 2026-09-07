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

/// A place the semantic index found for the query: the file (absolute),
/// the chunk's lines, how alike, and the chunk's first line to show. What
/// the tree and the editor are handed once the index has answered.
#[derive(Clone, Debug, PartialEq)]
pub struct MeaningHit {
    pub path: std::path::PathBuf,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
    pub text: String,
}

/// Below this cosine similarity a chunk is noise, not an answer: the
/// model's related pairs sit around 0.7 and its unrelated ones around 0.5.
pub const MEANING_FLOOR: f32 = 0.6;

/// The semantic index's progress, for the box: chunks embedded of the
/// chunks to embed, and the estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indexing {
    pub done: usize,
    pub total: usize,
    pub eta: Option<std::time::Duration>,
}

/// The pills: how many, and how found. One vocabulary for every row in
/// the window (David, 2026-09-07: "We should have a system of teal pills
/// … It's [icon-if-any] [count]. An item can have multiple pills on it"):
///   · a bare number — literal hits in the thing itself (a file's lines,
///     an issue's text, a port's title);
///   · the sparkle and a number — places found by meaning, not by the
///     word (David: "use an AI 'star' icon for the meaning results");
///   · a box glyph and a number — hits INSIDE an issue's environment, its
///     chat and terminals, which the box in the title bar always searches
///     and which are the environment's, not the issue's.
/// Each pill is the same teal (main.rs::search_css), and a row wears as
/// many as apply, in that order.
pub struct Counts {
    pub literal: usize,
    pub meaning: usize,
    pub inside: usize,
}

/// The pills for these counts; `None` when there is nothing to wear.
pub fn pills(counts: Counts) -> Option<gtk::Box> {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    row.set_valign(gtk::Align::Center);
    let plural = |n: usize| if n == 1 { "" } else { "es" };
    if counts.literal > 0 {
        row.append(&pill(
            None,
            &counts.literal.to_string(),
            &format!("{} match{}", counts.literal, plural(counts.literal)),
        ));
    }
    if counts.meaning > 0 {
        row.append(&pill(
            Some(MEANING_ICON),
            &counts.meaning.to_string(),
            &format!(
                "{} place{} found by meaning, not by the word",
                counts.meaning,
                if counts.meaning == 1 { "" } else { "s" }
            ),
        ));
    }
    if counts.inside > 0 {
        row.append(&pill(
            Some(INSIDE_ICON),
            &counts.inside.to_string(),
            &format!(
                "{} match{} inside its environment — its chat and terminals; click the \
                 row again to step through them",
                counts.inside,
                plural(counts.inside)
            ),
        ));
    }
    row.first_child().is_some().then_some(row)
}

/// The glyph of the inside-the-environment pill: a box, for the container
/// that was searched.
pub const INSIDE_ICON: &str = "package-x-generic-symbolic";
/// The glyph of the by-meaning pill: the sparkle the toggle wears.
pub const MEANING_ICON: &str = "taste-meaning-symbolic";
/// What the meaning toggle says when there is an index to ask.
const MEANING_TOOLTIP: &str = "Include results by meaning: what the local semantic index finds \
                               for the question, beside the literal hits";

/// The pill's text while the index builds: whole minutes left, rounded up,
/// never under one — a build that is nearly done is still building — and
/// an ellipsis until the plan pass has counted what there is to embed.
pub fn minutes_left(eta: Option<std::time::Duration>) -> String {
    match eta {
        Some(eta) => format!("{}m", (eta.as_secs_f64() / 60.0).ceil().max(1.0) as u64),
        None => "…".to_string(),
    }
}

fn pill(icon: Option<&str>, text: &str, tooltip: &str) -> gtk::Box {
    let pill = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(3)
        .css_classes(["hit-badge"])
        .valign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .build();
    if let Some(icon) = icon {
        pill.append(
            &gtk::Image::builder()
                .icon_name(icon)
                .pixel_size(11)
                .valign(gtk::Align::Center)
                .build(),
        );
    }
    pill.append(
        &gtk::Label::builder()
            .label(text)
            .css_classes(["caption", "numeric"])
            .build(),
    );
    pill
}

/// The bare pill alone — literal hits — for the rows that have only that
/// (a port, a log, a tab's count). One of `pills`' three, never a fourth
/// shape.
pub fn hit_badge(count: usize) -> gtk::Box {
    pills(Counts {
        literal: count,
        meaning: 0,
        inside: 0,
    })
    .unwrap_or_else(|| gtk::Box::new(gtk::Orientation::Horizontal, 0))
}

/// The match-count badge as a picture, for a tab. `AdwTabPage` has an icon
/// and an indicator and nothing else, so while a query stands a tab with
/// hits wears its count where its file-type glyph was (the glyph comes
/// back when the query clears). In the search hue's solid shade
/// (`palette::hit_background`), the one place CSS cannot reach.
/// Drawn at twice the size it is shown at, so
/// it is crisp on a HiDPI display and merely scaled on a plain one. Cached
/// per count and scheme: a count is drawn once.
pub fn badge_texture(count: usize) -> gtk::gdk::Texture {
    thread_local! {
        static CACHE: RefCell<HashMap<(usize, bool), gtk::gdk::Texture>> = RefCell::new(HashMap::new());
    }
    let dark = adw::StyleManager::default().is_dark();
    if let Some(texture) = CACHE.with(|cache| cache.borrow().get(&(count, dark)).cloned()) {
        return texture;
    }
    let text = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };
    let scale = 2.0;
    let height = (16.0 * scale) as i32;
    let width = ((10.0 + 7.0 * text.len() as f64) * scale).max(16.0 * scale) as i32;
    let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, width, height)
        .expect("a surface");
    {
        let cr = gtk::cairo::Context::new(&surface).expect("a context");
        let (w, h) = (f64::from(width), f64::from(height));
        let radius = h / 2.0;
        cr.new_sub_path();
        cr.arc(
            w - radius,
            radius,
            radius,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_2,
        );
        cr.arc(
            radius,
            radius,
            radius,
            std::f64::consts::FRAC_PI_2,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        let bg = crate::palette::rgba(crate::palette::hit_background(dark));
        cr.set_source_rgba(
            f64::from(bg.red()),
            f64::from(bg.green()),
            f64::from(bg.blue()),
            1.0,
        );
        let _ = cr.fill();
        let fg = crate::palette::rgba(crate::palette::hit_foreground(dark));
        cr.set_source_rgba(
            f64::from(fg.red()),
            f64::from(fg.green()),
            f64::from(fg.blue()),
            1.0,
        );
        cr.select_font_face(
            "Cantarell",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Bold,
        );
        cr.set_font_size(10.5 * scale);
        if let Ok(extents) = cr.text_extents(&text) {
            cr.move_to(
                (w - extents.width()) / 2.0 - extents.x_bearing(),
                (h - extents.height()) / 2.0 - extents.y_bearing(),
            );
            let _ = cr.show_text(&text);
        }
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.take_data().expect("the surface's pixels");
    let bytes = glib::Bytes::from(&data[..]);
    let texture = gtk::gdk::MemoryTexture::new(
        width,
        height,
        gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride,
    )
    .upcast::<gtk::gdk::Texture>();
    CACHE.with(|cache| {
        cache.borrow_mut().insert((count, dark), texture.clone());
    });
    texture
}

/// The query as a PCRE2 pattern that matches it literally, for VTE's own
/// search highlight (`Terminal::search_set_regex`).
pub fn literal_pattern(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if r"\.^$|()[]{}*+?-/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The panes that can be stepped through, in the window's reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Panel {
    Editor,
    Files,
    Ports,
    Logs,
    Backlog,
    Terminal,
    Chat,
}

impl Panel {
    /// The order Tab takes, always — the same whatever has results, so the
    /// hand learns it once (David, 2026-09-07: "Lozenges and tab
    /// advancement should always progress in the same order through
    /// panels, even ones without any results … Don't have the state of
    /// projects disrupt muscle memory"). His order: what is under the eye
    /// first, then the flank top to bottom, then the terminal, then the
    /// chat.
    pub const ORDER: [Panel; 7] = [
        Panel::Editor,
        Panel::Files,
        Panel::Ports,
        Panel::Logs,
        Panel::Backlog,
        Panel::Terminal,
        Panel::Chat,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Panel::Editor => "editor",
            Panel::Files => "files",
            Panel::Ports => "ports",
            Panel::Logs => "logs",
            Panel::Backlog => "backlog",
            Panel::Terminal => "terminal",
            Panel::Chat => "chat",
        }
    }

    /// The glyph the section wears elsewhere in the window, so a lozenge
    /// reads as the section without a word.
    pub fn icon(self) -> &'static str {
        match self {
            Panel::Editor => "text-x-generic-symbolic",
            Panel::Files => "folder-symbolic",
            Panel::Ports => "network-server-symbolic",
            Panel::Logs => "view-list-symbolic",
            Panel::Backlog => "view-list-ordered-symbolic",
            Panel::Terminal => "utilities-terminal-symbolic",
            Panel::Chat => "chat-message-new-symbolic",
        }
    }

    /// Tab stop N of 7.
    pub fn stop(self) -> usize {
        Self::ORDER.iter().position(|p| *p == self).unwrap_or(0) + 1
    }
}

/// Steps the rows of a `GtkListBox` a search filters — the ports, the logs,
/// the backlog — lighting the row it is on in the search's hue rather than
/// selecting it: selection in those lists means something else (the row
/// open in the editor; the environment the panes are aimed at), and a
/// search stepping through must not move either. Rows the query hid or
/// dimmed are passed over.
pub struct ListStepper {
    list: gtk::ListBox,
    at: Cell<Option<i32>>,
}

impl ListStepper {
    pub fn new(list: &gtk::ListBox) -> Rc<Self> {
        Rc::new(Self {
            list: list.clone(),
            at: Cell::new(None),
        })
    }

    fn candidates(&self) -> Vec<gtk::ListBoxRow> {
        let mut rows = Vec::new();
        let mut child = self.list.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            let Ok(row) = widget.downcast::<gtk::ListBoxRow>() else {
                continue;
            };
            if !row.is_visible() || !row.is_activatable() {
                continue;
            }
            let dimmed = row
                .child()
                .is_some_and(|child| child.has_css_class("search-dim"))
                || row.has_css_class("search-dim");
            if !dimmed {
                rows.push(row);
            }
        }
        rows
    }

    pub fn step(&self, step: Step) -> bool {
        let rows = self.candidates();
        if rows.is_empty() {
            return false;
        }
        let current = self
            .at
            .get()
            .and_then(|index| self.list.row_at_index(index))
            .and_then(|row| rows.iter().position(|r| *r == row));
        match step {
            Step::Activate => {
                if let Some(row) = current.and_then(|i| rows.get(i)) {
                    row.activate();
                    return true;
                }
                false
            }
            Step::Next | Step::Prev => {
                let next = match (current, step) {
                    (None, Step::Next) => 0,
                    (None, _) => rows.len() - 1,
                    (Some(i), Step::Next) => {
                        if i + 1 >= rows.len() {
                            return false;
                        }
                        i + 1
                    }
                    (Some(i), _) => {
                        if i == 0 {
                            return false;
                        }
                        i - 1
                    }
                };
                if let Some(previous) = current.and_then(|i| rows.get(i)) {
                    previous.remove_css_class("search-hit");
                }
                let row = &rows[next];
                row.add_css_class("search-hit");
                self.at.set(Some(row.index()));
                // Into view, without taking focus off the box.
                if let Some(scroller) = row
                    .ancestor(gtk::ScrolledWindow::static_type())
                    .and_downcast::<gtk::ScrolledWindow>()
                {
                    if let Some(bounds) = row.compute_bounds(&scroller) {
                        let adjustment = scroller.vadjustment();
                        let top = f64::from(bounds.y());
                        let bottom = top + f64::from(bounds.height());
                        if top < 0.0 {
                            adjustment.set_value(adjustment.value() + top);
                        } else if bottom > adjustment.page_size() {
                            adjustment
                                .set_value(adjustment.value() + bottom - adjustment.page_size());
                        }
                    }
                }
                true
            }
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
    meaning: gtk::ToggleButton,
    /// The Tab strip: the keycap and one lozenge per stop, hidden with the
    /// query (and by the rungs that have no room — `Search::summary`).
    summary: gtk::Box,
    strip: gtk::Box,
    /// Each stop's lozenge and its count, in `Panel::ORDER`.
    stops: Vec<(Panel, gtk::Box, gtk::Label)>,
    /// Each section's "No matches" banner (results.rs), lit when the stop
    /// is the current one and has nothing else to light.
    placeholders: RefCell<HashMap<Panel, std::rc::Weak<crate::results::ResultsPanel>>>,
    /// The semantic index being built, beside the box: the utilization
    /// gauge's drawing (`gauge.rs`) in the search's ink, and the time left.
    /// The minutes left on the meaning button while the index builds.
    index_pill: gtk::Label,
    /// The index is building: the meaning button stays disabled whatever
    /// the query says.
    indexing: Cell<bool>,
    /// The two above in one box, so the narrow rungs can hide the pair
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
    /// The microphone, while Ctrl+F or Start is held.
    dictation: RefCell<Option<taste_voice::Recorder>>,
    on_notice: RefCell<Option<Box<dyn Fn(String)>>>,
    return_focus: RefCell<Option<glib::WeakRef<gtk::Widget>>>,
}

impl Search {
    pub fn new() -> Rc<Self> {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Find everything")
            .tooltip_text(
                "One query, every surface: file names and contents, definitions, the \
                 backlog, branches, commits, terminals and chats. Ctrl+F from anywhere; \
                 Escape clears; Down steps through the panel you came from, Tab hops \
                 to the next panel with results and onto its next hit. An uppercase \
                 letter makes it case-sensitive.",
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
            .build();
        // The Tab strip: a keycap, then one lozenge per section in the order
        // Tab takes them, every section whether or not it has matches, the
        // current one filled (David, 2026-09-07: "show a 'tab' key icon
        // followed by teal lozenges for each section that I can tab
        // through. If I tab to one of those sections, highlight that tab
        // without altering any positioning"). Each lozenge is a fixed
        // width, so neither a count nor the highlight moves its neighbours
        // — or the entry, which the header bar centres with this widget.
        // It replaces a summary that said "113 hits · files": the count
        // per section is the count, and the section it names is lit.
        let tab_key = gtk::ShortcutLabel::new("Tab");
        tab_key.set_valign(gtk::Align::Center);
        tab_key.add_css_class("tab-key");
        tab_key.set_tooltip_text(Some(
            "Tab and Shift+Tab step through the sections in this order, always — \
             editor, files, ports, logs, backlog, terminal, chat",
        ));
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        strip.append(&tab_key);
        let mut stops = Vec::new();
        for panel in Panel::ORDER {
            let count = gtk::Label::builder()
                .label("0")
                .width_chars(3)
                .xalign(0.5)
                .css_classes(["caption", "numeric"])
                .build();
            let lozenge = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(3)
                .valign(gtk::Align::Center)
                .css_classes(["hit-badge", "tab-stop", "tab-stop-empty"])
                .build();
            lozenge.append(
                &gtk::Image::builder()
                    .icon_name(panel.icon())
                    .pixel_size(11)
                    .valign(gtk::Align::Center)
                    .build(),
            );
            lozenge.append(&count);
            strip.append(&lozenge);
            stops.push((panel, lozenge, count));
        }
        strip.set_visible(false);
        let summary = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        summary.append(&strip);
        // Results by meaning, beside the ghost: what the semantic index
        // finds joins the literal hits — a file the word is not in but the
        // idea is, a chunk of the file on screen — until this says not to
        // (David, 2026-09-07: "put a toggle next to the ghost to
        // include/exclude ML-based results").
        // While the index builds, the button is the indicator: disabled,
        // with a grey pill saying how many minutes are left, the chunk
        // count in its tooltip (David, 2026-09-07: "show that instead as a
        // disabled AI button with a gray pill showing simply 'Nm' … The
        // tool tip can provide chunk progress details"). A gauge beside the
        // box did this before, and was the first thing the narrow rungs had
        // to hide.
        let index_pill = gtk::Label::builder()
            .css_classes(["index-pill", "numeric"])
            .visible(false)
            .build();
        let meaning_face = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        meaning_face.append(&gtk::Image::from_icon_name(MEANING_ICON));
        meaning_face.append(&index_pill);
        let meaning = gtk::ToggleButton::builder()
            .child(&meaning_face)
            .tooltip_text(MEANING_TOOLTIP)
            .css_classes(["flat"])
            .active(true)
            .build();
        // The toggles BEFORE the box and the Tab strip after it: what
        // shapes the query on one side, where its results are on the other
        // (David, 2026-09-07: "move the ghost and AI filter buttons to the
        // left of the search box").
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("search-box");
        widget.append(&ghost);
        widget.append(&meaning);
        widget.append(&overlay);
        widget.append(&summary);

        let search = Rc::new(Self {
            widget,
            entry: entry.clone(),
            meaning: meaning.clone(),
            summary,
            strip,
            stops,
            placeholders: RefCell::new(HashMap::new()),
            index_pill,
            indexing: Cell::new(false),
            rule,
            query: RefCell::new(Query::default()),
            generation: Cell::new(0),
            cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
            listeners: RefCell::new(Vec::new()),
            status: RefCell::new(BTreeMap::new()),
            steppers: RefCell::new(HashMap::new()),
            panel_hits: RefCell::new(HashMap::new()),
            stepping: Cell::new(Panel::Editor),
            last_panel: Cell::new(Panel::Editor),
            dictation: RefCell::new(None),
            on_notice: RefCell::new(None),
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
            let weak = Rc::downgrade(&search);
            meaning.connect_toggled(move |meaning| {
                let Some(search) = weak.upgrade() else { return };
                let mut query = search.query.borrow().clone();
                if query.meaning == meaning.is_active() {
                    return;
                }
                query.meaning = meaning.is_active();
                search.publish(query);
            });
        }
        // A lozenge is the stop it names: a click steps there.
        for (panel, lozenge, _) in &search.stops {
            let weak = Rc::downgrade(&search);
            let panel = *panel;
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                if let Some(search) = weak.upgrade() {
                    search.jump_to_panel(panel);
                }
            });
            lozenge.add_controller(click);
            lozenge.set_cursor_from_name(Some("pointer"));
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
                    // The same hop a listing's Tab makes: the next panel
                    // with results, its next hit selected and the keyboard
                    // on it. It used to move the stepping only and keep the
                    // keyboard here (David, 2026-09-07: "'Tab' from the
                    // search input should hop through the results, same as
                    // tab from one of the listings").
                    Key::Tab | Key::ISO_Left_Tab => {
                        search.switch_panel_and_step(if shift || key == Key::ISO_Left_Tab {
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

    /// The field itself, for the key reveal to point at.
    pub fn entry(&self) -> &gtk::SearchEntry {
        &self.entry
    }

    /// The Tab strip beside the box, for the rung that has no room for it.
    pub fn summary(&self) -> &gtk::Box {
        &self.summary
    }

    /// A section's "No matches" banner: what is lit when the stop is the
    /// current one and there is no row to light.
    pub fn register_placeholder(&self, panel: Panel, results: &Rc<crate::results::ResultsPanel>) {
        self.placeholders
            .borrow_mut()
            .insert(panel, Rc::downgrade(results));
    }

    /// Step straight to a section — a lozenge clicked.
    pub fn jump_to_panel(&self, panel: Panel) {
        self.stepping.set(panel);
        self.redraw();
        if !self.step_now(Step::Next) {
            self.step_now(Step::Prev);
        }
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
        self.sync_meaning_sensitivity();
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

    /// The semantic index's progress beside the box, or nothing when no
    /// index is building. The gauge shows the fraction; the caption says
    /// how long is left, which is what a person waiting wants to know
    /// (David, 2026-09-07: "make it clear that the indexing is occurring
    /// with estimated remaining time").
    pub fn set_indexing(&self, indexing: Option<Indexing>) {
        let Some(indexing) = indexing else {
            self.indexing.set(false);
            self.index_pill.set_visible(false);
            self.meaning.set_tooltip_text(Some(MEANING_TOOLTIP));
            self.sync_meaning_sensitivity();
            return;
        };
        self.indexing.set(true);
        self.index_pill.set_label(&minutes_left(indexing.eta));
        self.index_pill.set_visible(true);
        self.meaning.set_tooltip_text(Some(&format!(
            "Indexing this checkout for search by meaning: {} of {} chunks embedded{}. \
             Results by meaning arrive when it finishes; literal search works now.",
            indexing.done,
            indexing.total,
            match indexing.eta {
                Some(eta) if eta.as_secs() >= 90 => format!(
                    ", about {} minutes left",
                    (eta.as_secs_f64() / 60.0).round().max(1.0) as u64
                ),
                Some(eta) => format!(", about {} seconds left", eta.as_secs().max(1)),
                None => ", estimating how long".to_string(),
            }
        )));
        self.sync_meaning_sensitivity();
    }

    /// The meaning button takes a click whenever there is an index to ask —
    /// before any text is typed too, since the toggle is how the next query
    /// is asked (David, 2026-09-07: "I should be able to toggle AI search
    /// before entering text").
    fn sync_meaning_sensitivity(&self) {
        self.meaning.set_sensitive(!self.indexing.get());
    }

    /// TASTE_PROBE_CHECK only: the index mid-build, so the frame shows the
    /// pill.
    pub fn seed_indexing_for_probe(&self) {
        self.set_indexing(Some(Indexing {
            done: 1_290,
            total: 3_257,
            eta: Some(std::time::Duration::from_secs(190)),
        }));
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
        let idle = self.query.borrow().is_empty();
        self.strip.set_visible(!idle);
        let stepping = self.stepping.get();
        {
            let counts = self.panel_hits.borrow();
            let placeholders = self.placeholders.borrow();
            for (panel, lozenge, count) in &self.stops {
                let n = counts.get(panel).copied().unwrap_or(0);
                count.set_label(&if n < 1000 {
                    n.to_string()
                } else {
                    "1k+".to_string()
                });
                let current = !idle && *panel == stepping;
                if current {
                    lozenge.add_css_class("tab-stop-current");
                } else {
                    lozenge.remove_css_class("tab-stop-current");
                }
                if n == 0 {
                    lozenge.add_css_class("tab-stop-empty");
                } else {
                    lozenge.remove_css_class("tab-stop-empty");
                }
                lozenge.set_tooltip_text(Some(&format!(
                    "{}: {n} match{} · Tab stop {} of {}{}",
                    panel.label(),
                    if n == 1 { "" } else { "es" },
                    panel.stop(),
                    Panel::ORDER.len(),
                    if current { " · here" } else { "" }
                )));
                if let Some(results) = placeholders.get(panel).and_then(std::rc::Weak::upgrade) {
                    results.set_current(current);
                }
            }
        }
        if idle {
            self.rule.set_visible(false);
            return;
        }
        let _ = hits;
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

    /// How many results a panel has: its lozenge's count. Tab does not skip
    /// a panel with none.
    pub fn set_panel_hits(&self, panel: Panel, hits: usize) {
        self.panel_hits.borrow_mut().insert(panel, hits);
        self.redraw();
    }

    /// Down, Up and Enter: the panel being stepped takes it. Public for the
    /// controller's D-pad and A (window.rs).
    pub fn step(&self, step: Step) {
        let panel = self.stepping.get();
        if let Some(stepper) = self.steppers.borrow().get(&panel) {
            stepper(step);
        }
    }

    /// Who hears what the box has to say aloud (a toast): the window.
    pub fn set_on_notice(&self, hook: impl Fn(String) + 'static) {
        *self.on_notice.borrow_mut() = Some(Box::new(hook));
    }

    fn notice(&self, text: &str) {
        if let Some(hook) = self.on_notice.borrow().as_ref() {
            hook(text.to_string());
        }
    }

    /// Speech to search (David, 2026-09-07: "use Ctrl-F held more than
    /// briefly as a 'speech to search' option … treated as completely
    /// fresh input to the search, replacing whatever was there"). Start
    /// listening; `stop_dictation` transcribes and makes the words the
    /// query. The box glows in the hue while it listens.
    pub fn start_dictation(self: &Rc<Self>) {
        if self.dictation.borrow().is_some() {
            return;
        }
        match crate::voice::readiness() {
            crate::voice::Readiness::Ready => {}
            crate::voice::Readiness::Downloading => {
                self.notice("the speech model is still downloading");
                return;
            }
            crate::voice::Readiness::Absent => {
                self.notice(
                    "no speech model yet — dictate into the composer once (hold Ctrl+D) \
                     to fetch it",
                );
                return;
            }
        }
        match taste_voice::Recorder::start() {
            Ok(recorder) => {
                *self.dictation.borrow_mut() = Some(recorder);
                self.entry.add_css_class("listening");
            }
            Err(e) => self.notice(&format!("{e:#}")),
        }
    }

    pub fn stop_dictation(self: &Rc<Self>) {
        let Some(recorder) = self.dictation.borrow_mut().take() else {
            return;
        };
        self.entry.remove_css_class("listening");
        let samples = recorder.stop();
        if !taste_voice::has_speech(&samples) {
            return;
        }
        let weak = Rc::downgrade(self);
        crate::voice::transcribe(samples, move |result| {
            let Some(search) = weak.upgrade() else { return };
            match result {
                Ok(text) if !text.trim().is_empty() => {
                    // The words join the query at the cursor, spaced as a
                    // typist would, with the cursor after them; a double
                    // tap is how the box is emptied first (David,
                    // 2026-09-08: "append to existing text at the cursor
                    // position").
                    let mut position = search.entry.position();
                    let existing = search.entry.text();
                    let before: String = existing.chars().take(position.max(0) as usize).collect();
                    let after: String = existing.chars().skip(position.max(0) as usize).collect();
                    let mut spoken = String::new();
                    if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        spoken.push(' ');
                    }
                    spoken.push_str(text.trim());
                    if after.chars().next().is_some_and(|c| !c.is_whitespace()) {
                        spoken.push(' ');
                    }
                    search.entry.insert_text(&spoken, &mut position);
                    search.entry.set_position(position);
                    search.set_text(&search.entry.text());
                    search.entry.grab_focus();
                    search.entry.select_region(position, position);
                }
                Ok(_) => {}
                Err(e) => search.notice(&format!("could not transcribe: {e}")),
            }
        });
    }

    /// Tab: the next section in `Panel::ORDER`, wrapping — with or without
    /// results. A section with none is still a stop, and its banner lights
    /// up to say the stop was taken; skipping it would make the count of
    /// presses to reach a section depend on what the query found.
    fn switch_panel(&self, direction: i32) -> bool {
        let order = Panel::ORDER;
        let current = order
            .iter()
            .position(|p| *p == self.stepping.get())
            .unwrap_or(0) as i32;
        let index = (current + direction).rem_euclid(order.len() as i32);
        self.stepping.set(order[index as usize]);
        self.redraw();
        true
    }

    /// Tab from INSIDE a listing: the next panel with results, and the
    /// focus goes with the stepping — that panel's next hit is selected
    /// (its first, if none was; its last, past the end), which is what puts
    /// the keyboard in the list Tab moved to. From the box, Tab only moves
    /// the stepping and the box keeps the keyboard; from a list, the user
    /// has already left the box for the results, and Tab should carry them
    /// to the next results rather than to whatever GTK's focus chain has
    /// next (David, 2026-09-06: "tab should take my focus to the next
    /// results list after I've started stepping through a specific list").
    pub fn switch_panel_and_step(&self, direction: i32) {
        if !self.switch_panel(direction) {
            return;
        }
        if !self.step_now(Step::Next) {
            self.step_now(Step::Prev);
        }
    }

    /// Whether the search box itself has the keyboard — a stepper that
    /// takes focus for a hit must not take it from the box while the user
    /// is typing there.
    pub fn box_has_focus(&self) -> bool {
        self.entry.has_focus()
    }

    fn step_now(&self, step: Step) -> bool {
        let panel = self.stepping.get();
        match self.steppers.borrow().get(&panel) {
            Some(stepper) => stepper(step),
            None => false,
        }
    }

    /// Put Tab and Shift+Tab on a listing (or the widget that holds it):
    /// from inside one, they move the stepping and the focus to the next
    /// panel with results (`switch_panel_and_step`) instead of to whatever
    /// GTK's focus chain has next. Capture phase, so a list's own Tab
    /// handling never sees it.
    pub fn tab_switches_panels(widget: &impl IsA<gtk::Widget>, search: &Rc<Search>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(search);
        keys.connect_key_pressed(move |_, key, _, state| {
            use gtk::gdk::Key;
            if !matches!(key, Key::Tab | Key::ISO_Left_Tab) {
                return glib::Propagation::Proceed;
            }
            let Some(search) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let back =
                key == Key::ISO_Left_Tab || state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            search.switch_panel_and_step(if back { -1 } else { 1 });
            glib::Propagation::Stop
        });
        widget.add_controller(keys);
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
    fn the_pill_counts_whole_minutes_up_and_never_under_one() {
        use std::time::Duration;
        assert_eq!(minutes_left(Some(Duration::from_secs(190))), "4m");
        assert_eq!(minutes_left(Some(Duration::from_secs(600))), "10m");
        assert_eq!(minutes_left(Some(Duration::from_secs(5))), "1m");
        assert_eq!(minutes_left(None), "…");
    }

    #[test]
    fn panels_are_stepped_in_one_fixed_order() {
        assert_eq!(
            Panel::ORDER,
            [
                Panel::Editor,
                Panel::Files,
                Panel::Ports,
                Panel::Logs,
                Panel::Backlog,
                Panel::Terminal,
                Panel::Chat,
            ]
        );
        assert_eq!(Panel::Files.label(), "files");
        assert_eq!(Panel::Editor.stop(), 1);
        assert_eq!(Panel::Chat.stop(), 7);
    }
}
