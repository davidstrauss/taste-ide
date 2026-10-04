//! **A table whose height agrees with its own layout.**
//!
//! Markdown tables were a `GtkGrid` of wrapping labels, and the grid
//! answered "how tall at this width?" as though every column sat at its
//! minimum — a single word wide, its cells a word per line — while it laid
//! the columns out far wider. A three-column table in a 367px chat column
//! asked for 1812px and drew in 325, and the transcript row kept the
//! difference: a reply that ended in a page of empty space (2026-10-04:
//! "Again, the super tall columns").
//!
//! So the columns are sized ONE way, in [`column_widths`], and both the
//! height a width is asked about and the allocation at that width use it:
//! each column its minimum, then what is left shared out by how much more
//! each one wants, and nobody past its natural width.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Each column's width out of `available`: the minimums first, then the
/// rest by each column's want (natural less minimum), capped at natural.
/// Below the minimums' total, the minimums — the table is then wider than
/// it was given, which is clipped rather than mis-measured.
pub fn column_widths(mins: &[i32], nats: &[i32], available: i32) -> Vec<i32> {
    let min_total: i32 = mins.iter().sum();
    let nat_total: i32 = nats.iter().sum();
    if available <= min_total {
        return mins.to_vec();
    }
    if available >= nat_total {
        return nats.to_vec();
    }
    let spare = available - min_total;
    let want_total: i64 = mins
        .iter()
        .zip(nats)
        .map(|(min, nat)| i64::from(nat - min))
        .sum();
    let mut widths: Vec<i32> = mins
        .iter()
        .zip(nats)
        .map(|(min, nat)| {
            let share = i64::from(spare) * i64::from(nat - min) / want_total.max(1);
            min + share as i32
        })
        .collect();
    // What integer division left over goes to the columns still short of
    // their natural width, left to right, so the widths add up exactly.
    let mut left = available - widths.iter().sum::<i32>();
    for (width, nat) in widths.iter_mut().zip(nats) {
        if left == 0 {
            break;
        }
        if *width < *nat {
            *width += 1;
            left -= 1;
        }
    }
    widths
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct Table {
        /// The cells, row by row, every row `columns` long.
        pub(super) rows: RefCell<Vec<Vec<gtk::Widget>>>,
        pub(super) columns: Cell<usize>,
        /// The rule under the head row.
        pub(super) rule: RefCell<Option<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Table {
        const NAME: &'static str = "TasteMarkdownTable";
        type Type = super::Table;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Table {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl Table {
        /// Each column's minimum and natural width.
        fn column_sizes(&self) -> (Vec<i32>, Vec<i32>) {
            let columns = self.columns.get();
            let mut mins = vec![0; columns];
            let mut nats = vec![0; columns];
            for row in self.rows.borrow().iter() {
                for (column, cell) in row.iter().enumerate() {
                    let (min, nat, _, _) = cell.measure(gtk::Orientation::Horizontal, -1);
                    mins[column] = mins[column].max(min);
                    nats[column] = nats[column].max(nat);
                }
            }
            (mins, nats)
        }

        /// Each row's height with the columns at `widths`.
        fn row_heights(&self, widths: &[i32]) -> Vec<i32> {
            self.rows
                .borrow()
                .iter()
                .map(|row| {
                    row.iter()
                        .zip(widths)
                        .map(|(cell, width)| cell.measure(gtk::Orientation::Vertical, *width).1)
                        .max()
                        .unwrap_or(0)
                })
                .collect()
        }

        fn rule_height(&self) -> i32 {
            self.rule
                .borrow()
                .as_ref()
                .map(|rule| rule.measure(gtk::Orientation::Vertical, -1).1)
                .unwrap_or(0)
        }
    }

    impl WidgetImpl for Table {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let (mins, nats) = self.column_sizes();
            match orientation {
                gtk::Orientation::Horizontal => (mins.iter().sum(), nats.iter().sum(), -1, -1),
                _ => {
                    let widths = if for_size < 0 {
                        nats
                    } else {
                        column_widths(&mins, &nats, for_size)
                    };
                    let height = self.row_heights(&widths).iter().sum::<i32>() + self.rule_height();
                    (height, height, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let (mins, nats) = self.column_sizes();
            let widths = column_widths(&mins, &nats, width);
            let heights = self.row_heights(&widths);
            let total: i32 = widths.iter().sum();
            let rule_height = self.rule_height();
            let mut y = 0;
            for (index, (row, height)) in self.rows.borrow().iter().zip(&heights).enumerate() {
                let mut x = 0;
                for (cell, cell_width) in row.iter().zip(&widths) {
                    cell.size_allocate(&gtk::Allocation::new(x, y, *cell_width, *height), -1);
                    x += cell_width;
                }
                y += height;
                if index == 0 {
                    if let Some(rule) = self.rule.borrow().as_ref() {
                        rule.size_allocate(&gtk::Allocation::new(0, y, total, rule_height), -1);
                    }
                    y += rule_height;
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct Table(ObjectSubclass<imp::Table>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Table {
    /// A table of `rows` of cells, the first the head, with a rule under
    /// it. Rows shorter than the longest are padded with empty labels.
    pub fn new(rows: Vec<Vec<gtk::Widget>>) -> Self {
        let table: Self = glib::Object::builder().build();
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let rows: Vec<Vec<gtk::Widget>> = rows
            .into_iter()
            .map(|mut row| {
                while row.len() < columns {
                    row.push(gtk::Label::new(None).upcast());
                }
                row
            })
            .collect();
        for (index, row) in rows.iter().enumerate() {
            for cell in row {
                cell.set_parent(&table);
            }
            if index == 0 {
                let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
                rule.set_parent(&table);
                *table.imp().rule.borrow_mut() = Some(rule.upcast());
            }
        }
        table.imp().columns.set(columns);
        *table.imp().rows.borrow_mut() = rows;
        table
    }
}

#[cfg(test)]
mod tests {
    use super::column_widths;

    #[test]
    fn columns_take_their_minimum_then_share_by_want() {
        // Room for everything: natural widths.
        assert_eq!(column_widths(&[30, 30], &[100, 50], 200), [100, 50]);
        // Too little for the minimums: the minimums.
        assert_eq!(column_widths(&[30, 30], &[100, 50], 40), [30, 30]);
        // Between: the 60 spare goes 70:20 by want, and adds up exactly.
        let widths = column_widths(&[30, 30], &[100, 50], 120);
        assert_eq!(widths.iter().sum::<i32>(), 120);
        assert_eq!(widths, [77, 43]);
        // A column that wants nothing more gets nothing more.
        assert_eq!(column_widths(&[40, 30], &[40, 90], 100), [40, 60]);
    }
}
