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
//!
//! A line of the source asked for — a search result's, a link's `#L12` —
//! is put in view from where each block, and each line of a paragraph or
//! a code block, landed as the document was written (`crate::doc_source`,
//! [`DocView::reveal_line`]); a line of a table is the table.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::doc_source::{LineIndex, SourceMap};
use crate::markdown_view::{list_shapes, DocumentBase, ListShape, Readiness};

/// The view's margin, all round.
const MARGIN: i32 = 16;

/// How far a code block's text stands in from the column, each side.
const CODE_INSET: i32 = 12;

/// How far each list or quote level stands in.
const LEVEL_INDENT: i32 = 22;

/// How far down the view a line asked for is put, as a fraction of its
/// height: near the top, with a little of what comes before it above —
/// as the source editor puts a line it jumps to.
const REVEAL_ALIGN: f32 = 0.1;

/// The priority of the work a text view does laying its lines out in the
/// gaps between frames, `GTK_TEXT_VIEW_PRIORITY_VALIDATE`, which GTK does
/// not export: drawing's, and five below it.
const TEXT_VIEW_VALIDATE: i32 = gtk::gdk::PRIORITY_REDRAW as i32 + 5;

/// The longest a kept place is put back as the text view lays the
/// document out: past it, the layout still going, the view is left where
/// the text view has it. A long document is laid out in a second or two.
const SETTLE_FOR: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Where the source's lines landed (`crate::doc_source`).
    source: SourceMap,
    /// The source it was written from: what the next one's lines are
    /// followed back to.
    text: String,
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
    source: SourceMap,
    text: String,
}

/// How a held place is put in view.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hold {
    /// A line asked for: a tenth of the way down the view.
    Revealed,
    /// The reader's own place, kept through a refresh: the character
    /// `chars` past the line's place — the first of the display line the
    /// view's top edge is in — with its display line's top `above` pixels
    /// above that edge. `line_above` is how far the line's own place is
    /// above the edge, for when the line was changed and its characters
    /// are not what they were.
    Kept {
        chars: i32,
        above: i32,
        line_above: i32,
    },
}

/// A place in the document, as a line of its source.
#[derive(Clone, Copy, Debug)]
struct Place {
    /// 0-based.
    line: u32,
    hold: Hold,
}

/// A place held in view, at a mark in the shown rendering's buffer.
struct Held {
    mark: gtk::TextMark,
    place: Place,
}

/// A source line asked for before the rendering that has it is shown.
struct Wanted {
    /// 1-based, as a search result or a link's `#L12` gives it.
    line: u32,
    /// The text it is a line of is still to be presented: the rendering
    /// shown now, or one on its way, is of an older text.
    after_present: bool,
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
    /// Which rendering is on screen.
    shown_generation: Cell<u64>,
    on_link: RefCell<Option<Rc<dyn Fn(&str)>>>,
    base: RefCell<Option<DocumentBase>>,
    /// A line asked for that waits on its rendering.
    wanted: RefCell<Option<Wanted>>,
    /// The place a line asked for landed, or the reader's place kept
    /// through a refresh, held there while what is above it is laid out
    /// again — a picture or a diagram come in, the column's width settled
    /// — until the reader scrolls, clicks, or types.
    target: RefCell<Option<Held>>,
    /// A kept place is put again whenever the view moves or the
    /// document's height changes, while the text view lays out the lines
    /// above it (`converge`).
    armed: Cell<bool>,
    /// When `armed` was, for [`SETTLE_FOR`].
    armed_at: Cell<Option<std::time::Instant>>,
    /// A kept place is being put: the view moving is this.
    settling: Cell<bool>,
    /// A [`DocView::queue_settle`] is waiting to run.
    settle_queued: Cell<bool>,
    /// The waiting settle is a [`DocView::settle_soon`]'s, which ends
    /// `fresh`.
    unfresh: Cell<bool>,
    /// A kept place's mark is in a buffer just shown, whose first screen
    /// the text view is still to lay out: where anything stands in it is
    /// not yet known, so nothing is put until it is (`settle_soon`).
    fresh: Cell<bool>,
    /// TASTE_PROBE_KEEP only: a wait in place of the editor's.
    wait_for_probe: Cell<Option<std::time::Duration>>,
}

