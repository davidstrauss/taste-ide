//! Selecting across a rendered document drawn as labels. A chat answer is
//! a column of labels — runs of prose, code blocks, table cells — and a
//! GtkLabel selects only within itself, so a drag stopped at the edge of
//! the label it began in, and the view did not scroll under it (David,
//! 2026-11-09: "I can't select long spans of text"). A run is cut at a few
//! thousand bytes for the drawing's sake (`markdown_view::layout`), which
//! made the span a selection could reach a screen or so.
//!
//! An editor's Markdown preview is a text view instead (`crate::doc_text`),
//! which selects as a whole by itself; this is for what is still labels.
//!
//! [`install`] puts a drag over the document's box that, once it has moved,
//! takes the gesture from the label under it and selects through every
//! label between where it began and where the pointer is, in the order the
//! document reads, scrolling the view while the pointer is past its edge.
//! A copy of that selection by Ctrl+C is the whole of it ([`crate::copy`]),
//! and so is the primary selection; Ctrl+A selects the whole document. A
//! click, a double-click, and a link are still the label's own.
//!
//! The span is PAINTED on the labels, a background in their text's
//! attributes, rather than set as each label's own selection: a label that
//! selects takes the display's primary selection, and the label that held
//! it lets its own selection go, so only the last label of a span stayed
//! lit.

use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// The key the document's selection is kept under on its box.
const KEY: &str = "taste-doc-selection";

/// How far into the view's top or bottom edge the pointer scrolls it.
const EDGE: f64 = 12.0;

/// The edge scroll's speed, in pixels a second for each pixel the pointer
/// is past the edge, and the most it goes.
const SPEED: f64 = 20.0;
const MAX_SPEED: f64 = 4000.0;

/// A place in the document: a label's index among its labels, and a byte
/// offset in the label's text, as its layout has it.
type Place = (usize, u32);

/// A label the span is painted on: the attributes it had before, given
/// back when the span leaves it, and its part of the span, in bytes.
struct Painted {
    original: Option<gtk::pango::AttrList>,
    span: (u32, u32),
}

pub struct Selection {
    root: glib::WeakRef<gtk::Widget>,
    /// The document's selectable labels in reading order, as the current
    /// selection found them.
    labels: RefCell<Vec<gtk::Label>>,
    anchor: Cell<Option<Place>>,
    /// The span selected, ordered, when it was this that selected it.
    range: Cell<Option<(Place, Place)>>,
    /// The labels the span is painted on, by their index in `labels`.
    painted: RefCell<HashMap<usize, Painted>>,
    /// While a drag is on: the pointer, in the scrolled window's
    /// coordinates, which hold still while the document scrolls under it.
    pointer: Cell<Option<(f64, f64)>>,
    dragging: Cell<bool>,
}

/// Make `root`, a document's box, selectable across its labels.
pub fn install(root: &impl IsA<gtk::Widget>) {
    let root = root.as_ref();
    let selection = Rc::new(Selection {
        root: root.downgrade(),
        labels: RefCell::new(Vec::new()),
        anchor: Cell::new(None),
        range: Cell::new(None),
        painted: RefCell::new(HashMap::new()),
        pointer: Cell::new(None),
        dragging: Cell::new(false),
    });

    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    // Before the labels: a drag that has moved is taken from them.
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let selection = selection.clone();
        drag.connect_drag_begin(move |_, x, y| {
            selection.begin(graphene::Point::new(x as f32, y as f32));
        });
    }
    {
        let selection = selection.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((x, y)) = gesture.start_point() else {
                return;
            };
            if !selection.dragging.get() {
                // Still a click: the label's, which may be a link's.
                if dx.hypot(dy) < 6.0 {
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                selection.start_drag();
            }
            selection.follow(graphene::Point::new((x + dx) as f32, (y + dy) as f32));
        });
    }
    {
        let selection = selection.clone();
        drag.connect_drag_end(move |_, _, _| selection.finish());
    }
    {
        let selection = selection.clone();
        drag.connect_cancel(move |_, _| selection.finish());
    }
    root.add_controller(drag);

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let selection = selection.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let plain = modifiers
                & (gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::SHIFT_MASK
                    | gtk::gdk::ModifierType::ALT_MASK);
            if plain == gtk::gdk::ModifierType::CONTROL_MASK
                && matches!(key, gtk::gdk::Key::a | gtk::gdk::Key::A)
            {
                selection.select_all();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    root.add_controller(keys);

    // SAFETY: the key holds only an `Rc<Selection>`, set here once and read
    // back as that type by `of` alone, on the main thread.
    unsafe { root.set_data(KEY, selection) };
}

