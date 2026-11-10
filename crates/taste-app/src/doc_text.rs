//! A Markdown document's preview as one text: a read-only text view whose
//! buffer the document is written into, its headings, emphasis, lists,
//! quotes, links, and code as tags, and what is not text — a picture, a
//! diagram, a table, a rule, a code block's copy button — as widgets at
//! anchors in it.
//!
//! The preview was a column of labels, and a label selects only within
//! itself: a selection could not run from one block into the next, nor
//! scroll the view under a drag (David, 2026-11-09: "I can't select long
//! spans of text"), and a label given the keyboard selected all of itself.
//! A text view is GTK's own answer to a document that flows, scrolls, and
//! selects as a whole — the drag and its scrolling, Shift with the arrows,
//! a double or triple click, Ctrl+A, Ctrl+C, the menu's Copy, the primary
//! selection, and what a screen reader is told are all its own — and it
//! draws only the lines on screen, so a long document no longer has to be
//! cut into short labels to be selected in quickly.
//!
//! The overview strip beside it is the document as it is drawn, scaled
//! to the strip (`crate::preview_map`), painted from a second view of the
//! same buffer at the preview's width and its whole height, which nobody
//! sees: its pictures, diagrams, and tables are the preview's own, painted
//! again ([`Mirror`]). GtkSourceView's map was tried first and is the
//! wrong tool here: it sets the buffer in a one-point font in a column a
//! few dozen pixels wide, so every paragraph char-wrapped into a streak of
//! dots, the code's margins pushed it off the strip, and a picture was a
//! line (David, 2026-11-09: "the sourcemap on this is highly broken").
//!
//! The view is a subclass of GTK's, [`DocTextView`], which draws a code
//! block's card
//! and a quote's bar behind the text, as GtkSourceView draws its own
//! highlights — a paragraph's background starts at its own margin, so a
//! code block's text could not stand in from the card it is on.
//!
//! A table stays a widget: cells that wrap each in its own column are not
//! something a buffer's lines can hold. A selection through one leaves its
//! cells out of what it copies; each cell selects on its own.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::markdown_view::{DocumentBase, Readiness};

/// The view's margin, all round.
const MARGIN: i32 = 16;

/// How far a code block's text stands in from the column, each side.
const CODE_INSET: i32 = 12;

/// How far each list or quote level stands in.
const LEVEL_INDENT: i32 = 22;

/// What a click on a span does.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    /// A link: a URL to open, or a path of the checkout to open.
    Href(String),
    /// A code span: its text to the clipboard.
    Copy(String),
}

/// A document written into a buffer, not yet shown.
pub struct Rendering {
    buffer: gtk::TextBuffer,
    decor: Vec<Decor>,
    actions: HashMap<gtk::TextTag, Action>,
    children: Vec<(gtk::TextChildAnchor, Column)>,
    styles: Styles,
}

/// The tags whose colours follow the style: set again when it turns dark
/// or light.
#[derive(Clone)]
struct Styles {
    link: gtk::TextTag,
    code_span: gtk::TextTag,
    quote: gtk::TextTag,
}

impl Styles {
    fn paint(&self) {
        let style = adw::StyleManager::default();
        let dark = style.is_dark();
        let ink = |alpha: f32| {
            if dark {
                gtk::gdk::RGBA::new(1.0, 1.0, 1.0, alpha)
            } else {
                gtk::gdk::RGBA::new(0.0, 0.0, 0.0, alpha)
            }
        };
        self.link
            .set_foreground_rgba(Some(&style.accent_color().to_standalone_rgba(dark)));
        self.code_span.set_background_rgba(Some(&ink(0.10)));
        self.quote.set_foreground_rgba(Some(&ink(0.62)));
    }
}

/// What is shown now.
struct Shown {
    actions: HashMap<gtk::TextTag, Action>,
    children: Vec<(gtk::TextChildAnchor, Column)>,
    /// Each child's stand-in in the strip's view.
    mirrors: Vec<Mirror>,
    styles: Styles,
}

/// The preview's face: the text view and its map, side by side.
pub struct DocView {
    pub body: gtk::Box,
    pub view: DocTextView,
    pub scroller: gtk::ScrolledWindow,
    /// The view the strip paints: the same buffer, the whole of it.
    ghost: DocTextView,
    map: crate::preview_map::PreviewMap,
    shown: RefCell<Option<Shown>>,
    /// Which rendering is the latest: one overtaken by a newer edit before
    /// it was whole is dropped rather than shown late.
    generation: Cell<u64>,
    on_link: RefCell<Option<Rc<dyn Fn(&str)>>>,
    base: RefCell<Option<DocumentBase>>,
}

