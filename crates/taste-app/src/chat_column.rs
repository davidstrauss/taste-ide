//! What the chat column is wrapped in where it meets the rest of the
//! layout: a one-child widget that will not trade width for height.
//!
//! GTK lets a widget answer "how wide do you need to be?" differently when
//! the question arrives with a height attached, and a wrapping `GtkLabel`
//! answers it literally: *to fit this sentence in one line's worth of
//! height, give me the width of the whole sentence.* Every container above
//! it forwards that answer, and `GtkPaned` measures its children for the
//! height it is about to allocate — so with `shrink-*-child` false, which
//! is what keeps panes off each other's minimums, the paned takes that
//! answer as a minimum and hands it out whether or not the window has the
//! width to give.
//!
//! Measured at the consolidated rung, where the chat is a pinned tab and
//! therefore SHORT: the permission card's prose asked for 731px so it
//! could stay on one line, the centre inherited that as its minimum, and
//! the outer paned allocated 392 + 5 + 731 = 1128px inside a 945px window
//! — the centre's right edge 183px past the frame, and worse the narrower
//! the window got. The same column at full width asks for nothing of the
//! sort, because a tall pane has room to wrap; the fault shows only where
//! the pane is short, which is exactly the rung that exists for narrow
//! windows.
//!
//! The honest answer is the one this gives. Everything in the chat column
//! wraps or scrolls, and the column states its real floor itself (a 320px
//! width request), so how much height it is offered cannot change how much
//! width it needs. This drops the height from the question before passing
//! it on, and leaves everything else — natural width, both heights,
//! baselines, and the whole allocation — to the child.
//!
//! It is a widget of its own rather than a property on the box because GTK
//! calls a widget's `measure` only while it has no layout manager, and
//! `GtkBox`, `AdwBin` and friends all have one; gtk-rs cannot subclass
//! `GtkBoxLayout` either. A `GtkBox` subclass overriding `measure`
//! compiles, runs, and is never called — measured, before this.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ChatColumn;

    #[glib::object_subclass]
    impl ObjectSubclass for ChatColumn {
        const NAME: &'static str = "TasteChatColumn";
        type Type = super::ChatColumn;
        type ParentType = gtk::Widget;
        // Deliberately no layout manager: one would take the measuring
        // over, and the measuring is the whole point.
    }

    impl ObjectImpl for ChatColumn {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ChatColumn {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.obj()
                .first_child()
                .map_or(gtk::SizeRequestMode::ConstantSize, |child| {
                    child.request_mode()
                })
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.obj().first_child() else {
                return (0, 0, -1, -1);
            };
            // Width asked for a height is answered as width asked for
            // nothing. Heights are left alone: a column that lied about
            // its height would clip its own composer.
            let for_size = match orientation {
                gtk::Orientation::Horizontal => -1,
                _ => for_size,
            };
            child.measure(orientation, for_size)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    pub struct ChatColumn(ObjectSubclass<imp::ChatColumn>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ChatColumn {
    pub fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let column: Self = glib::Object::new();
        child.as_ref().set_parent(&column);
        column
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One long wrapping line, the shape the permission card's prose has.
    fn prose() -> gtk::Label {
        gtk::Label::builder()
            .label(
                "The config on disk differs from the container that is \
                 running. Applying it rebuilds the container and runs its \
                 postCreateCommand.",
            )
            .wrap(true)
            .xalign(0.0)
            .build()
    }

    fn boxed_prose() -> gtk::Box {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&prose());
        column
    }

    /// The defect this widget exists for, the GTK behaviour underneath it,
    /// and the two numbers that must not move — all in one test, because
    /// GTK initializes on one thread and a second test would silently skip
    /// itself rather than fail. Needs a display; skips without one.
    #[test]
    fn a_short_column_asks_for_no_more_width_than_a_tall_one() {
        if gtk::init().is_err() {
            println!("chat column: no display — skipped");
            return;
        }

        // The control: a plain GtkBox answers "how wide, to fit in one
        // line's height?" with the width of the whole line. This is what
        // GtkPaned asked the chat column, and why the centre pane was
        // allocated 731px in a 553px hole.
        let control = boxed_prose();
        let (control_any_height, _, _, _) = control.measure(gtk::Orientation::Horizontal, -1);
        let (control_one_line, _, _, _) = control.measure(gtk::Orientation::Horizontal, 24);
        assert!(
            control_one_line > control_any_height,
            "GtkBox stopped trading width for height ({control_one_line} vs \
             {control_any_height}); this widget may no longer be needed"
        );

        let column = ChatColumn::new(&boxed_prose());
        let (any_height, _, _, _) = column.measure(gtk::Orientation::Horizontal, -1);
        let (one_line, _, _, _) = column.measure(gtk::Orientation::Horizontal, 24);
        assert_eq!(
            one_line, any_height,
            "the chat column widened because it was short"
        );

        // ...and what must NOT move: the floor the column states for
        // itself, and the natural width, which is what decides the pane's
        // share of a window wide enough for all four panes.
        let (_, control_natural, _, _) = control.measure(gtk::Orientation::Horizontal, -1);
        let inner = boxed_prose();
        inner.set_width_request(320);
        let column = ChatColumn::new(&inner);
        let (_, natural, _, _) = column.measure(gtk::Orientation::Horizontal, -1);
        assert_eq!(natural, control_natural, "the natural width moved");
        for for_size in [-1, 24, 400] {
            let (min, _, _, _) = column.measure(gtk::Orientation::Horizontal, for_size);
            assert_eq!(min, 320, "measured for a height of {for_size}");
        }
    }
}