/// The selection of the document `widget` is in, if it is in one.
fn of(widget: &gtk::Widget) -> Option<Rc<Selection>> {
    let mut at = Some(widget.clone());
    while let Some(widget) = at {
        // SAFETY: only `install` sets the key, and always to this type.
        if let Some(found) = unsafe { widget.data::<Rc<Selection>>(KEY) } {
            return Some(unsafe { found.as_ref() }.clone());
        }
        at = widget.parent();
    }
    None
}

/// Make Ctrl+C in `label` copy the document's span whole, when there is
/// one and the label has no selection of its own made since.
pub fn copies_across(label: &gtk::Label) {
    label.connect_copy_clipboard(|label| {
        if label.selection_bounds().is_some() {
            return;
        }
        let Some(selection) = of(label.upcast_ref()) else {
            return;
        };
        if let Some(text) = selection.text() {
            let _ = crate::copy::whole(label, &text);
            label.stop_signal_emission_by_name("copy-clipboard");
        }
    });
}

impl Selection {
    fn root(&self) -> Option<gtk::Widget> {
        self.root.upgrade()
    }

    /// The document's labels as it is now, in reading order.
    fn collect(&self) {
        let mut found = Vec::new();
        if let Some(root) = self.root() {
            let mut stack = vec![root];
            while let Some(widget) = stack.pop() {
                if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                    if label.is_selectable() && label.is_drawable() {
                        found.push(label.clone());
                    }
                    continue;
                }
                let mut child = widget.last_child();
                while let Some(c) = child {
                    child = c.prev_sibling();
                    stack.push(c);
                }
            }
        }
        *self.labels.borrow_mut() = found;
    }

    /// A press: whatever was selected anywhere in the document goes, and
    /// the place under it is where a drag would begin.
    fn begin(&self, at: graphene::Point) {
        self.clear();
        self.collect();
        self.anchor.set(self.locate(at));
    }

    /// Nothing selected: the span's paint taken off, and every label's own
    /// selection let go.
    fn clear(&self) {
        let labels = self.labels.borrow();
        for (index, painted) in self.painted.borrow_mut().drain() {
            if let Some(label) = labels.get(index) {
                label.set_attributes(painted.original.as_ref());
            }
        }
        for label in labels.iter() {
            if label.selection_bounds().is_some() {
                label.select_region(0, 0);
            }
        }
        self.range.set(None);
    }

    /// The press has moved far enough to be a drag: the keyboard to the
    /// label it began in, so that Ctrl+C reaches this, and the view
    /// scrolled while the pointer is past its edge.
    fn start_drag(self: &Rc<Self>) {
        self.dragging.set(true);
        // What the label began selecting of itself before the drag was
        // taken from it.
        for label in self.labels.borrow().iter() {
            if label.selection_bounds().is_some() {
                label.select_region(0, 0);
            }
        }
        let Some(root) = self.root() else { return };
        if let Some((index, _)) = self.anchor.get() {
            let focus_inside = root
                .root()
                .and_then(|window| window.focus())
                .is_some_and(|focus| focus.is_ancestor(&root) || focus == root);
            if !focus_inside {
                if let Some(label) = self.labels.borrow().get(index) {
                    label.grab_focus();
                }
            }
        }
        let selection = self.clone();
        let last: Cell<Option<i64>> = Cell::new(None);
        root.add_tick_callback(move |_, clock| {
            if !selection.dragging.get() {
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            let seconds = last
                .replace(Some(now))
                .map_or(0.0, |then| (now - then) as f64 / 1_000_000.0);
            selection.edge_scroll(seconds.min(0.1));
            glib::ControlFlow::Continue
        });
    }

    /// The pointer at `at`, in the document's coordinates.
    fn follow(&self, at: graphene::Point) {
        let Some(root) = self.root() else { return };
        if let Some(scroller) = scroller_of(&root) {
            if let Some(point) = root.compute_point(&scroller, &at) {
                self.pointer
                    .set(Some((f64::from(point.x()), f64::from(point.y()))));
            }
        }
        self.extend_to(at);
    }

    /// Select from the anchor to `at`.
    fn extend_to(&self, at: graphene::Point) {
        let (Some(anchor), Some(focus)) = (self.anchor.get(), self.locate(at)) else {
            return;
        };
        let span = if focus < anchor {
            (focus, anchor)
        } else {
            (anchor, focus)
        };
        self.apply(span);
    }

    /// A step of the view toward the pointer while it is past an edge, and
    /// the selection carried to what has come under it.
    ///
    /// The speed is by the second, not the frame — a slow frame does not
    /// slow the view — and grows with how far past the edge the pointer
    /// is, as a text view's does.
    fn edge_scroll(&self, seconds: f64) {
        let (Some(root), Some((x, y))) = (self.root(), self.pointer.get()) else {
            return;
        };
        let Some(scroller) = scroller_of(&root) else {
            return;
        };
        // What is under the pointer now, as the last frame laid it out.
        if let Some(at) = scroller.compute_point(&root, &graphene::Point::new(x as f32, y as f32)) {
            self.extend_to(at);
        }
        let height = f64::from(scroller.height());
        let past = if y < EDGE {
            y - EDGE
        } else if y > height - EDGE {
            y - (height - EDGE)
        } else {
            return;
        };
        let adjustment = scroller.vadjustment();
        let step = (past * SPEED).clamp(-MAX_SPEED, MAX_SPEED) * seconds;
        let top = adjustment.upper() - adjustment.page_size();
        adjustment.set_value((adjustment.value() + step).clamp(adjustment.lower(), top));
    }

    /// The drag is over: what it selected is the primary selection too,
    /// whole.
    fn finish(&self) {
        if !self.dragging.replace(false) {
            return;
        }
        self.pointer.set(None);
        self.own_primary();
    }

    fn own_primary(&self) {
        if let (Some(root), Some(text)) = (self.root(), self.text()) {
            let _ = crate::copy::onto(&root.primary_clipboard(), &text);
        }
    }

    /// The whole document selected.
    fn select_all(&self) {
        self.clear();
        self.collect();
        let last = {
            let labels = self.labels.borrow();
            let Some(last) = labels.last() else { return };
            (labels.len() - 1, text_len(last))
        };
        self.anchor.set(Some((0, 0)));
        self.apply(((0, 0), last));
        self.own_primary();
    }

    /// Where in the document `at` falls: in the label under it, or, between
    /// labels, at the end of the last one above it.
    fn locate(&self, at: graphene::Point) -> Option<Place> {
        let root = self.root()?;
        let labels = self.labels.borrow();
        let (x, y) = (at.x(), at.y());
        let mut row: Option<(usize, f32)> = None;
        let mut above: Option<usize> = None;
        for (index, label) in labels.iter().enumerate() {
            let Some(bounds) = label.compute_bounds(&root) else {
                continue;
            };
            let (top, bottom) = (bounds.y(), bounds.y() + bounds.height());
            if y >= top && y < bottom {
                let (left, right) = (bounds.x(), bounds.x() + bounds.width());
                let distance = if x < left {
                    left - x
                } else if x > right {
                    x - right
                } else {
                    0.0
                };
                if row.is_none_or(|(_, best)| distance < best) {
                    row = Some((index, distance));
                }
            } else if bottom <= y {
                above = Some(index);
            }
        }
        if let Some((index, _)) = row {
            let label = &labels[index];
            let point = root.compute_point(label, &at)?;
            return Some((index, offset_at(label, point.x(), point.y())));
        }
        match above {
            Some(index) => Some((index, text_len(&labels[index]))),
            None => (!labels.is_empty()).then_some((0, 0)),
        }
    }

    /// Show `span` as the selection, repainting only the labels whose
    /// part of it changed: a label lays itself out again for each.
    fn apply(&self, span: (Place, Place)) {
        let labels = self.labels.borrow();
        let mut painted = self.painted.borrow_mut();
        let ((lo, lo_at), (hi, hi_at)) = span;
        let (from, to) = match self.range.get() {
            Some(((was_lo, _), (was_hi, _))) => (lo.min(was_lo), hi.max(was_hi)),
            None => (lo, hi),
        };
        for index in from..=to.min(labels.len().saturating_sub(1)) {
            let label = &labels[index];
            let wanted = if index < lo || index > hi {
                None
            } else {
                let start = if index == lo { lo_at } else { 0 };
                let end = if index == hi { hi_at } else { text_len(label) };
                (start < end).then_some((start, end))
            };
            if painted.get(&index).map(|p| p.span) == wanted {
                continue;
            }
            let original = match painted.remove(&index) {
                Some(was) => was.original,
                None => label.attributes(),
            };
            match wanted {
                Some(part) => {
                    label.set_attributes(Some(&with_span(original.as_ref(), part)));
                    painted.insert(
                        index,
                        Painted {
                            original,
                            span: part,
                        },
                    );
                }
                None => label.set_attributes(original.as_ref()),
            }
        }
        self.range.set(Some(span));
    }

    /// The text this selected, whole.
    fn text(&self) -> Option<String> {
        let ((lo, lo_at), (hi, hi_at)) = self.range.get()?;
        let labels = self.labels.borrow();
        let mut out = String::new();
        let mut previous: Option<&gtk::Label> = None;
        for (index, label) in labels.iter().enumerate().take(hi + 1).skip(lo) {
            let text = label.layout().text();
            let start = if index == lo { lo_at as usize } else { 0 };
            let end = if index == hi {
                hi_at as usize
            } else {
                text.len()
            };
            if let Some(previous) = previous {
                out.push_str(separator(previous, label));
            }
            out.push_str(text.get(start.min(end)..end).unwrap_or_default());
            previous = Some(label);
        }
        (!out.is_empty()).then_some(out)
    }
}

