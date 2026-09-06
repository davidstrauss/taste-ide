//! What the chat column is wrapped in where it meets the rest of the
//! layout: a one-child widget whose width is **its own, never its
//! content's**.
//!
//! David, 2026-09-06: "The chat should probably not be sized using a
//! width computed from the content. That content will change all the
//! time." That is the whole contract. The column answers "how wide do you
//! need to be?" with two constants — [`MIN_WIDTH`], the floor below which
//! the composer's row of buttons stops fitting, and [`NATURAL_WIDTH`], the
//! width a chat opens at — whatever is inside it. A transcript, a
//! permission card, a settings shade with a long model name, a pasted
//! path: none of them can widen the pane, move the paned, or raise the
//! responsive ladder's thresholds (which are the sum of the panes'
//! minimums — see `window.rs`, "responsive ladder"). Content that needs
//! more than the column has is allocated its minimum and **clipped** at
//! the column's edge; we cope with a chat that renders badly when narrow,
//! and never with a chat that takes width from the editor.
//!
//! Two GTK behaviours made this necessary, both measured before this
//! widget existed:
//!
//! - A widget may answer the width question differently when it arrives
//!   with a height attached, and a wrapping `GtkLabel` answers literally:
//!   *to fit this sentence in one line's height, give me the width of the
//!   whole sentence.* `GtkPaned` measures its children for the height it
//!   is about to allocate, so at the consolidated rung — where the chat is
//!   a tab and therefore short — the permission card's prose asked for
//!   731px, the centre inherited it, and the outer paned allocated 1128px
//!   inside a 945px window.
//! - A minimum is a sum of whatever refuses to fold: a `GtkRevealer`
//!   measures its child whether or not it is revealed, a `GtkStack` its
//!   every page, a `GtkDropDown` its selected item unellipsized. Each new
//!   card is a new way for the pane's minimum to grow while the window
//!   sits still — and the ladder re-reads that minimum once a second, so
//!   the window flipped to its narrow rungs at widths that had nothing to
//!   do with the window.
//!
//! Heights are left to the child: they are measured for the width the
//! child will actually get, which is the larger of the column's width and
//! the child's own minimum, so a column that clips sideways never clips
//! its composer at the bottom.
//!
//! It is a widget of its own rather than a property on the box because GTK
//! calls a widget's `measure` only while it has no layout manager, and
//! `GtkBox`, `AdwBin` and friends all have one; gtk-rs cannot subclass
//! `GtkBoxLayout` either. A `GtkBox` subclass overriding `measure`
//! compiles, runs, and is never called — measured, before this.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// The chat's floor, in px: what the composer's action row (attach, mic,
/// meter, send) needs to stay whole. Below it the pane is not a chat.
pub const MIN_WIDTH: i32 = 320;

/// The width a chat opens at, and what it asks for when a window is sized
/// from its panes' natural widths. Not a maximum: the divider is the
/// user's.
pub const NATURAL_WIDTH: i32 = 420;

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
        fn constructed(&self) {
            self.parent_constructed();
            // Content wider than the column is cut at the column's edge,
            // not drawn over the pane beside it.
            self.obj().set_overflow(gtk::Overflow::Hidden);
        }

        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    /// The width the child is laid out at when the column is `width`
    /// wide: the column's, unless the child cannot fold that far.
    fn child_width(child: &gtk::Widget, width: i32) -> i32 {
        let (child_min, _, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        width.max(child_min)
    }

    impl WidgetImpl for ChatColumn {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            // Heights follow widths (prose wraps); widths follow nothing.
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                // The two numbers are the column's own. The child is not
                // asked, so nothing in it can move them.
                gtk::Orientation::Horizontal => (MIN_WIDTH, NATURAL_WIDTH, -1, -1),
                gtk::Orientation::Vertical => {
                    let Some(child) = self.obj().first_child() else {
                        return (0, 0, -1, -1);
                    };
                    // For the width the child will be given, which is the
                    // column's unless the child's floor is higher.
                    let width = if for_size < 0 {
                        -1
                    } else {
                        child_width(&child, for_size)
                    };
                    child.measure(orientation, width)
                }
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                // Never below the child's minimum — GTK warns and clips
                // arbitrarily below it. At least the column's width, and
                // the overflow set in `constructed` cuts the rest.
                child.allocate(child_width(&child, width), height, baseline, None);
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
                "Claude Code wants to run `cargo test -p taste-app filetree` in the \
                 devcontainer; allow this command for the rest of the conversation?",
            )
            .wrap(true)
            .build()
    }

    fn boxed_prose() -> gtk::Box {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&prose());
        column
    }

    /// The GTK behaviour underneath this widget, and the numbers that must
    /// not move — all in one test, because GTK initializes on one thread
    /// and a second test would silently skip itself rather than fail.
    /// Needs a display; skips without one.
    #[test]
    fn the_column_is_as_wide_as_it_says_whatever_is_in_it() {
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
             {control_any_height}); revisit whether this widget is still needed"
        );

        // The column: the same two numbers for any height, and for any
        // content — prose, a child with a 900px floor, or nothing at all.
        let wide = gtk::Box::new(gtk::Orientation::Vertical, 0);
        wide.set_width_request(900);
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for (what, child) in [("prose", boxed_prose()), ("a 900px child", wide), ("nothing", empty)]
        {
            let column = ChatColumn::new(&child);
            for for_size in [-1, 24, 400] {
                let (min, natural, _, _) = column.measure(gtk::Orientation::Horizontal, for_size);
                assert_eq!(min, MIN_WIDTH, "minimum, holding {what}, for a height of {for_size}");
                assert_eq!(
                    natural, NATURAL_WIDTH,
                    "natural, holding {what}, for a height of {for_size}"
                );
            }
        }

        // Heights are still the child's: measured at the width the child
        // will get, so a wrapping line asked about a narrow column reports
        // the taller answer rather than the one-line one.
        let column = ChatColumn::new(&boxed_prose());
        let (tall, _, _, _) = column.measure(gtk::Orientation::Vertical, MIN_WIDTH);
        let (short, _, _, _) = column.measure(gtk::Orientation::Vertical, 2000);
        assert!(tall > short, "a narrow column should need more height ({tall} vs {short})");
    }
}