impl DocView {
    /// The face, empty, with a strip `map_width` wide beside it.
    pub fn new(map_width: i32) -> Rc<Self> {
        let view = DocTextView::new();
        view.set_hexpand(true);
        view.set_vexpand(true);
        // EXTERNAL, not NEVER: never sideways either way, but NEVER makes
        // the scroller's minimum width the view's, and the view's is its
        // widest widget's — a picture, a table, a code block's head, each
        // the column wide, so as wide as the view already is. That held
        // the editor at whatever width it last had: a window could not
        // shrink it, nor the chat's divider move into it (David,
        // 2026-11-09: "I can't even resize the chat area"). EXTERNAL asks
        // nothing of the view's width and gives it the viewport's, so the
        // columns follow the pane rather than holding it.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::External)
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
            shown_generation: Cell::new(0),
            on_link: RefCell::new(None),
            base: RefCell::new(None),
            wanted: RefCell::new(None),
            target: RefCell::new(None),
            armed: Cell::new(false),
            armed_at: Cell::new(None),
            settling: Cell::new(false),
            settle_queued: Cell::new(false),
            unfresh: Cell::new(false),
            fresh: Cell::new(false),
            wait_for_probe: Cell::new(None),
        });
        this.install_links();
        this.install_letting_go();
        // While a kept place is being put (`converge`), wherever the text
        // view moves the view to — its first line on screen held in place
        // as the lines above it are laid out, or not quite — and whenever
        // what is laid out changes the document's height, the place is put
        // back its pixels above the top edge before the next frame, once
        // the text view's step of layout is over (`queue_settle`).
        {
            let weak = Rc::downgrade(&this);
            this.scroller.vadjustment().connect_value_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    if this.armed.get() && !this.settling.get() {
                        this.queue_settle();
                    }
                }
            });
            let weak = Rc::downgrade(&this);
            this.scroller.vadjustment().connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    if this.armed.get() {
                        this.settle_soon();
                    }
                }
            });
        }
        // A new width lays every line out again: the reader's place is
        // read before at the width they read it at, and put back after.
        {
            let weak = Rc::downgrade(&this);
            *this.view.imp().on_width.borrow_mut() = Some(Box::new(move |laid_out| {
                if let Some(this) = weak.upgrade() {
                    if laid_out {
                        this.follow_target();
                    } else {
                        this.hold_through_width();
                    }
                }
            }));
        }
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
        let wait = self.wait_for_probe.get().unwrap_or(wait);
        let readiness = Readiness::default();
        let rendering = render(&self.view, text, &base, &readiness);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        // A line asked of the text this is waits on this rendering now.
        if let Some(wanted) = self.wanted.borrow_mut().as_mut() {
            wanted.after_present = false;
        }
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
                    this.shown_generation.set(generation);
                    this.land_wanted();
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
            let weak = Rc::downgrade(self);
            readiness.when_ready(move || {
                on_ready();
                // Shown before it was whole, at the wait's end: what came
                // in since may have moved a line asked for.
                if let Some(this) = weak.upgrade() {
                    if this.shown_generation.get() == generation {
                        this.follow_target();
                    }
                }
            });
            glib::timeout_add_local_once(wait, move || swap());
        }
    }

    /// Put 1-based source `line` of the document in view: the block it is
    /// in — its own line of a paragraph or a code block, the table or the
    /// picture it is part of — near the top, with a little above it. With
    /// `after_present`, the line is of a text still to be presented (the
    /// editor's restyle is on its way), and it lands when that rendering
    /// is shown; otherwise it lands now, or when the rendering already on
    /// its way is shown.
    pub fn reveal_line(self: &Rc<Self>, line: u32, after_present: bool) {
        let waiting = self.shown_generation.get() != self.generation.get();
        if after_present || waiting || self.shown.borrow().is_none() {
            *self.wanted.borrow_mut() = Some(Wanted {
                line,
                after_present,
            });
            return;
        }
        self.wanted.borrow_mut().take();
        self.land(line);
    }

    /// The line asked for, landed, if its rendering is the one shown.
    fn land_wanted(self: &Rc<Self>) {
        let wanted = self.wanted.borrow_mut().take_if(|w| !w.after_present);
        if let Some(wanted) = wanted {
            self.land(wanted.line);
        }
    }

    fn land(self: &Rc<Self>, line: u32) {
        self.land_at(Place {
            line: line.saturating_sub(1),
            hold: Hold::Revealed,
        });
    }

    /// Put `place` in view, in the rendering just shown or shown already,
    /// and hold it there.
    fn land_at(self: &Rc<Self>, place: Place) {
        let Some(mark) = self.mark_for(place) else {
            return;
        };
        self.let_go();
        *self.target.borrow_mut() = Some(Held { mark, place });
        match place.hold {
            Hold::Revealed => {
                self.follow_target();
                // Once the lines around it are laid out, as they are
                // drawn: a widget measured at its column's width for the
                // first time moves what is under it.
                let frames = Cell::new(0);
                let weak = Rc::downgrade(self);
                self.view.add_tick_callback(move |_, _| {
                    frames.set(frames.get() + 1);
                    if frames.get() < 3 {
                        return glib::ControlFlow::Continue;
                    }
                    if let Some(this) = weak.upgrade() {
                        this.follow_target();
                    }
                    glib::ControlFlow::Break
                });
            }
            Hold::Kept { .. } => {
                // A fresh buffer is laid out from its first line, a
                // screen of it before the first frame; where the rest
                // stands is estimated until it is laid out. The place is
                // put once that screen is (`settle_soon`), at its place
                // as estimated, which puts it and the lines after it on
                // screen, laid out; and again as the lines above it are
                // laid out too (`converge`).
                self.fresh.set(true);
                self.settle_soon();
                self.converge();
            }
        }
    }

    /// A mark at `place` in the shown rendering's buffer.
    fn mark_for(&self, place: Place) -> Option<gtk::TextMark> {
        let offset = {
            let shown = self.shown.borrow();
            shown.as_ref()?.source.place(place.line)
        };
        let buffer = self.view.buffer();
        let mut iter = match place.hold {
            Hold::Kept { chars, .. } => buffer.iter_at_offset(offset + chars),
            Hold::Revealed => buffer.iter_at_offset(offset),
        };
        // A widget — a table, a picture, a diagram — is put with the end
        // of the line before it where a line of text is put: a text view
        // places a place's whole height in the view, and one taller than
        // a tenth of it would otherwise come out at its top edge, flush,
        // or wherever its height left it.
        if place.hold == Hold::Revealed && iter.child_anchor().is_some() {
            iter.backward_char();
        }
        Some(buffer.create_mark(None, &iter, true))
    }

    /// Put the held place where it is held again, if one is: what is laid
    /// out again above it — a widget measured, a picture or a diagram come
    /// in, the column's width settled — is what this answers.
    fn follow_target(self: &Rc<Self>) {
        let (mark, hold) = match self.target.borrow().as_ref() {
            Some(held) if !held.mark.is_deleted() => (held.mark.clone(), held.place.hold),
            _ => return,
        };
        match hold {
            Hold::Revealed => {
                self.view
                    .scroll_to_mark(&mark, 0.0, true, 0.0, f64::from(REVEAL_ALIGN))
            }
            Hold::Kept { .. } => {
                self.settle();
                self.settle_soon();
                self.converge();
            }
        }
    }

    /// Move the view so a kept place stands its pixels above the top edge,
    /// where the lines around it are laid out now. Whether it moved.
    fn settle(&self) -> bool {
        if self.settling.get() || self.fresh.get() {
            return false;
        }
        let (mark, above) = match self.target.borrow().as_ref() {
            Some(Held {
                mark,
                place:
                    Place {
                        hold: Hold::Kept { above, .. },
                        ..
                    },
            }) if !mark.is_deleted() => (mark.clone(), *above),
            _ => return false,
        };
        let iter = self.view.buffer().iter_at_mark(&mark);
        let off = self.view.iter_location(&iter).y() + above - self.view.visible_rect().y();
        if off == 0 {
            return false;
        }
        self.settling.set(true);
        let adjustment = self.scroller.vadjustment();
        let was = adjustment.value();
        adjustment.set_value(was + f64::from(off));
        self.settling.set(false);
        adjustment.value() != was
    }

    /// [`Self::settle`] once the text view has laid out the lines on
    /// screen, and before the next frame is drawn: after its own idle that
    /// does (`GTK_PRIORITY_RESIZE - 2`), before the frame clock's
    /// (`GDK_PRIORITY_REDRAW`).
    fn settle_soon(self: &Rc<Self>) {
        self.unfresh.set(true);
        self.queue_settle();
    }

    /// [`Self::settle`] at the same idle, and never inside the signal that
    /// asks for it: the view moving and the document's height changing are
    /// emitted from inside the text view's own layout, a step at a time,
    /// and a settle there moved the view in the middle of that layout. A
    /// resize once hung the app in it for good, every sample of the main
    /// thread a settle under the text view's validation (2026-11-16; David:
    /// "Resizing Taste seems to have crashed it"). Out here, the layout
    /// runs a step, the main loop turns, and the place is put back before
    /// the frame is drawn, as before. One idle however many ask.
    fn queue_settle(self: &Rc<Self>) {
        if self.settle_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let priority = glib::Priority::from(gtk::PRIORITY_RESIZE as i32 - 1);
        glib::idle_add_local_full(priority, move || {
            if let Some(this) = weak.upgrade() {
                this.settle_queued.set(false);
                if this.unfresh.replace(false) {
                    this.fresh.set(false);
                }
                // However long the layout takes, a place is held for
                // `SETTLE_FOR` at most, and then the view is the text
                // view's again.
                if this.armed.get()
                    && this
                        .armed_at
                        .get()
                        .is_some_and(|at| at.elapsed() > SETTLE_FOR)
                {
                    this.armed.set(false);
                }
                this.settle();
            }
            glib::ControlFlow::Break
        });
    }

    /// Settle a kept place whenever the view moves or the document's
    /// height changes, until the text view has laid out the whole of the
    /// document: it lays out the rest of a fresh one in the gaps between
    /// frames, and holds the line at the view's top in place as it does,
    /// but not to the pixel — a line laid out above the place, taller or
    /// shorter than it was estimated, moves it.
    fn converge(self: &Rc<Self>) {
        if self.armed.replace(true) {
            return;
        }
        self.armed_at.set(Some(std::time::Instant::now()));
        // Done when the text view's layout is: an idle just below its own
        // (`GTK_TEXT_VIEW_PRIORITY_VALIDATE`) runs only once that has
        // nothing left to do.
        let weak = Rc::downgrade(self);
        let priority = glib::Priority::from(TEXT_VIEW_VALIDATE + 1);
        glib::idle_add_local_full(priority, move || {
            if let Some(this) = weak.upgrade() {
                if this.armed.replace(false) {
                    this.settle();
                }
            }
            glib::ControlFlow::Break
        });
    }

    /// Stop holding a place.
    fn let_go(&self) {
        self.armed.set(false);
        self.fresh.set(false);
        if let Some(held) = self.target.borrow_mut().take() {
            if let Some(buffer) = held.mark.buffer() {
                buffer.delete_mark(&held.mark);
            }
        }
    }

    /// Where the reader is, as a place to keep through a refresh: a line
    /// asked for and still held, or the source line at the view's top, the
    /// character the top display line starts with, and how far into that
    /// line the top edge is. None at the very top, which a refresh leaves
    /// at the top.
    fn reading(&self) -> Option<Place> {
        if let Some(held) = self.target.borrow().as_ref() {
            // A kept place not yet put is where the reader is.
            if held.place.hold == Hold::Revealed || self.fresh.get() {
                return Some(held.place);
            }
        }
        if self.scroller.vadjustment().value() <= 0.0 {
            return None;
        }
        let shown = self.shown.borrow();
        let shown = shown.as_ref()?;
        let (top, above) = self.top_display_line();
        let (line, at) = shown.source.line_at(top.offset()).unwrap_or((0, 0));
        let place = self.view.buffer().iter_at_offset(at);
        let line_above = self.view.visible_rect().y() - self.view.iter_location(&place).y();
        Some(Place {
            line,
            hold: Hold::Kept {
                chars: top.offset() - at,
                above,
                line_above,
            },
        })
    }

    /// Before the column is laid out at a new width: the reader's place
    /// held, read at the width it was read at, unless a line asked for is
    /// held already.
    fn hold_through_width(&self) {
        let Some(place) = self.reading() else { return };
        if place.hold == Hold::Revealed {
            return;
        }
        let Some(mark) = self.mark_for(place) else {
            return;
        };
        self.let_go();
        *self.target.borrow_mut() = Some(Held { mark, place });
    }

    /// The reader's own scroll, click, or key — in the text or on the
    /// strip — lets a held place go: where they move to is theirs.
    fn install_letting_go(self: &Rc<Self>) {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.let_go();
            }
            glib::Propagation::Proceed
        });
        self.body.add_controller(scroll);
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.let_go();
            }
        });
        self.body.add_controller(click);
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        key.connect_key_pressed(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.let_go();
            }
            glib::Propagation::Proceed
        });
        self.body.add_controller(key);
    }

    /// TASTE_PROBE_LINK only: the display line at the top of the view, the
    /// one where a line asked for is put, and where 1-based source `line`'s
    /// place is in the view, as text and pixels.
    #[doc(hidden)]
    pub fn view_for_probe(&self, line: Option<u32>) -> String {
        let rect = self.view.visible_rect();
        let say = |iter: gtk::TextIter| {
            let mut start = iter;
            if !self.view.starts_display_line(&start) {
                self.view.backward_display_line_start(&mut start);
            }
            let mut end = start;
            self.view.forward_display_line_end(&mut end);
            let text = start.slice(&end).replace('\u{fffc}', "[widget]");
            let text: String = text.chars().take(60).collect();
            format!("«{text}»")
        };
        let at_y = |y: i32| {
            self.view
                .iter_at_location(MARGIN, y)
                .unwrap_or_else(|| self.view.line_at_y(y).0)
        };
        let aim = rect.y() + (rect.height() as f32 * REVEAL_ALIGN) as i32 + 1;
        let place = line.and_then(|line| {
            let shown = self.shown.borrow();
            let offset = shown.as_ref()?.source.place(line.saturating_sub(1));
            let iter = self.view.buffer().iter_at_offset(offset);
            let y = self.view.iter_location(&iter).y() - rect.y();
            Some(format!(
                "; its place {} at {y}px of {}px ({:.0}%)",
                say(iter),
                rect.height(),
                f64::from(y) * 100.0 / f64::from(rect.height().max(1))
            ))
        });
        format!(
            "scrolled {}px of {:.0}px; top {}; at {:.0}% {}{}",
            rect.y(),
            self.view
                .vadjustment()
                .map_or(0.0, |adjustment| adjustment.upper()),
            say(at_y(rect.y())),
            REVEAL_ALIGN * 100.0,
            say(at_y(aim)),
            place.unwrap_or_default()
        )
    }

    /// The display line the view's top edge is in: its first character,
    /// and how far the top edge is below that line's top.
    fn top_display_line(&self) -> (gtk::TextIter, i32) {
        let top = self.view.visible_rect().y();
        let (mut iter, _) = self.view.line_at_y(top);
        let line = iter.line();
        let mut next = iter;
        while self.view.forward_display_line(&mut next)
            && next.line() == line
            && self.view.iter_location(&next).y() <= top
        {
            iter = next;
        }
        let y = self.view.iter_location(&iter).y();
        (iter, top - y)
    }

    /// TASTE_PROBE_KEEP only: the display line at the view's top, as text,
    /// how far into it the top edge is, and how far the view is scrolled.
    #[doc(hidden)]
    pub fn top_for_probe(&self) -> String {
        let (start, into) = self.top_display_line();
        let mut end = start;
        self.view.forward_display_line_end(&mut end);
        let text = start.slice(&end).replace('\u{fffc}', "[widget]");
        let text: String = text.chars().take(48).collect();
        let adjustment = self.scroller.vadjustment();
        format!(
            "«{text}» +{into}px (scrolled {:.0} of {:.0})",
            adjustment.value(),
            adjustment.upper() - adjustment.page_size()
        )
    }

    /// TASTE_PROBE_LISTS only: each list item's first line, as where its
    /// marker begins, where the marker's last glyph before its space (an
    /// ordered item's period) ends, and where its text begins, and where
    /// each of its later display lines begins — every x in the view's own
    /// pixels — so a list whose periods and text stand in one column says
    /// so in numbers.
    #[doc(hidden)]
    pub fn lists_for_probe(&self) -> Vec<String> {
        let buffer = self.view.buffer();
        let Some(marker) = buffer.tag_table().lookup(MARKER) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let mut iter = buffer.start_iter();
        loop {
            if iter.starts_tag(Some(&marker)) {
                let start = iter;
                let mut text = start;
                text.forward_to_tag_toggle(Some(&marker));
                let mut period = text;
                period.backward_chars(2);
                let place = self.view.iter_location(&period);
                let mut wraps = Vec::new();
                let mut next = start;
                while self.view.forward_display_line(&mut next)
                    && next.line() == start.line()
                    && !next.is_end()
                {
                    wraps.push(self.view.iter_location(&next).x());
                }
                let margin = start
                    .tags()
                    .iter()
                    .filter(|tag| tag.is_left_margin_set())
                    .map(|tag| (tag.left_margin(), tag.indent()))
                    .collect::<Vec<_>>();
                lines.push(format!(
                    "list item «{}» margin {margin:?} marker x {} · its end {} · text x {} · wraps at {:?} · «{}»",
                    start.slice(&text).trim_end(),
                    self.view.iter_location(&start).x(),
                    place.x() + place.width(),
                    self.view.iter_location(&text).x(),
                    wraps,
                    {
                        let mut end = text;
                        end.forward_chars(24);
                        text.slice(&end)
                    }
                ));
            }
            if !iter.forward_to_tag_toggle(Some(&marker)) {
                break;
            }
            // Past the marker's end, on to the next one's start.
            if iter.ends_tag(Some(&marker)) && !iter.forward_to_tag_toggle(Some(&marker)) {
                break;
            }
        }
        lines
    }

    /// TASTE_PROBE_KEEP only: the 0-based source line at the view's top.
    #[doc(hidden)]
    pub fn top_line_for_probe(&self) -> Option<u32> {
        let (top, _) = self.top_display_line();
        let shown = self.shown.borrow();
        Some(shown.as_ref()?.source.line_at(top.offset())?.0)
    }

    /// TASTE_PROBE_KEEP only: scroll `at` of the way down, as a reader
    /// would, and on into the display line found there, so the top edge
    /// is partway through a line rather than on its boundary.
    #[doc(hidden)]
    pub fn scroll_for_probe(&self, at: f64) {
        self.let_go();
        let adjustment = self.scroller.vadjustment();
        let end = (adjustment.upper() - adjustment.page_size()).max(0.0);
        adjustment.set_value(end * at.clamp(0.0, 1.0));
        if at > 0.0 && at < 1.0 {
            let (_, into) = self.top_display_line();
            if into < 4 {
                adjustment.set_value(adjustment.value() + f64::from(9 - into));
            }
        }
    }

    /// TASTE_PROBE_KEEP only: how long a rendering waits for its pictures
    /// and diagrams before it is shown, in place of the editor's — none,
    /// for a picture that comes in after its rendering is up.
    #[doc(hidden)]
    pub fn set_wait_for_probe(&self, wait: Option<std::time::Duration>) {
        self.wait_for_probe.set(wait);
    }

    /// Put `rendering` in place of what is shown, keeping the reader's
    /// place when `keep_place`: the source line at the view's top, found
    /// in the new text through the edit (`doc_source::follow_line`), its
    /// character at the top the same pixels above the top edge as it was
    /// — its own place, where the line was changed — or a line asked for
    /// and still held, at the place a line asked for is put. An edit above
    /// the view, below it, or in the very paragraph at its top moves
    /// nothing on screen, and a view at its top stays there.
    fn swap(self: &Rc<Self>, rendering: Rendering, keep_place: bool) {
        let place = keep_place.then(|| self.reading()).flatten();
        let mut old_text = String::new();
        if let Some(shown) = self.shown.borrow_mut().take() {
            for (_, child) in shown.children {
                self.view.remove(&child);
            }
            for mirror in shown.mirrors {
                self.ghost.remove(&mirror);
            }
            old_text = shown.text;
        }
        // A place held in the last rendering's buffer is that buffer's.
        self.let_go();
        let Rendering {
            buffer,
            decor,
            actions,
            children,
            styles,
            source,
            text,
        } = rendering;
        let place = place.map(|place| {
            let moved = crate::doc_source::follow_line(&old_text, &text, place.line);
            use crate::doc_source::Fate;
            let hold = match (place.hold, moved.fate) {
                // A line changed: its own place, as far above the edge as
                // it was.
                (Hold::Kept { line_above, .. }, Fate::Changed) => Hold::Kept {
                    chars: 0,
                    above: line_above,
                    line_above,
                },
                // A line deleted outright: what follows it, from its top.
                (Hold::Kept { .. }, Fate::Gone) => Hold::Kept {
                    chars: 0,
                    above: 0,
                    line_above: 0,
                },
                (hold, _) => hold,
            };
            Place {
                line: moved.line,
                hold,
            }
        });
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
            source,
            text,
        });
        if let Some(place) = place {
            self.land_at(place);
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
    fn measure_children(self: &Rc<Self>) {
        self.measure_each_child();
        // A held place stays where it was put as they take their sizes.
        self.follow_target();
    }

    fn measure_each_child(&self) {
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

/// The tag a list item's marker wears (`Tags::marker`).
const MARKER: &str = "taste-doc-marker";

/// A marker's font features: tabular figures.
const MARKER_FEATURES: &str = "tnum";

/// Write `text` into a buffer of its own, as `view` will show it.
fn render(view: &DocTextView, text: &str, base: &DocumentBase, readiness: &Readiness) -> Rendering {
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_enable_undo(false);
    let mut writer = Writer::new(&buffer, view.upcast_ref(), Some(base), Some(readiness));
    writer.write(text);
    Rendering {
        text: text.to_string(),
        ..writer.finish()
    }
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
    /// Each open list's gutter, the markers' column: a level's indent, or
    /// its widest marker's width when that is wider. Every item of a list,
    /// and every line of an item, starts its text at the gutter's end, and
    /// each marker ends there — an ordered list's numbers right-aligned on
    /// their periods — so a document's lists put their text in one column
    /// unless a number is too wide for it.
    gutters: Vec<i32>,
    /// The document's lists, counted before they are written
    /// (`markdown_view::list_shapes`), the next to begin first.
    shapes: std::vec::IntoIter<ListShape>,
    /// Each marker's width as measured, for the next item that has it.
    measured: HashMap<String, i32>,
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
    /// Where the source's lines landed: (0-based source line, offset).
    points: Vec<(u32, i32)>,
    /// The source lines of blocks begun and not yet written to: they land
    /// where their first line, or their widget, does.
    opening: Vec<u32>,
    /// The source line the event being written began on.
    source_line: u32,
    /// The last source line a run of text was written from.
    text_line: Option<u32>,
    /// A code block's lines by where they begin in its text: they land
    /// once the block's head is written.
    code_lines: Vec<(u32, i32)>,
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
    /// A list item's marker: its number in tabular figures, so numbers
    /// of as many digits are as wide.
    marker: gtk::TextTag,
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
            marker: add(gtk::TextTag::builder()
                .name(MARKER)
                .font_features(MARKER_FEATURES)
                .build()),
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
            gutters: Vec::new(),
            shapes: Vec::new().into_iter(),
            measured: HashMap::new(),
            task: None,
            item_fresh: false,
            quote: 0,
            code: None,
            table: None,
            image: None,
            blank: true,
            decor: Vec::new(),
            quote_starts: Vec::new(),
            points: Vec::new(),
            opening: Vec::new(),
            source_line: 0,
            text_line: None,
            code_lines: Vec::new(),
        }
    }

    /// The blocks begun since the last landed, landed where the buffer
    /// ends now.
    fn land_opening(&mut self) {
        if self.opening.is_empty() {
            return;
        }
        let offset = self.buffer.end_iter().offset();
        self.points
            .extend(self.opening.drain(..).map(|line| (line, offset)));
    }

    fn finish(mut self) -> Rendering {
        self.end_line();
        self.land_opening();
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
            source: SourceMap::new(self.points),
            text: String::new(),
        }
    }

    /// A margin tag for a line standing `left` from the view's edge. A
    /// negative `indent` is Pango's hanging one: the paragraph's first line
    /// starts at `left` and every line after it `-indent` further in.
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

    /// How wide `marker` is set in the view's font as a marker is, in
    /// tabular figures, to the nearest pixel: a margin is whole pixels, and
    /// the nearest puts the text after any marker within half of one of
    /// where the list's text stands.
    fn width_of(&mut self, marker: &str) -> i32 {
        if let Some(width) = self.measured.get(marker) {
            return *width;
        }
        // Measured where its text begins, before a letter, as the line
        // sets it: alone, a task's box took its space into the fallback
        // font it is drawn in and measured five pixels wider than drawn.
        let layout = self.view.create_pango_layout(Some(&format!("{marker}x")));
        let attributes = gtk::pango::AttrList::new();
        let mut features = gtk::pango::AttrFontFeatures::new(MARKER_FEATURES);
        features.set_end_index(marker.len() as u32);
        attributes.insert(features);
        layout.set_attributes(Some(&attributes));
        let scale = gtk::pango::SCALE;
        let width = (layout.index_to_pos(marker.len() as i32).x() + scale / 2) / scale;
        self.measured.insert(marker.to_string(), width);
        width
    }

    /// The widest marker a list shaped as `shape` has. Each of its
    /// numbers is measured — the last has the most digits, but a font
    /// without tabular figures need not make it the widest — and each
    /// measure is kept for the item that writes it.
    fn widest_marker(&mut self, shape: &ListShape) -> i32 {
        let mut widest = 0;
        if let Some(numbers) = shape.numbers() {
            for n in numbers {
                widest = widest.max(self.width_of(&format!("{n}. ")));
            }
        } else if shape.items > shape.tasks {
            widest = self.width_of("• ");
        }
        if shape.tasks > 0 {
            widest = widest.max(self.width_of("☐ ")).max(self.width_of("☑ "));
        }
        if widest > 0 {
            widest
        } else {
            self.width_of("• ")
        }
    }

    /// Open a line, if none is: its tags decided by where it stands, and
    /// an item's marker written at its start.
    fn begin_line(&mut self) {
        if self.line.is_some() {
            return;
        }
        self.land_opening();
        let mut tags = Vec::new();
        let depth = self.lists.len();
        let quote_left = self.quote as i32 * LEVEL_INDENT;
        let mut marker = None;
        if depth > 0 {
            // A nested list's gutter begins where the list it is in has
            // its text.
            let outer: i32 = self.gutters[..depth - 1].iter().sum();
            let left = MARGIN + quote_left + outer;
            // Where the list's text stands.
            let text_x = left + self.gutters.last().copied().unwrap_or(LEVEL_INDENT);
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
                // The marker ends where the list's text stands, numbers
                // right-aligned on their periods: the first line begins
                // the marker's width before it, and the lines it wraps
                // onto at it.
                let width = self.width_of(&text);
                tags.push(self.margin(text_x - width, -width));
                marker = Some(text);
            } else {
                // Under the item's text, where the list's text stands.
                tags.push(self.margin(text_x, 0));
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
            self.insert_marker(&marker);
        }
    }

    /// Insert an item's `marker` with the line's tags and the marker's.
    fn insert_marker(&mut self, marker: &str) {
        let mut tags = self.line.clone().unwrap_or_default();
        tags.push(self.tags.marker.clone());
        let refs: Vec<&gtk::TextTag> = tags.iter().collect();
        self.buffer
            .insert_with_tags(&mut self.buffer.end_iter(), marker, &refs);
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
        // A source line's first run is where that line is: a paragraph's
        // second line, or what follows a hard break.
        if self.text_line != Some(self.source_line) {
            self.text_line = Some(self.source_line);
            let offset = self.buffer.end_iter().offset();
            self.points.push((self.source_line, offset));
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
        self.land_opening();
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
        let lines = LineIndex::new(text);
        // The events whole, so each list is counted before it is written
        // (`list_shapes`): where its text stands is its widest marker's
        // end, and its widest number is its last item's.
        let events: Vec<_> = Parser::new_ext(text, options).into_offset_iter().collect();
        self.shapes = list_shapes(events.iter().map(|(event, _)| event)).into_iter();
        for (event, range) in events {
            self.source_line = lines.line_of(range.start);
            if crate::doc_source::opens_block(&event) {
                self.opening.push(self.source_line);
            }
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
                    // Each of its lines where it begins in the block's
                    // text, which the block writes as it is.
                    let mut chars = code.chars().count() as i32;
                    for (index, piece) in text.split_inclusive('\n').enumerate() {
                        self.code_lines
                            .push((self.source_line + index as u32, chars));
                        chars += piece.chars().count() as i32;
                    }
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
                let shape = self.shapes.next().unwrap_or(ListShape {
                    start,
                    ..ListShape::default()
                });
                let gutter = LEVEL_INDENT.max(self.widest_marker(&shape));
                self.lists.push(start);
                self.gutters.push(gutter);
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
                self.gutters.pop();
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
        let code_lines = std::mem::take(&mut self.code_lines);
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
        let start = self.buffer.end_iter().offset();
        self.points.extend(
            code_lines
                .into_iter()
                .map(|(line, chars)| (line, start + chars)),
        );
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
        /// The width it was last laid out at.
        pub(super) width: Cell<i32>,
        /// Told before a new width is laid out (`false`), and after
        /// (`true`).
        pub(super) on_width: RefCell<Option<Box<dyn Fn(bool)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DocTextView {
        const NAME: &'static str = "TasteDocTextView";
        type Type = super::DocTextView;
        type ParentType = gtk::TextView;
    }

    impl ObjectImpl for DocTextView {}

    impl WidgetImpl for DocTextView {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let was = self.width.replace(width);
            let told = was > 0 && was != width;
            if told {
                if let Some(on_width) = self.on_width.borrow().as_ref() {
                    on_width(false);
                }
            }
            self.parent_size_allocate(width, height, baseline);
            if told {
                if let Some(on_width) = self.on_width.borrow().as_ref() {
                    on_width(true);
                }
            }
        }
    }

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

    /// A source line lands on what it wrote: a heading, a paragraph's
    /// later line, a list's items, a code block's head and each of its
    /// own lines, a table's widget from any of its rows, and a blank line
    /// on the block before it.
    #[test]
    fn a_source_line_lands_on_what_it_wrote() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (_, rendering) = written(
                "# Title\n\nA paragraph that runs\nover two lines.\n\n- one\n- two\n\n\
                 ```\nfirst\nsecond\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nThe end.\n",
            );
            let buffer = &rendering.buffer;
            let at = |line: u32| {
                let start = buffer.iter_at_offset(rendering.source.place(line - 1));
                let mut end = start;
                end.forward_to_line_end();
                start.slice(&end).to_string()
            };
            assert_eq!(at(1), "Title");
            assert_eq!(at(3), "A paragraph that runs over two lines.");
            assert_eq!(at(4), "over two lines.");
            assert_eq!(at(5), "over two lines.");
            assert_eq!(at(6), "• one");
            assert_eq!(at(7), "• two");
            assert_eq!(at(9), "\u{fffc}", "the fence is the block's head");
            assert_eq!(at(10), "first");
            assert_eq!(at(11), "second");
            for row in 14..=16 {
                assert_eq!(at(row), "\u{fffc}", "row {row} is the table");
            }
            assert_eq!(at(18), "The end.");
        });
    }

    /// Where line `line` of `rendering` stands laid out in a view: where
    /// it begins (an item's marker), where its text begins past the marker,
    /// and where a line it wraps onto begins — Pango's hanging indent puts
    /// every line after a paragraph's first `-indent` past its margin.
    fn stands(rendering: &Rendering, line: i32) -> (i32, i32, i32) {
        let buffer = &rendering.buffer;
        let view = DocTextView::new();
        view.set_buffer(Some(buffer));
        let start = buffer.iter_at_line(line).unwrap();
        let mut text = start;
        let marker = buffer.tag_table().lookup(MARKER).unwrap();
        if start.starts_tag(Some(&marker)) {
            text.forward_to_tag_toggle(Some(&marker));
        }
        let (left, indent) = start
            .tags()
            .iter()
            .find(|tag| tag.is_left_margin_set())
            .map_or((MARGIN, 0), |tag| (tag.left_margin(), tag.indent()));
        let wraps = if indent < 0 { left - indent } else { left };
        (
            view.iter_location(&start).x(),
            view.iter_location(&text).x(),
            wraps,
        )
    }

    /// A list item's lines after its first stand under its text, not under
    /// its marker: the marker hangs.
    #[test]
    fn an_item_hangs_its_marker() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (_, rendering) = written("Before.\n\n- one\n\n  more of one\n- two\n");
            let (paragraph, _, _) = stands(&rendering, 0);
            let (marker, text, wraps) = stands(&rendering, 1);
            assert!(marker < text, "the marker before the text: {marker} {text}");
            assert!(
                marker >= paragraph,
                "within the column: {marker} {paragraph}"
            );
            assert_eq!(wraps, text, "a wrapped line stands under the text");
            assert_eq!(
                stands(&rendering, 2),
                (text, text, text),
                "a second paragraph stands under the text"
            );
        });
    }

    /// An ordered list that crosses from one digit to two stands every
    /// item's text at one place, the widest number's end, and ends each
    /// number there: right-aligned on the period, in tabular figures. A
    /// nested list is a list of its own, its gutter beginning at its
    /// parent's text.
    #[test]
    fn an_ordered_list_stands_its_text_at_one_place() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (text, rendering) = written(
                "8. eight\n9. nine\n\n   more of nine\n10. ten\n    1. one\n    2. two\n11. eleven\n",
            );
            assert_eq!(
                text,
                "8. eight\n9. nine\nmore of nine\n10. ten\n1. one\n2. two\n11. eleven"
            );
            let (_, text_x, _) = stands(&rendering, 0);
            let mut markers = Vec::new();
            for line in [0, 1, 3, 6] {
                let (marker, text, wraps) = stands(&rendering, line);
                assert_eq!((text, wraps), (text_x, text_x), "line {line}");
                markers.push(marker);
            }
            assert_eq!(markers[0], markers[1], "8 and 9 as wide");
            assert!(markers[1] > markers[2], "9 stands in from 10: {markers:?}");
            assert_eq!(markers[2], markers[3], "10 and 11 as wide");
            assert_eq!(stands(&rendering, 2), (text_x, text_x, text_x));
            let (nested, nested_text, _) = stands(&rendering, 4);
            assert!(nested >= text_x, "{nested} {text_x}");
            assert_eq!(stands(&rendering, 5).1, nested_text);
            // The marker wears tabular figures.
            let marker = rendering.buffer.tag_table().lookup(MARKER).unwrap();
            assert!(rendering.buffer.iter_at_line(3).unwrap().has_tag(&marker));
        });
    }

    /// Lists put their text in one column: a bullet's and a short ordered
    /// list's both stand a level in, each marker ending there.
    #[test]
    fn lists_share_a_column() {
        crate::gtk_test::on_gtk_thread("doc text: no display — skipped", || {
            let (_, rendering) = written("- bullet\n\nBetween.\n\n1. one\n2. two\n");
            let (bullet, bullet_text, _) = stands(&rendering, 0);
            let (number, number_text, _) = stands(&rendering, 3);
            assert_eq!(bullet_text, MARGIN + LEVEL_INDENT);
            assert_eq!(number_text, bullet_text);
            assert!(number < bullet, "a number is wider: {number} {bullet}");
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
            let (marker, second, wraps) = stands(&rendering, 1);
            assert!(marker < second);
            assert_eq!(wraps, second);
            assert_eq!(stands(&rendering, 2), (second, second, second));
            // A box measured as it is drawn: alone, it took its space into
            // its fallback font and its wrapped lines stood past its text.
            let (_, to_do, to_do_wraps) = stands(&rendering, 3);
            assert_eq!(to_do_wraps, to_do, "a box's wrapped lines under its text");
            assert_eq!(
                stands(&rendering, 4).1,
                to_do,
                "the boxes' text at one place"
            );
        });
    }
}
