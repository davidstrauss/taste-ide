//! A holder that gives a scrolled list a whole number of rows.
//!
//! The file tree's list takes the column's slack, and the slack is
//! whatever the sections under it leave, so its height was any number of
//! pixels and a row was always sliced at one edge — the top one once the
//! list was scrolled to its end (David, 2026-09-28: "This text is still
//! cutting off on the top"). The holder rounds the height it is given down
//! to the rows' own pitch, the list's top padding included, and leaves
//! the remainder, under a row's height, empty below the list. Rows of
//! mixed heights (the Dirty filter's two-line rows) have no pitch to round
//! to and get the whole height, as before.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct WholeRows {
        /// A pass that found the rows not laid out yet has asked for one
        /// more; this keeps it to one.
        pub(super) retry: Cell<bool>,
        /// The height last given, so a resize — and only a resize — puts
        /// the scroll offset back on a row.
        pub(super) last_height: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WholeRows {
        const NAME: &'static str = "TasteWholeRows";
        type Type = super::WholeRows;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for WholeRows {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for WholeRows {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match self.obj().first_child() {
                Some(child) => {
                    let (min, nat, _, _) = child.measure(orientation, for_size);
                    (min, nat, -1, -1)
                }
                None => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let obj = self.obj();
            let Some(child) = obj.first_child() else {
                return;
            };
            let scroller = child.downcast_ref::<gtk::ScrolledWindow>().cloned();
            let measured = match &scroller {
                Some(scroller) => whole_rows(scroller, height),
                None => Some((height, None)),
            };
            // A resize leaves the list at the offset it had, which is where
            // the old height put it: scrolled to the end, that is mid-row
            // for the new one, and the top row came out sliced (David,
            // 2026-09-28: "it occurs after I reposition the window using
            // [Windows] + [Right]"). Put back on a row, after this pass —
            // an adjustment changed inside an allocation is a relayout
            // inside the one under way.
            if let (Some(scroller), Some((_, Some(pitch)))) = (&scroller, measured) {
                if self.last_height.replace(height) != height {
                    let scroller = scroller.clone();
                    glib::idle_add_local_once(move || snap_offset(&scroller, pitch));
                }
            }
            let snapped = measured.map(|(height, _)| height);
            // Before the rows are laid out there is no pitch to read: the
            // whole height now, and one more pass once they are.
            let height = match snapped {
                Some(snapped) => {
                    self.retry.set(false);
                    snapped
                }
                None => {
                    if !self.retry.replace(true) {
                        let weak = obj.downgrade();
                        glib::idle_add_local_once(move || {
                            if let Some(holder) = weak.upgrade() {
                                holder.queue_allocate();
                            }
                        });
                    }
                    height
                }
            };
            child.allocate(width, height, -1, None);
        }
    }
}

glib::wrapper! {
    pub struct WholeRows(ObjectSubclass<imp::WholeRows>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl WholeRows {
    pub fn new(scroller: &gtk::ScrolledWindow) -> Self {
        let holder: Self = glib::Object::new();
        holder.set_vexpand(true);
        scroller.set_parent(&holder);
        holder
    }
}

/// `height` rounded down to the list's whole rows, its top padding
/// included, with the rows' pitch; `height` itself, and no pitch, when the
/// rows differ in pitch, there is no list, or there are too few rows laid
/// out to measure one; `None` when none are laid out yet.
fn whole_rows(scroller: &gtk::ScrolledWindow, height: i32) -> Option<(i32, Option<i32>)> {
    let Some(list) = scroller.child().and_downcast::<gtk::ListView>() else {
        return Some((height, None));
    };
    // Where each laid-out row starts. A row's own height leaves out its
    // CSS margin, so the pitch is read as the distance from one row to the
    // next; a recycled row the list has not placed is 0 tall and skipped.
    let mut tops = Vec::new();
    let mut child = list.first_child();
    while let Some(row) = child {
        child = row.next_sibling();
        if !row.is_visible() || row.height() <= 0 {
            continue;
        }
        if let Some(point) = row.compute_point(&list, &gtk::graphene::Point::new(0.0, 0.0)) {
            tops.push(point.y().round() as i32);
        }
    }
    if tops.is_empty() {
        return None;
    }
    tops.sort_unstable();
    let steps: Vec<i32> = tops.windows(2).map(|w| w[1] - w[0]).collect();
    let Some(&pitch) = steps.first() else {
        return Some((height, None));
    };
    if pitch <= 0 || steps.iter().any(|step| *step != pitch) {
        return Some((height, None));
    }
    // Where the first row sits with the list scrolled to its top — the
    // list's own padding, which a sidebar list has — read off whichever
    // row is laid out first, since a recycled list need not hold row 0.
    let offset = scroller.vadjustment().value().round() as i32;
    let pad = (tops[0] + offset).rem_euclid(pitch);
    Some((snap(height, pitch, pad), Some(pitch)))
}

/// The scroll offset put on a row: the nearest multiple of the pitch, so
/// a row starts where the first one does at the top — below the list's
/// padding — and the height's whole rows end on the viewport's edge. Never
/// past the end, where the nearest whole row below it is taken.
fn snap_offset(scroller: &gtk::ScrolledWindow, pitch: i32) {
    let adjustment = scroller.vadjustment();
    let max = (adjustment.upper() - adjustment.page_size()).max(0.0);
    let value = adjustment.value();
    let snapped = offset_on_row(value, max, pitch as f64);
    if (snapped - value).abs() >= 0.5 {
        adjustment.set_value(snapped);
    }
}

fn offset_on_row(value: f64, max: f64, pitch: f64) -> f64 {
    let nearest = (value / pitch).round() * pitch;
    if nearest <= max {
        nearest
    } else {
        (max / pitch).floor() * pitch
    }
}

/// The tallest `pad + n * pitch` that fits `height`; `height` when not
/// even one row does.
fn snap(height: i32, pitch: i32, pad: i32) -> i32 {
    let rows = (height - pad) / pitch;
    if rows < 1 {
        height
    } else {
        pad + rows * pitch
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_resized_list_rests_on_a_row() {
        assert_eq!(super::offset_on_row(50.0, 400.0, 22.0), 44.0);
        assert_eq!(super::offset_on_row(0.0, 400.0, 22.0), 0.0);
        // Scrolled to an end that is not on a row: the row before it.
        assert_eq!(super::offset_on_row(399.0, 399.0, 22.0), 396.0);
    }

    #[test]
    fn a_height_rounds_down_to_whole_rows_after_the_padding() {
        assert_eq!(super::snap(100, 24, 6), 78);
        assert_eq!(super::snap(78, 24, 6), 78);
        assert_eq!(super::snap(20, 24, 6), 20);
    }
}