impl DocView {
    /// The face, empty, with a strip `map_width` wide beside it.
    pub fn new(map_width: i32) -> Rc<Self> {
        let view = DocTextView::new();
        view.set_hexpand(true);
        view.set_vexpand(true);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .vexpand(true)
            .build();
        // The strip's view: not scrolled, so as tall as the document; the
        // preview's width, so its lines break where the preview's do; and
        // out of reach of the pointer, the keyboard, and a screen reader.
        let ghost = DocTextView::new();
        ghost.set_can_target(false);
        ghost.set_can_focus(false);
        ghost.update_state(&[gtk::accessible::State::Hidden(true)]);
        let width_of = {
            let view = view.downgrade();
            Rc::new(move || view.upgrade().map_or(1, |view| view.width().max(1)))
        };
        let map = crate::preview_map::PreviewMap::with_ghost(
            ghost.upcast_ref(),
            width_of,
            &scroller,
            map_width,
        );
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&scroller);
        body.append(&map);
        let this = Rc::new(Self {
            body,
            view,
            scroller,
            ghost,
            map,
            shown: RefCell::new(None),
            generation: Cell::new(0),
            on_link: RefCell::new(None),
            base: RefCell::new(None),
        });
        this.install_links();
        // The pictures, diagrams, and tables are the column wide: when the
        // column's width changes they are measured again.
        {
            let weak = Rc::downgrade(&this);
            let width = Cell::new(0);
            this.scroller
                .hadjustment()
                .connect_changed(move |adjustment| {
                    let now = adjustment.page_size() as i32;
                    if width.replace(now) == now {
                        return;
                    }
                    // The strip's view follows the preview's width.
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(this) = weak.upgrade() {
                            this.map.queue_allocate();
                            this.measure_children();
                        }
                    });
                });
        }
        {
            let weak = Rc::downgrade(&this);
            adw::StyleManager::default().connect_dark_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    if let Some(shown) = this.shown.borrow().as_ref() {
                        shown.styles.paint();
                    }
                    this.map.refresh();
                }
            });
        }
        crate::viewport_matches::watch(&this.view);
        this
    }

    /// Show `text`. The first rendering shows at once and fills in; a
    /// later one is built beside what is on screen and takes its place in
    /// one step once its pictures and diagrams are in, or after `wait`,
    /// whichever is first — so an edit or a reload never shows the page
    /// torn down or jumping (David, 2026-10-09: "Ensure Markdown preview
    /// updates get double-buffered to avoid jank").
    pub fn present(
        self: &Rc<Self>,
        text: &str,
        on_link: Rc<dyn Fn(&str)>,
        base: DocumentBase,
        wait: std::time::Duration,
    ) {
        *self.on_link.borrow_mut() = Some(on_link);
        *self.base.borrow_mut() = Some(base.clone());
        let readiness = Readiness::default();
        let rendering = render(&self.view, text, &base, &readiness);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let first = self.shown.borrow().is_none();
        let pending = Rc::new(RefCell::new(Some(rendering)));
        let swap: Rc<dyn Fn()> = {
            let weak = Rc::downgrade(self);
            let pending = pending.clone();
            Rc::new(move || {
                let Some(this) = weak.upgrade() else { return };
                if this.generation.get() != generation {
                    return;
                }
                if let Some(rendering) = pending.borrow_mut().take() {
                    this.swap(rendering, !first);
                }
            })
        };
        if first {
            swap();
            // What came in after it was shown is measured again.
            let weak = Rc::downgrade(self);
            readiness.when_ready(move || {
                if let Some(this) = weak.upgrade() {
                    if this.generation.get() == generation {
                        this.measure_children();
                    }
                }
            });
        } else {
            let on_ready = swap.clone();
            readiness.when_ready(move || on_ready());
            glib::timeout_add_local_once(wait, move || swap());
        }
    }

    /// Put `rendering` in place of what is shown, keeping the reader's
    /// place by line when `keep_place`.
    fn swap(&self, rendering: Rendering, keep_place: bool) {
        let place = keep_place.then(|| {
            let rect = self.view.visible_rect();
            let (top, _) = self.view.line_at_y(rect.y());
            top.line()
        });
        if let Some(shown) = self.shown.borrow_mut().take() {
            for (_, child) in shown.children {
                self.view.remove(&child);
            }
            for mirror in shown.mirrors {
                self.ghost.remove(&mirror);
            }
        }
        let Rendering {
            buffer,
            decor,
            actions,
            children,
            styles,
        } = rendering;
        styles.paint();
        self.view.set_buffer(Some(&buffer));
        self.ghost.set_buffer(Some(&buffer));
        self.follow_in_strip(&buffer);
        *self.view.imp().decor.borrow_mut() = decor.clone();
        *self.ghost.imp().decor.borrow_mut() = decor;
        let mut mirrors = Vec::with_capacity(children.len());
        for (anchor, child) in &children {
            self.view.add_child_at_anchor(child, anchor);
            let mirror = Mirror::new(child);
            self.ghost.add_child_at_anchor(&mirror, anchor);
            mirrors.push(mirror);
        }
        *self.shown.borrow_mut() = Some(Shown {
            actions,
            children,
            mirrors,
            styles,
        });
        if let Some(line) = place.filter(|line| *line > 0) {
            if let Some(iter) = buffer.iter_at_line(line) {
                let mark = buffer.create_mark(None, &iter, true);
                self.view.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
                buffer.delete_mark(&mark);
            }
        }
    }

    /// The strip drawn again when what the ghost shows of `buffer` changes:
    /// its text, its tags (the find's marks, a widget's line laid out
    /// again), or its selection. Nothing draws the ghost but the strip, so
    /// nothing else would say so.
    fn follow_in_strip(&self, buffer: &gtk::TextBuffer) {
        let map = self.map.downgrade();
        let refresh = move || {
            if let Some(map) = map.upgrade() {
                map.refresh();
            }
        };
        let on = refresh.clone();
        buffer.connect_changed(move |_| on());
        let on = refresh.clone();
        buffer.connect_apply_tag(move |_, _, _, _| on());
        let on = refresh.clone();
        buffer.connect_remove_tag(move |_, _, _, _| on());
        buffer.connect_mark_set(move |buffer, _, mark| {
            if *mark == buffer.selection_bound() || *mark == buffer.get_insert() {
                refresh();
            }
        });
    }

    /// Have the view measure its widgets again: the column's width
    /// changed, or what they hold came in.
    fn measure_children(&self) {
        let shown = self.shown.borrow();
        let Some(shown) = shown.as_ref() else { return };
        let buffer = self.view.buffer();
        let nudge = buffer.tag_table().lookup(NUDGE);
        for mirror in &shown.mirrors {
            mirror.queue_resize();
        }
        for (anchor, child) in &shown.children {
            child.queue_resize();
            // A line is laid out again when its tags change, and its
            // widget measured with it.
            if let Some(nudge) = &nudge {
                let start = buffer.iter_at_child_anchor(anchor);
                let mut end = start;
                end.forward_char();
                buffer.apply_tag(nudge, &start, &end);
                buffer.remove_tag(nudge, &start, &end);
            }
        }
    }

    /// What a link or a code span is under `(x, y)`, the view's own point.
    fn action_at(&self, x: f64, y: f64) -> Option<Action> {
        let (bx, by) =
            self.view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let iter = self.view.iter_at_location(bx, by)?;
        let shown = self.shown.borrow();
        let shown = shown.as_ref()?;
        iter.tags()
            .iter()
            .find_map(|tag| shown.actions.get(tag).cloned())
    }

    /// Links and code spans as GTK's hypertext demo has them: a click on
    /// one (not the end of a drag) acts, the pointer over one is a hand,
    /// and its tooltip says where it goes or that it copies.
    fn install_links(self: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        {
            let weak = Rc::downgrade(self);
            click.connect_released(move |gesture, presses, x, y| {
                let Some(this) = weak.upgrade() else { return };
                if presses != 1 || this.view.buffer().has_selection() {
                    return;
                }
                if let Some(action) = this.action_at(x, y) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.act(&action);
                }
            });
        }
        self.view.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        {
            let weak = Rc::downgrade(self);
            let over = Cell::new(false);
            motion.connect_motion(move |_, x, y| {
                let Some(this) = weak.upgrade() else { return };
                let now = this.action_at(x, y).is_some();
                if over.replace(now) != now {
                    this.view
                        .set_cursor_from_name(Some(if now { "pointer" } else { "text" }));
                }
            });
        }
        self.view.add_controller(motion);

        self.view.set_has_tooltip(true);
        let weak = Rc::downgrade(self);
        self.view
            .connect_query_tooltip(move |_, x, y, keyboard, tooltip| {
                let Some(this) = weak.upgrade() else {
                    return false;
                };
                if keyboard {
                    return false;
                }
                match this.action_at(f64::from(x), f64::from(y)) {
                    Some(Action::Href(href)) => {
                        tooltip.set_text(Some(&href));
                        true
                    }
                    Some(Action::Copy(_)) => {
                        tooltip.set_text(Some("Click to copy"));
                        true
                    }
                    None => false,
                }
            });
    }

    fn act(&self, action: &Action) {
        match action {
            Action::Copy(text) => {
                if crate::copy::whole(&self.view, text).is_ok() {
                    crate::copy::toast(self.view.upcast_ref(), "Copied");
                }
            }
            Action::Href(href) => {
                if href.starts_with("http://") || href.starts_with("https://") {
                    if let Some(on_link) = self.on_link.borrow().as_ref() {
                        on_link(href);
                    }
                    return;
                }
                // A path of this checkout: the file, in the editor.
                // Anything else goes nowhere.
                let base = self.base.borrow().clone();
                if let Some(base) = base {
                    if let Some((path, at)) = base.resolve_link(href) {
                        (base.open_file)(path, at);
                    }
                }
            }
        }
    }

    /// The probe's measure of selecting in it: the whole document selected
    /// from its start further each frame, 240 frames to its end, as a drag
    /// grows a selection, with each frame's work and the time between
    /// frames printed (`TASTE_PROBE_SELECT`).
    #[doc(hidden)]
    pub fn select_bench(&self, no_map: bool) {
        use std::time::Instant;
        if no_map {
            self.map.set_visible(false);
        }
        let buffer = self.view.buffer();
        let Some(clock) = self.view.frame_clock() else {
            println!("select bench: no frame clock");
            return;
        };
        let chars = buffer.char_count();
        println!(
            "select bench: a text view of {chars} characters, {} lines",
            buffer.line_count()
        );
        let begun: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let last: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let frames: Rc<RefCell<Vec<(f64, f64)>>> = Rc::new(RefCell::new(Vec::new()));
        let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
        let update = {
            let begun = begun.clone();
            clock.connect_update(move |_| begun.set(Some(Instant::now())))
        };
        let after = {
            let (begun, last, frames) = (begun.clone(), last.clone(), frames.clone());
            clock.connect_after_paint(move |_| {
                let now = Instant::now();
                if let Some(start) = begun.take() {
                    let gap = last.get().map(|end| ms(end, now)).unwrap_or(0.0);
                    frames.borrow_mut().push((ms(start, now), gap));
                }
                last.set(Some(now));
            })
        };
        let handlers = RefCell::new(Some([update, after]));
        let step = Cell::new(0);
        self.view.add_tick_callback(move |view, clock| {
            let buffer = view.buffer();
            let n = step.get() + 1;
            step.set(n);
            if n <= 240 {
                let end = buffer.iter_at_offset(chars * n / 240);
                buffer.select_range(&buffer.start_iter(), &end);
                return glib::ControlFlow::Continue;
            }
            if let Some(handlers) = handlers.borrow_mut().take() {
                for handler in handlers {
                    clock.disconnect(handler);
                }
            }
            let mut work: Vec<f64> = frames.borrow().iter().map(|f| f.0).collect();
            let mut gaps: Vec<f64> = frames.borrow().iter().skip(1).map(|f| f.1).collect();
            let at = |v: &mut Vec<f64>, q: f64| {
                v.sort_by(f64::total_cmp);
                v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                    .copied()
                    .unwrap_or(0.0)
            };
            println!(
                "select bench: {} frames; work p50 {:.1}ms p95 {:.1}ms max {:.1}ms; \
                 between frames p50 {:.1}ms p95 {:.1}ms max {:.1}ms; {} selected",
                work.len(),
                at(&mut work, 0.5),
                at(&mut work, 0.95),
                at(&mut work, 1.0),
                at(&mut gaps, 0.5),
                at(&mut gaps, 0.95),
                at(&mut gaps, 1.0),
                buffer
                    .selection_bounds()
                    .map_or(0, |(a, b)| b.offset() - a.offset()),
            );
            glib::ControlFlow::Break
        });
    }
}

