//! Where a widget's first line of text actually sits.
//!
//! Lining a dot, a bullet or an icon up with a row of text means lining it
//! up with the first LINE, and a text widget's box is not that line: a
//! label's leading, a card's padding, and a text view's
//! `pixels_above_lines` each push the two apart by a pixel or three, in
//! three different systems, none of them visible in the source and all of
//! them different per row (the transcript's step titles, its captions, and
//! its prose were out by 2.5, 3.5, and 1.5 — David, 2026-09-08: "the first
//! line of text is still misaligned with the dot on some rows"). One
//! constant cannot answer that, so the line is measured.
//!
//! Both the transcript's timeline (which centres its dot on the line) and
//! the geometry probe (which reports the offset, so a misalignment is
//! measured instead of squinted at) ask this module, so there is one
//! answer rather than two that have to agree.

use adw::prelude::*;

/// The first line's top and height inside `widget`, in the widget's own
/// coordinates. `None` for anything that is not a label or a text view,
/// and for either of them while it holds no text.
pub fn first_line(widget: &gtk::Widget) -> Option<(i32, i32)> {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        if label.text().is_empty() {
            return None;
        }
        // The first line's box within the layout, plus the layout's own
        // offset within the widget. Not the line's `pixel_extents`: those
        // are measured from the baseline, so their `y` is an ascent rather
        // than a position.
        let (_, logical) = label.layout().iter().line_extents();
        let (_, offset_y) = label.layout_offsets();
        let scale = gtk::pango::SCALE;
        Some((offset_y + logical.y() / scale, logical.height() / scale))
    } else if let Some(view) = widget.downcast_ref::<gtk::TextView>() {
        let buffer = view.buffer();
        if buffer.char_count() == 0 {
            return None;
        }
        // The glyph box of the first character, not `line_yrange`: that
        // one counts the line's leading in, which is the very padding this
        // is here to see past.
        let location = view.iter_location(&buffer.start_iter());
        let (_, window_y) =
            view.buffer_to_window_coords(gtk::TextWindowType::Widget, location.x(), location.y());
        Some((window_y, location.height()))
    } else {
        None
    }
}

/// The first line's vertical centre inside `widget` — the y an adjacent
/// dot or icon has to sit on. Descends to find the first text `widget`
/// contains, so a whole card can be asked rather than the label buried in
/// it. `None` while there is no text yet to line up with.
pub fn first_line_mid(widget: &gtk::Widget) -> Option<i32> {
    let (child, (y, h)) = first_text(widget)?;
    let offset = if child == *widget {
        0.0
    } else {
        child.compute_bounds(widget)?.y()
    };
    let mid = offset.round() as i32 + y + h / 2;
    // A centre at or above the widget's own top edge is not a measurement:
    // it is a widget the frame clock has not laid out yet, whose layout
    // offsets are still answering from nowhere. Say so, rather than hand
    // back a number a caller would place something at.
    (mid > 0).then_some(mid)
}

/// The first descendant of `widget`, in tree order, that shows text and
/// has room to show it — the one the eye reads first.
fn first_text(widget: &gtk::Widget) -> Option<(gtk::Widget, (i32, i32))> {
    if widget.is_visible() {
        if let Some(line) = first_line(widget) {
            return Some((widget.clone(), line));
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            child = current.next_sibling();
            if let Some(found) = first_text(&current) {
                return Some(found);
            }
        }
    }
    None
}
