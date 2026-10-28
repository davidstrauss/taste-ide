//! **Every match of the query on screen, lit as it is typed.**
//!
//! The one query's listing (SEARCH.md rule 2) waits a quarter second after
//! the last keystroke and then has every surface answer it, which is right
//! for a project's worth of sources and wrong for the document in front:
//! a find in a document is expected to colour what is on screen at once,
//! key by key (David, 2026-10-27: "highlight matches in the active
//! viewport and feel as fast as doing a typical, document-local Ctrl-F
//! search"). So the text views of documents — a file's buffer, a log's,
//! a task's output — are watched here, and the visible lines of each one
//! on screen are searched at every keystroke, with no delay: a screen of
//! text is microseconds, whatever the file's length. They are searched
//! again when the view scrolls, its text changes, or it comes on screen,
//! once a frame however often it is asked. The matches wear the search
//! hue's wash, under the selected hit's solid (`palette::highlight_range`),
//! and are found by the query's own matcher (`Query::ranges`), so what is
//! lit and what the listing counts are one answer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::search::Query;

/// The tag the matches on screen wear.
const MATCH_TAG: &str = "taste-search-match";

thread_local! {
    /// The query as typed, ahead of the listing's.
    static QUERY: RefCell<Query> = RefCell::new(Query::default());
    /// The views watched.
    static VIEWS: RefCell<Vec<glib::WeakRef<gtk::TextView>>> = const { RefCell::new(Vec::new()) };
}

/// The query as it now stands in the box, keystroke by keystroke: every
/// watched view on screen is lit again for it.
pub fn set_query(query: &Query) {
    let changed = QUERY.with(|current| {
        let mut current = current.borrow_mut();
        let changed = current.text != query.text;
        *current = query.clone();
        changed
    });
    if !changed {
        return;
    }
    VIEWS.with(|views| {
        views.borrow_mut().retain(|watched| {
            let Some(view) = watched.upgrade() else {
                return false;
            };
            if view.is_mapped() {
                light(&view);
            } else {
                // Off screen: its matches are taken off now, and lit when
                // it comes back (`watch`'s map).
                clear(&view.buffer());
            }
            true
        });
    });
}

/// Keep `view`'s visible matches lit: at each keystroke, and when it
/// scrolls, its text changes, or it comes on screen.
pub fn watch(view: &impl IsA<gtk::TextView>) {
    let view = view.as_ref().clone();
    let queued = Rc::new(Cell::new(false));
    VIEWS.with(|views| views.borrow_mut().push(view.downgrade()));
    let relight = {
        let view = view.downgrade();
        let queued = queued.clone();
        Rc::new(move || {
            if let Some(view) = view.upgrade() {
                queue(&view, &queued);
            }
        })
    };
    {
        let relight = relight.clone();
        view.connect_map(move |_| relight());
    }
    {
        let relight = relight.clone();
        view.buffer().connect_changed(move |_| relight());
    }
    // The vertical adjustment arrives with the scrolled window, and a view
    // moved to another one gets a new adjustment.
    let hook = {
        let relight = relight.clone();
        move |view: &gtk::TextView| {
            if let Some(adjustment) = view.vadjustment() {
                let on_value = relight.clone();
                adjustment.connect_value_changed(move |_| on_value());
                let on_extent = relight.clone();
                adjustment.connect_changed(move |_| on_extent());
            }
        }
    };
    hook(&view);
    view.connect_notify_local(Some("vadjustment"), move |view, _| hook(view));
}

/// Light `view` at its next frame, once however often it is asked.
fn queue(view: &gtk::TextView, queued: &Rc<Cell<bool>>) {
    if queued.replace(true) {
        return;
    }
    let queued = queued.clone();
    view.add_tick_callback(move |view, _| {
        queued.set(false);
        if view.is_mapped() {
            light(view);
        }
        glib::ControlFlow::Break
    });
}

/// The query's matches in the lines `view` shows, lit, and every other
/// match it had lit taken off.
fn light(view: &gtk::TextView) {
    let buffer = view.buffer();
    clear(&buffer);
    let query = QUERY.with(|query| query.borrow().clone());
    if query.is_empty() {
        return;
    }
    // By line, not by point: a point in the margin, or past a short line's
    // end, is over no text, and asked by point the view answers nothing.
    let rect = view.visible_rect();
    let (top, _) = view.line_at_y(rect.y());
    let (mut bottom, _) = view.line_at_y(rect.y() + rect.height());
    if !bottom.ends_line() {
        bottom.forward_to_line_end();
    }
    // The slice, not the text: it keeps a picture's or a widget's place as
    // a character, so its offsets are the buffer's.
    let slice = buffer.slice(&top, &bottom, true);
    let base = top.offset();
    let tag = match_tag(&buffer);
    let mut chars = 0;
    let mut at = 0;
    for (from, to) in query.ranges(&slice) {
        chars += slice[at..from].chars().count();
        let start = chars;
        chars += slice[from..to].chars().count();
        at = to;
        buffer.apply_tag(
            &tag,
            &buffer.iter_at_offset(base + start as i32),
            &buffer.iter_at_offset(base + chars as i32),
        );
    }
}

fn clear(buffer: &gtk::TextBuffer) {
    if let Some(tag) = buffer.tag_table().lookup(MATCH_TAG) {
        buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    }
}

/// The buffer's match tag, made the first time, and kept under the
/// selected hit's: the one hit stepped to stays solid among the washed.
fn match_tag(buffer: &gtk::TextBuffer) -> gtk::TextTag {
    let table = buffer.tag_table();
    let tag = table.lookup(MATCH_TAG).unwrap_or_else(|| {
        let tag = gtk::TextTag::new(Some(MATCH_TAG));
        table.add(&tag);
        tag
    });
    let dark = adw::StyleManager::default().is_dark();
    tag.set_background_rgba(Some(&crate::palette::match_background(dark)));
    crate::palette::keep_hit_above(buffer, &tag);
    tag
}