/// The tag a line wears for a moment to be laid out again.
const NUDGE: &str = "taste-doc-nudge";

/// Write `text` into a buffer of its own, as `view` will show it.
fn render(view: &DocTextView, text: &str, base: &DocumentBase, readiness: &Readiness) -> Rendering {
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_enable_undo(false);
    let mut writer = Writer::new(&buffer, view.upcast_ref(), Some(base), Some(readiness));
    writer.write(text);
    writer.finish()
}

/// A widget at an anchor, as wide as the text column less `inset`: a text
/// view gives the widgets in its text what they ask for, and a picture
/// asks for its own width, which may be the screen's.
mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Column {
        pub(super) child: RefCell<Option<gtk::Widget>>,
        pub(super) view: glib::WeakRef<gtk::TextView>,
        pub(super) inset: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Column {
        const NAME: &'static str = "TasteDocColumn";
        type Type = super::Column;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Column {
        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Column {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let width = self.obj().column_width();
            let Some(child) = self.child.borrow().clone() else {
                return (0, 0, -1, -1);
            };
            // As its minimum: a text view lays a widget out at the size it
            // must have, not the size it would like.
            match orientation {
                gtk::Orientation::Horizontal => (width, width, -1, -1),
                _ => {
                    let at = if for_size >= 0 { for_size } else { width };
                    let (_, natural, _, _) = child.measure(gtk::Orientation::Vertical, at);
                    (natural, natural, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.borrow().as_ref() {
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    pub struct Column(ObjectSubclass<imp::Column>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Column {
    fn new(child: &impl IsA<gtk::Widget>, view: &gtk::TextView, inset: i32) -> Self {
        let this: Self = glib::Object::new();
        child.set_parent(&this);
        *this.imp().child.borrow_mut() = Some(child.clone().upcast());
        this.imp().view.set(Some(view));
        this.imp().inset.set(inset);
        this
    }

    /// Draw what this holds into `snapshot`, as this draws it: for its
    /// mirror in the strip's view. Nothing until it has been laid out.
    fn draw_into(&self, snapshot: &gtk::Snapshot) {
        if let Some(child) = self.imp().child.borrow().as_ref() {
            if child.width() > 0 && child.height() > 0 {
                self.snapshot_child(child, snapshot);
            }
        }
    }

    fn column_width(&self) -> i32 {
        let width = self
            .imp()
            .view
            .upgrade()
            .map(|view| view.width() - view.left_margin() - view.right_margin())
            .unwrap_or(0)
            - self.imp().inset.get();
        if width > 0 {
            width
        } else {
            480
        }
    }
}

/// Writes a document's Markdown into a buffer: blocks as lines (a
/// paragraph, a heading, a list item, a line of code), each line's tags
/// saying how it stands, and the inline styles as tags over its text.
struct Writer<'a> {
    buffer: gtk::TextBuffer,
    view: &'a gtk::TextView,
    base: Option<&'a DocumentBase>,
    readiness: Option<&'a Readiness>,
    tags: Tags,
    actions: HashMap<gtk::TextTag, Action>,
    children: Vec<(gtk::TextChildAnchor, Column)>,
    /// The inline styles in force: emphasis, a link, a code span.
    inline: Vec<gtk::TextTag>,
    /// The open line's tags, or `None` between lines.
    line: Option<Vec<gtk::TextTag>>,
    /// Where the last line ended began, as an offset.
    last_line: Option<i32>,
    heading: Option<HeadingLevel>,
    lists: Vec<Option<u64>>,
    /// How wide each open list's last marker was, for the lines of its
    /// item after the first.
    marker_widths: Vec<i32>,
    /// The item about to begin is a task, done or not.
    task: Option<bool>,
    /// An item has begun and its marker is not yet written.
    item_fresh: bool,
    quote: usize,
    code: Option<(String, String)>,
    table: Option<(Vec<pulldown_cmark::Alignment>, Vec<Vec<String>>)>,
    image: Option<(String, String)>,
    /// Nothing written yet: the first heading takes no space above it.
    blank: bool,
    /// What is drawn behind the text: code blocks' cards, quotes' bars.
    decor: Vec<Decor>,
    /// The lines each open quote began at.
    quote_starts: Vec<i32>,
}

/// The tags every rendering has, made once into its buffer's table.
struct Tags {
    paragraph: gtk::TextTag,
    item: gtk::TextTag,
    after_list: gtk::TextTag,
    tight: gtk::TextTag,
    headings: [gtk::TextTag; 4],
    first_heading: gtk::TextTag,
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    strike: gtk::TextTag,
    code_span: gtk::TextTag,
    link: gtk::TextTag,
    quote: gtk::TextTag,
    code_head: gtk::TextTag,
    code: gtk::TextTag,
    code_last: gtk::TextTag,
    spacer: gtk::TextTag,
    widget_line: gtk::TextTag,
    /// Margins by (left margin, indent): a list's levels and a quote's.
    margins: HashMap<(i32, i32), gtk::TextTag>,
}

impl Tags {
    fn new(table: &gtk::TextTagTable) -> Self {
        let add = |tag: gtk::TextTag| {
            table.add(&tag);
            tag
        };
        let heading = |scale: f64, weight: i32| {
            add(gtk::TextTag::builder()
                .scale(scale)
                .weight(weight)
                .pixels_above_lines(14)
                .pixels_below_lines(6)
                .build())
        };
        let code_margins = |builder: gtk::builders::TextTagBuilder| {
            builder
                .left_margin(MARGIN + CODE_INSET)
                .right_margin(MARGIN + CODE_INSET)
        };
        let this = Self {
            paragraph: add(gtk::TextTag::builder().pixels_below_lines(10).build()),
            item: add(gtk::TextTag::builder().pixels_below_lines(4).build()),
            headings: [
                heading(1.81, 800),
                heading(1.36, 800),
                heading(1.36, 700),
                heading(1.18, 700),
            ],
            first_heading: add(gtk::TextTag::builder().pixels_above_lines(0).build()),
            after_list: add(gtk::TextTag::builder().pixels_below_lines(10).build()),
            tight: add(gtk::TextTag::builder().pixels_below_lines(0).build()),
            bold: add(gtk::TextTag::builder().weight(700).build()),
            italic: add(gtk::TextTag::builder()
                .style(gtk::pango::Style::Italic)
                .build()),
            strike: add(gtk::TextTag::builder().strikethrough(true).build()),
            code_span: add(gtk::TextTag::builder().family("monospace").build()),
            link: add(gtk::TextTag::builder()
                .underline(gtk::pango::Underline::Single)
                .build()),
            quote: add(gtk::TextTag::new(None)),
            code_head: add(code_margins(gtk::TextTag::builder())
                .left_margin(MARGIN)
                .right_margin(MARGIN)
                .pixels_above_lines(0)
                .pixels_below_lines(0)
                .build()),
            code: add(code_margins(gtk::TextTag::builder())
                .family("monospace")
                .pixels_below_lines(0)
                .wrap_mode(gtk::WrapMode::WordChar)
                .build()),
            code_last: add(gtk::TextTag::builder().pixels_below_lines(10).build()),
            spacer: add(gtk::TextTag::builder().size_points(5.0).build()),
            widget_line: add(gtk::TextTag::builder().pixels_below_lines(10).build()),
            margins: HashMap::new(),
        };
        add(gtk::TextTag::builder().name(NUDGE).build());
        this
    }

    fn styles(&self) -> Styles {
        Styles {
            link: self.link.clone(),
            code_span: self.code_span.clone(),
            quote: self.quote.clone(),
        }
    }
}

impl<'a> Writer<'a> {
    fn new(
        buffer: &gtk::TextBuffer,
        view: &'a gtk::TextView,
        base: Option<&'a DocumentBase>,
        readiness: Option<&'a Readiness>,
    ) -> Self {
        Self {
            buffer: buffer.clone(),
            view,
            base,
            readiness,
            tags: Tags::new(&buffer.tag_table()),
            actions: HashMap::new(),
            children: Vec::new(),
            inline: Vec::new(),
            line: None,
            last_line: None,
            heading: None,
            lists: Vec::new(),
            marker_widths: Vec::new(),
            task: None,
            item_fresh: false,
            quote: 0,
            code: None,
            table: None,
            image: None,
            blank: true,
            decor: Vec::new(),
            quote_starts: Vec::new(),
        }
    }

    fn finish(mut self) -> Rendering {
        self.end_line();
        // The last line's end is no line of its own.
        let mut end = self.buffer.end_iter();
        let mut before = end;
        if before.backward_char() && before.char() == '\n' {
            self.buffer.delete(&mut before, &mut end);
        }
        let styles = self.tags.styles();
        styles.paint();
        Rendering {
            buffer: self.buffer,
            decor: self.decor,
            actions: self.actions,
            children: self.children,
            styles,
        }
    }

    /// A margin tag for a line standing `left` from the view's edge with a
    /// first line `indent` from that.
    fn margin(&mut self, left: i32, indent: i32) -> gtk::TextTag {
        let table = self.buffer.tag_table();
        self.tags
            .margins
            .entry((left, indent))
            .or_insert_with(|| {
                let tag = gtk::TextTag::builder()
                    .left_margin(left)
                    .indent(indent)
                    .build();
                table.add(&tag);
                tag
            })
            .clone()
    }

    /// How wide `marker` is set in the view's font.
    fn width_of(&self, marker: &str) -> i32 {
        self.view.create_pango_layout(Some(marker)).pixel_size().0
    }

    /// Open a line, if none is: its tags decided by where it stands, and
    /// an item's marker written at its start.
    fn begin_line(&mut self) {
        if self.line.is_some() {
            return;
        }
        let mut tags = Vec::new();
        let depth = self.lists.len();
        let quote_left = self.quote as i32 * LEVEL_INDENT;
        let mut marker = None;
        if depth > 0 {
            let left = MARGIN + quote_left + (depth as i32 - 1) * LEVEL_INDENT;
            if self.item_fresh {
                // A task's box is its item's marker, in the bullet's place.
                let text = match (self.task.take(), self.lists.last_mut()) {
                    (Some(done), _) => (if done { "☑ " } else { "☐ " }).to_string(),
                    (None, Some(Some(n))) => {
                        let text = format!("{n}. ");
                        *n += 1;
                        text
                    }
                    _ => "• ".to_string(),
                };
                let width = self.width_of(&text);
                if let Some(slot) = self.marker_widths.last_mut() {
                    *slot = width;
                }
                tags.push(self.margin(left + width, -width));
                marker = Some(text);
            } else {
                // Under the item's text, past its own marker.
                let width = self
                    .marker_widths
                    .last()
                    .copied()
                    .filter(|width| *width > 0)
                    .unwrap_or_else(|| self.width_of("• "));
                tags.push(self.margin(left + width, 0));
            }
            tags.push(self.tags.item.clone());
        } else if self.quote > 0 {
            tags.push(self.margin(MARGIN + quote_left, 0));
        }
        if self.quote > 0 {
            tags.push(self.tags.quote.clone());
        }
        match self.heading {
            Some(level) => {
                let index = match level {
                    HeadingLevel::H1 => 0,
                    HeadingLevel::H2 => 1,
                    HeadingLevel::H3 => 2,
                    _ => 3,
                };
                tags.push(self.tags.headings[index].clone());
                if self.blank {
                    tags.push(self.tags.first_heading.clone());
                }
            }
            None if depth == 0 => tags.push(self.tags.paragraph.clone()),
            None => {}
        }
        self.blank = false;
        self.item_fresh = false;
        self.line = Some(tags);
        if let Some(marker) = marker {
            self.insert_plain(&marker);
        }
    }

    /// Insert `text` with the line's tags only.
    fn insert_plain(&mut self, text: &str) {
        let tags = self.line.clone().unwrap_or_default();
        let refs: Vec<&gtk::TextTag> = tags.iter().collect();
        self.buffer
            .insert_with_tags(&mut self.buffer.end_iter(), text, &refs);
    }

    /// Insert `text` on the open line with the inline styles in force.
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin_line();
        let mut tags = self.line.clone().unwrap_or_default();
        tags.extend(self.inline.iter().cloned());
        let refs: Vec<&gtk::TextTag> = tags.iter().collect();
        self.buffer
            .insert_with_tags(&mut self.buffer.end_iter(), text, &refs);
    }

    /// End the open line, if one is.
    fn end_line(&mut self) {
        if self.line.is_none() {
            return;
        }
        let start = self.buffer.end_iter();
        let mut line_start = start;
        line_start.set_line_offset(0);
        self.last_line = Some(line_start.offset());
        self.insert_plain("\n");
        self.line = None;
    }

    /// The line that ended last, by its number.
    fn last_line_number(&self) -> Option<i32> {
        self.last_line
            .map(|offset| self.buffer.iter_at_offset(offset).line())
    }

    /// Apply `tag` to the line that ended last.
    fn tag_last_line(&self, tag: &gtk::TextTag) {
        if let Some(offset) = self.last_line {
            let start = self.buffer.iter_at_offset(offset);
            let mut end = start;
            end.forward_line();
            self.buffer.apply_tag(tag, &start, &end);
        }
    }

    /// A widget on a line of its own, the column wide less `inset`, with
    /// `tags` on its line.
    fn block_widget(
        &mut self,
        widget: &impl IsA<gtk::Widget>,
        inset: i32,
        tags: Vec<gtk::TextTag>,
    ) {
        self.end_line();
        self.blank = false;
        self.line = Some(tags);
        let mut end = self.buffer.end_iter();
        let anchor = self.buffer.create_child_anchor(&mut end);
        let start = self.buffer.iter_at_child_anchor(&anchor);
        if let Some(line) = self.line.clone() {
            let mut after = start;
            after.forward_char();
            for tag in &line {
                self.buffer.apply_tag(tag, &start, &after);
            }
        }
        self.children
            .push((anchor, Column::new(widget, self.view, inset)));
        self.end_line();
    }

    fn write(&mut self, text: &str) {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);
        for event in Parser::new_ext(text, options) {
            self.event(event);
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, alt)) = self.image.as_mut() {
                    alt.push_str(&text);
                } else if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&text);
                } else if let Some(cell) = self.cell() {
                    cell.push_str(&text);
                } else {
                    self.text(&text);
                }
            }
            Event::Code(code) => {
                if let Some(cell) = self.cell() {
                    cell.push_str(&code);
                    return;
                }
                // A URL written as code is the URL: a link to it.
                let bare = code.trim();
                let action = if (bare.starts_with("https://") || bare.starts_with("http://"))
                    && !bare.contains(char::is_whitespace)
                {
                    Action::Href(bare.to_string())
                } else {
                    Action::Copy(code.to_string())
                };
                let link = matches!(action, Action::Href(_));
                let identity = self.action_tag(action);
                self.inline.push(self.tags.code_span.clone());
                if link {
                    self.inline.push(self.tags.link.clone());
                }
                self.inline.push(identity);
                self.text(&code);
                self.inline.pop();
                if link {
                    self.inline.pop();
                }
                self.inline.pop();
            }
            Event::SoftBreak => {
                if self.cell().is_none() {
                    self.text(" ");
                }
            }
            Event::HardBreak => {
                if self.line.is_some() {
                    self.end_line();
                    self.tag_last_line(&self.tags.tight.clone());
                }
            }
            Event::Rule => {
                let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
                rule.set_valign(gtk::Align::Center);
                rule.set_margin_top(6);
                rule.set_margin_bottom(6);
                let tags = vec![self.tags.widget_line.clone()];
                self.block_widget(&rule, 0, tags);
            }
            Event::TaskListMarker(done) => {
                if self.item_fresh && self.line.is_none() {
                    self.task = Some(done);
                } else {
                    self.text(if done { "☑ " } else { "☐ " });
                }
            }
            _ => {}
        }
    }

    /// The table cell being written, when one is.
    fn cell(&mut self) -> Option<&mut String> {
        self.table
            .as_mut()
            .and_then(|(_, rows)| rows.last_mut())
            .and_then(|row| row.last_mut())
    }

    /// A tag of its own for one link or code span, which a click finds.
    fn action_tag(&mut self, action: Action) -> gtk::TextTag {
        let tag = gtk::TextTag::new(None);
        self.buffer.tag_table().add(&tag);
        self.actions.insert(tag.clone(), action);
        tag
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.end_line();
                self.heading = Some(level);
            }
            Tag::Paragraph => {}
            Tag::CodeBlock(kind) => {
                self.end_line();
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((info, String::new()));
            }
            Tag::List(start) => {
                // The text before a list — a paragraph, or the item a
                // nested list belongs to — ends here.
                self.end_line();
                self.lists.push(start);
                self.marker_widths.push(0);
            }
            Tag::Item => {
                self.end_line();
                self.item_fresh = true;
            }
            Tag::BlockQuote(_) => {
                self.end_line();
                self.quote += 1;
                self.quote_starts.push(self.buffer.end_iter().line());
            }
            Tag::Emphasis => self.inline.push(self.tags.italic.clone()),
            Tag::Strong => self.inline.push(self.tags.bold.clone()),
            Tag::Strikethrough => self.inline.push(self.tags.strike.clone()),
            Tag::Link { dest_url, .. } => {
                let identity = self.action_tag(Action::Href(dest_url.to_string()));
                self.inline.push(self.tags.link.clone());
                self.inline.push(identity);
            }
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            Tag::Table(alignments) => {
                self.end_line();
                self.table = Some((alignments, Vec::new()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some((_, rows)) = self.table.as_mut() {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|(_, rows)| rows.last_mut()) {
                    row.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.end_line();
                self.heading = None;
            }
            TagEnd::Paragraph => self.end_line(),
            TagEnd::CodeBlock => {
                if let Some((info, code)) = self.code.take() {
                    self.code_block(&info, code.trim_end_matches('\n'));
                }
            }
            TagEnd::List(_) => {
                self.end_line();
                self.lists.pop();
                self.marker_widths.pop();
                if self.lists.is_empty() {
                    self.tag_last_line(&self.tags.after_list.clone());
                }
            }
            TagEnd::Item => {
                if self.item_fresh {
                    // An empty item is still its marker.
                    self.begin_line();
                }
                self.end_line();
                self.item_fresh = false;
            }
            TagEnd::BlockQuote(_) => {
                self.end_line();
                if let (Some(first), Some(last)) =
                    (self.quote_starts.pop(), self.last_line_number())
                {
                    if last >= first {
                        self.decor.push(Decor::Quote {
                            first,
                            last,
                            depth: self.quote as i32,
                        });
                    }
                }
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.inline.pop();
            }
            TagEnd::Link => {
                self.inline.pop();
                self.inline.pop();
            }
            TagEnd::Image => {
                if let Some((source, alt)) = self.image.take() {
                    self.image(&source, &alt);
                }
            }
            TagEnd::Table => {
                if let Some((alignments, rows)) = self.table.take() {
                    let table = crate::markdown_view::table_card(&alignments, &rows);
                    let tags = vec![self.tags.widget_line.clone()];
                    self.block_widget(&table, 0, tags);
                }
            }
            _ => {}
        }
    }

    /// An image: a picture of the checkout's own on a line of its own, a
    /// remote one as a link the reader opens on purpose — never a fetch
    /// the document makes for them — and anything else its alt text.
    fn image(&mut self, source: &str, alt: &str) {
        let file = self.base.and_then(|base| base.resolve(source));
        match (file, self.base) {
            (Some(path), Some(base)) => {
                let picture = crate::markdown_view::picture(
                    base,
                    path,
                    alt,
                    self.readiness.map(Readiness::hold),
                );
                let tags = vec![self.tags.widget_line.clone()];
                self.block_widget(&picture, 0, tags);
            }
            _ if source.starts_with("http://") || source.starts_with("https://") => {
                let identity = self.action_tag(Action::Href(source.to_string()));
                self.inline.push(self.tags.italic.clone());
                self.inline.push(self.tags.link.clone());
                self.inline.push(identity);
                self.text(&format!(
                    "[image: {}]",
                    if alt.is_empty() { source } else { alt }
                ));
                self.inline.truncate(self.inline.len() - 3);
            }
            _ => {
                self.inline.push(self.tags.italic.clone());
                self.text(&format!("[image: {alt}]"));
                self.inline.pop();
            }
        }
    }

    /// A block of code: a head line with its copy button, its lines on the
    /// block's background — text of the document, so a selection runs
    /// through it — and a little space after. A Mermaid block whose fence
    /// has closed is the diagram instead.
    fn code_block(&mut self, info: &str, code: &str) {
        if crate::mermaid::is_mermaid(info) {
            let card = crate::markdown_view::code_card(code, Ok(code.to_string()), true);
            let diagram = crate::mermaid::diagram(code, card, self.readiness.map(Readiness::hold));
            let tags = vec![self.tags.widget_line.clone()];
            self.block_widget(&diagram, 0, tags);
            return;
        }
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let language = gtk::Label::builder()
            .label(info.split_whitespace().next().unwrap_or(""))
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label", "caption"])
            .margin_start(CODE_INSET)
            .build();
        head.append(&language);
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy code block")
            .css_classes(["flat", "circular"])
            .margin_top(2)
            .margin_end(4)
            .build();
        crate::copy::button(&copy, Ok(code.to_string()));
        head.append(&copy);
        let tags = vec![self.tags.code_head.clone()];
        let first = self.buffer.end_iter().line();
        self.block_widget(&head, 0, tags);
        self.line = Some(vec![self.tags.code.clone()]);
        self.insert_plain(code);
        self.end_line();
        self.tag_last_line(&self.tags.code_last.clone());
        if let Some(last) = self.last_line_number() {
            self.decor.push(Decor::Code { first, last });
        }
        self.line = Some(vec![self.tags.spacer.clone()]);
        self.end_line();
    }
}

/// What the view draws behind a run of lines.
#[derive(Clone, Debug, PartialEq)]
enum Decor {
    /// A code block's card, from its head line to its last line.
    Code { first: i32, last: i32 },
    /// A quote's bar beside its lines, `depth` quotes deep.
    Quote { first: i32, last: i32, depth: i32 },
}

mod view_imp {
    use super::*;

    #[derive(Default)]
    pub struct DocTextView {
        pub(super) decor: RefCell<Vec<Decor>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DocTextView {
        const NAME: &'static str = "TasteDocTextView";
        type Type = super::DocTextView;
        type ParentType = gtk::TextView;
    }

    impl ObjectImpl for DocTextView {}
    impl WidgetImpl for DocTextView {}

    impl TextViewImpl for DocTextView {
        fn snapshot_layer(&self, layer: gtk::TextViewLayer, snapshot: gtk::Snapshot) {
            self.parent_snapshot_layer(layer, snapshot.clone());
            if layer == gtk::TextViewLayer::BelowText {
                self.obj().draw_decor(&snapshot, &self.decor.borrow());
            }
        }
    }
}

glib::wrapper! {
    /// The preview's text view, read-only, that draws
    /// code blocks' cards and quotes' bars behind its text.
    pub struct DocTextView(ObjectSubclass<view_imp::DocTextView>)
        @extends gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl DocTextView {
    /// A view for a document: read-only, its cursor hidden, wrapping.
    fn new() -> Self {
        let view: Self = glib::Object::new();
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(MARGIN);
        view.set_right_margin(MARGIN);
        view.set_top_margin(MARGIN);
        view.set_bottom_margin(MARGIN);
        view.set_pixels_inside_wrap(2);
        view.add_css_class("document-preview");
        view
    }

    /// Draw `decor` where it falls in view, in buffer coordinates.
    fn draw_decor(&self, snapshot: &gtk::Snapshot, decor: &[Decor]) {
        let buffer = self.buffer();
        let visible = self.visible_rect();
        let (view_top, view_bottom) = (visible.y(), visible.y() + visible.height());
        let dark = adw::StyleManager::default().is_dark();
        let ink = |alpha: f32| {
            if dark {
                gtk::gdk::RGBA::new(1.0, 1.0, 1.0, alpha)
            } else {
                gtk::gdk::RGBA::new(0.0, 0.0, 0.0, alpha)
            }
        };
        let span = |first: i32, last: i32| -> Option<(f32, f32)> {
            let top = buffer.iter_at_line(first)?;
            let bottom = buffer.iter_at_line(last)?;
            let (y, _) = self.line_yrange(&top);
            let (last_y, last_height) = self.line_yrange(&bottom);
            let end = last_y + last_height;
            (end >= view_top && y <= view_bottom).then_some((y as f32, (end - y) as f32))
        };
        let right = (self.width() - MARGIN) as f32;
        for item in decor {
            match *item {
                Decor::Code { first, last } => {
                    let Some((y, height)) = span(first, last) else {
                        continue;
                    };
                    let card =
                        gtk::graphene::Rect::new(MARGIN as f32, y, right - MARGIN as f32, height);
                    snapshot.push_rounded_clip(&gtk::gsk::RoundedRect::from_rect(card, 8.0));
                    snapshot.append_color(&ink(if dark { 0.06 } else { 0.05 }), &card);
                    snapshot.pop();
                }
                Decor::Quote { first, last, depth } => {
                    let Some((y, height)) = span(first, last) else {
                        continue;
                    };
                    // Beside the text, not the space after its last line.
                    let x = (MARGIN + (depth - 1) * LEVEL_INDENT) as f32;
                    let bar = gtk::graphene::Rect::new(x, y, 3.0, (height - 10.0).max(2.0));
                    snapshot.push_rounded_clip(&gtk::gsk::RoundedRect::from_rect(bar, 1.5));
                    snapshot.append_color(&ink(0.25), &bar);
                    snapshot.pop();
                }
            }
        }
    }
}

/// A widget of the preview, again, in the strip's view: as large as the
/// one it stands for, and drawn as it is drawn — its column draws its child
/// into the mirror's frame — so a picture is not decoded twice nor a
/// diagram drawn twice. A widget lives in one view only, and each view
/// needs one at an anchor. Not a `GtkWidgetPaintable`: that shows what a
/// frame last drew of a widget, and a text view draws only the widgets on
/// screen, so a picture below the fold was blank in the strip.
mod mirror_imp {
    use super::*;

    #[derive(Default)]
    pub struct Mirror {
        pub(super) original: RefCell<Option<Column>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Mirror {
        const NAME: &'static str = "TasteDocMirror";
        type Type = super::Mirror;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Mirror {}

    impl WidgetImpl for Mirror {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match self.original.borrow().as_ref() {
                Some(original) => {
                    let (min, natural, _, _) = original.measure(orientation, for_size);
                    (min, natural, -1, -1)
                }
                None => (0, 0, -1, -1),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(original) = self.original.borrow().as_ref() {
                original.draw_into(snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct Mirror(ObjectSubclass<mirror_imp::Mirror>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Mirror {
    fn new(original: &Column) -> Self {
        let this: Self = glib::Object::new();
        *this.imp().original.borrow_mut() = Some(original.clone());
        this
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(text: &str) -> (String, Rendering) {
        let view = DocTextView::new();
        let buffer = gtk::TextBuffer::new(None);
        let mut writer = Writer::new(&buffer, view.upcast_ref(), None, None);
        writer.write(text);
        let rendering = writer.finish();
        let buffer = &rendering.buffer;
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        (text, rendering)
    }

    /// A document is one text: its blocks are its lines, a code block's
    /// lines among them, so a selection from the heading to the end
    /// copies the code with the prose.
    #[test]
    fn a_document_is_one_text() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (text, rendering) = written(
                "# Plan\n\nFirst we **read** the `config`, then [the tree](src/tree.rs).\n\n\
                 - one\n- two\n  - nested\n\n1. first\n2. second\n\n> quoted\n\n\
                 ```rust\nfn main() {}\n```\n\nThe end.",
            );
            assert_eq!(
                text,
                "Plan\nFirst we read the config, then the tree.\n• one\n• two\n• nested\n\
                 1. first\n2. second\nquoted\n\nfn main() {}\n\nThe end."
            );
            let actions: Vec<&Action> = rendering.actions.values().collect();
            assert!(actions.contains(&&Action::Copy("config".into())));
            assert!(actions.contains(&&Action::Href("src/tree.rs".into())));
            // The code block's head is a widget: one, at its own line.
            assert_eq!(rendering.children.len(), 1);
        });
    }

    /// A list item's lines after its first stand under its text, not under
    /// its marker: the marker hangs.
    #[test]
    fn an_item_hangs_its_marker() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (_, rendering) = written("- one\n\n  more of one\n- two\n");
            let buffer = &rendering.buffer;
            let margin = |line: i32| {
                let iter = buffer.iter_at_line(line).unwrap();
                iter.tags()
                    .iter()
                    .find(|tag| tag.is_left_margin_set())
                    .map(|tag| (tag.left_margin(), tag.indent()))
            };
            let (first, hang) = margin(0).expect("the first line has a margin");
            assert!(hang < 0, "the marker hangs: {hang}");
            assert_eq!(
                margin(1),
                Some((first, 0)),
                "a second paragraph stands under the text"
            );
        });
    }

    /// An ordered item's second paragraph stands under its text, past its
    /// own marker rather than a bullet's; a task's box takes the bullet's
    /// place.
    #[test]
    fn an_item_continues_under_its_own_marker() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (text, rendering) =
                written("1. First\n2. Second:\n\n   more of it\n\n- [ ] to do\n- [x] done\n");
            assert_eq!(text, "1. First\n2. Second:\nmore of it\n☐ to do\n☑ done");
            let buffer = &rendering.buffer;
            let margin = |line: i32| {
                let iter = buffer.iter_at_line(line).unwrap();
                iter.tags()
                    .iter()
                    .find(|tag| tag.is_left_margin_set())
                    .map(|tag| (tag.left_margin(), tag.indent()))
            };
            let (second, hang) = margin(1).unwrap();
            assert_eq!(margin(2), Some((second, 0)), "{:?}", margin(2));
            assert!(hang < 0);
        });
    }
}
