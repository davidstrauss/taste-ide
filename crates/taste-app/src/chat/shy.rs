//! **Whole, or not at all**: a one-child widget that asks for none of the
//! width its child needs, and shows the child only while it is given that
//! much.
//!
//! For a fact on a crowded row that must never be cut — the chat header's
//! "Claude Code", which ellipsized came out as "Clau… Code" — and that is
//! not worth the row's minimum either: four tabs, the name, and the gauges
//! asked for 360px of a column whose floor is 320 (`chat_column`), and
//! the chat clips what does not fit rather than widening, so the Plan gauge
//! went off the edge. Given less than its child's minimum, it draws
//! nothing; given that much or more, the child is drawn whole.
//!
//! A widget of its own, like `chat_column::ChatColumn` and for the same
//! reason: GTK asks a widget to measure itself only while it has no layout
//! manager.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Shy;

    #[glib::object_subclass]
    impl ObjectSubclass for Shy {
        const NAME: &'static str = "TasteShy";
        type Type = super::Shy;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Shy {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Shy {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.obj().first_child() else {
                return (0, 0, -1, -1);
            };
            let (min, natural, _, _) = child.measure(orientation, for_size);
            match orientation {
                // Nothing at least; the whole child, when there is room.
                gtk::Orientation::Horizontal => (0, natural, -1, -1),
                _ => (min, natural, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.obj().first_child() else {
                return;
            };
            let (min, _, _, _) = child.measure(gtk::Orientation::Horizontal, height);
            let fits = width >= min;
            child.set_child_visible(fits);
            if fits {
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    pub struct Shy(ObjectSubclass<imp::Shy>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Shy {
    pub fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let shy: Self = glib::Object::new();
        child.as_ref().set_parent(&shy);
        shy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Narrower than its child, it asks for nothing and hides the child;
    /// as wide, it shows it whole.
    #[test]
    fn a_shy_child_is_whole_or_hidden() {
        crate::gtk_test::on_gtk_thread("a_shy_child_is_whole_or_hidden", || {
            let label = gtk::Label::new(Some("Claude Code"));
            let shy = Shy::new(&label);
            let (min, natural, _, _) = shy.measure(gtk::Orientation::Horizontal, -1);
            let (label_min, _, _, _) = label.measure(gtk::Orientation::Horizontal, -1);
            assert_eq!(min, 0);
            assert!(natural >= label_min);
            shy.size_allocate(&gtk::Allocation::new(0, 0, label_min - 1, 20), -1);
            assert!(!label.is_child_visible());
            shy.size_allocate(&gtk::Allocation::new(0, 0, label_min, 20), -1);
            assert!(label.is_child_visible());
        });
    }
}
