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
            let snapped = match child.downcast_ref::<gtk::ScrolledWindow>() {
                Some(scroller) => whole_rows_height(scroller, height),
                None => Some(height),
            };
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
/// included; `height` itself when the rows differ in pitch, there is no
/// list, or there are too few rows laid out to measure one; `None` when
/// none are laid out yet.
fn whole_rows_height(scroller: &gtk::ScrolledWindow, height: i32) -> Option<i32> {
    let Some(list) = scroller.child().and_downcast::<gtk::ListView>() else {
        return Some(height);
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
        return Some(height);
    };
    if pitch <= 0 || steps.iter().any(|step| *step != pitch) {
        return Some(height);
    }
    // Where the first row sits with the list scrolled to its top — the
    // list's own padding, which a sidebar list has — read off whichever
    // row is laid out first, since a recycled list need not hold row 0.
    let offset = scroller.vadjustment().value().round() as i32;
    let pad = (tops[0] + offset).rem_euclid(pitch);
    Some(snap(height, pitch, pad))
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
    fn a_height_rounds_down_to_whole_rows_after_the_padding() {
        assert_eq!(super::snap(100, 24, 6), 78);
        assert_eq!(super::snap(78, 24, 6), 78);
        assert_eq!(super::snap(20, 24, 6), 20);
    }
}