/// What stands between two labels' parts of a copied selection: a tab
/// between cells of a table's row, a line between its rows, and a blank
/// line between blocks.
fn separator(previous: &gtk::Label, next: &gtk::Label) -> &'static str {
    match previous.parent() {
        Some(parent) if next.parent().as_ref() == Some(&parent) => {
            let top = |label: &gtk::Label| label.compute_bounds(&parent).map(|b| b.y());
            if top(previous) == top(next) {
                "\t"
            } else {
                "\n"
            }
        }
        _ => "\n\n",
    }
}

/// The length of `label`'s text in bytes, as its layout has it.
fn text_len(label: &gtk::Label) -> u32 {
    label.layout().text().len() as u32
}

/// `original` with the span's paint over bytes `start..end`: the accent's
/// wash, as a label's own selection wears it.
fn with_span(
    original: Option<&gtk::pango::AttrList>,
    (start, end): (u32, u32),
) -> gtk::pango::AttrList {
    let attributes = original
        .and_then(gtk::pango::AttrList::copy)
        .unwrap_or_default();
    let accent = adw::StyleManager::default().accent_color_rgba();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 65535.0) as u16;
    let mut wash = gtk::pango::AttrColor::new_background(
        channel(accent.red()),
        channel(accent.green()),
        channel(accent.blue()),
    );
    wash.set_start_index(start);
    wash.set_end_index(end);
    attributes.insert(wash);
    let mut alpha = gtk::pango::AttrInt::new_background_alpha(channel(0.3));
    alpha.set_start_index(start);
    alpha.set_end_index(end);
    attributes.insert(alpha);
    attributes
}

/// The byte offset in `label`'s text nearest its point `(x, y)`.
fn offset_at(label: &gtk::Label, x: f32, y: f32) -> u32 {
    let layout = label.layout();
    let (left, top) = label.layout_offsets();
    let scale = gtk::pango::SCALE as f32;
    let (_, index, trailing) = layout.xy_to_index(
        ((x - left as f32) * scale) as i32,
        ((y - top as f32) * scale) as i32,
    );
    let text = layout.text();
    let mut at = (index.max(0) as usize).min(text.len());
    // Past the character the point is in the trailing half of.
    for _ in 0..trailing.max(0) {
        at += text[at..].chars().next().map_or(0, char::len_utf8);
    }
    at as u32
}

/// The scrolled window the document scrolls in.
fn scroller_of(root: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
    root.ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast()
}
