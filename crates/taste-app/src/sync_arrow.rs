//! **The sync counts' arrows, drawn: ↓, ↑, and the double-stemmed ⇊.**
//!
//! The file tree's header counts where its branch stands with three
//! arrows: ↓ against its own copy on the remote, ↑ for what a push sends,
//! and ⇊ against the canonical branch, which wants a down arrow that says
//! "more than ↓". Neither ⇊ nor ⇓ is in Adwaita Sans, so either came from a
//! fallback font: heavier, and set apart from its count; and two ↓ drawn
//! together were twice as wide and read as two things (David, 2026-10-19:
//! "I want the double arrow icons to be much tighter … Or just an arrow
//! with a double stem?").
//!
//! So all three are drawn by the one function here, which is what keeps
//! them one set (David: "I would probably draw the whole set just to
//! guarantee consistency long-term"): the UI font's own ↓ is laid out and
//! measured, and each arrow is stroked into that box — the head as wide as
//! the glyph's, the apex where its apex is, one stem or two, at a weight
//! taken from the glyph's width, and turned over for ↑. Each is drawn in
//! its count's colour, so it dims with its button and takes the count's
//! warning colour.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, gsk};

/// The glyph whose box and size the arrows take.
const GLYPH: &str = "↓";

/// Which arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Arrow {
    /// ↓
    #[default]
    Down,
    /// ↑
    Up,
    /// ⇊, a ↓ with a double stem.
    DoubleDown,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SyncArrow {
        pub arrow: Cell<Arrow>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SyncArrow {
        const NAME: &'static str = "TasteSyncArrow";
        type Type = super::SyncArrow;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("syncarrow");
        }
    }

    impl ObjectImpl for SyncArrow {}

    impl WidgetImpl for SyncArrow {
        /// The ↓'s own logical box, so the arrow lines up with a label in
        /// the same font beside it, baseline and all.
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let layout = self.obj().create_pango_layout(Some(GLYPH));
            let (width, height) = layout.pixel_size();
            let baseline = layout.baseline() / gtk::pango::SCALE;
            match orientation {
                gtk::Orientation::Horizontal => (width, width, -1, -1),
                _ => (height, height, baseline, baseline),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let layout = widget.create_pango_layout(Some(GLYPH));
            let (ink, _) = layout.pixel_extents();
            // The glyph's box centred in the allocation, as a label centres
            // its line: the count beside it can be a pixel or two taller,
            // and drawn from the top the arrow sat above the ↓ next to it.
            let (width, height) = layout.pixel_size();
            let dx = (widget.width() - width) as f32 / 2.0;
            let dy = (widget.height() - height) as f32 / 2.0;
            let Some((path, weight)) = path(
                self.arrow.get(),
                ink.x() as f32 + dx,
                ink.y() as f32 + dy,
                ink.width() as f32,
                ink.height() as f32,
            ) else {
                return;
            };
            let stroke = gsk::Stroke::new(weight);
            stroke.set_line_cap(gsk::LineCap::Butt);
            stroke.set_line_join(gsk::LineJoin::Miter);
            // The count's colour, whatever state the button is in: Adwaita
            // dims a disabled button's label, not every child of it, and in
            // its own colour the arrow stayed bright beside a greyed count.
            let color = widget
                .next_sibling()
                .map(|count| count.color())
                .unwrap_or_else(|| widget.color());
            snapshot.append_stroke(&path, &stroke, &color);
        }
    }
}

glib::wrapper! {
    pub struct SyncArrow(ObjectSubclass<imp::SyncArrow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SyncArrow {
    pub fn new(arrow: Arrow) -> Self {
        let this: Self = glib::Object::new();
        this.imp().arrow.set(arrow);
        this
    }
}

/// `arrow` in the glyph's ink box at (`x`, `y`), `width` by `height`: a
/// chevron head as wide as the box with its apex at the bottom (the top,
/// for ↑), and one stem down the middle to the apex, or two that stop where
/// they meet the head's arms. The stroke weight is taken from the width — a
/// regular ↓'s stroke is about a sixth of its head — and returned with the
/// path. `None` for a box too small to draw in.
fn path(arrow: Arrow, x: f32, y: f32, width: f32, height: f32) -> Option<(gsk::Path, f32)> {
    if width < 3.0 || height < 3.0 {
        return None;
    }
    let weight = (width * 0.16).max(1.0);
    let half = weight / 2.0;
    let centre = x + width / 2.0;
    // Drawn pointing down, from `top` to the apex; ↑ is the same turned
    // over inside the box.
    let top = y;
    let apex = y + height - half;
    let flip = |py: f32| match arrow {
        Arrow::Up => y + height - (py - y),
        _ => py,
    };
    // A right-angled chevron: its arms fall as far as they reach out.
    let reach = width / 2.0 - half;
    let shoulders = apex - reach;
    let builder = gsk::PathBuilder::new();
    match arrow {
        Arrow::DoubleDown => {
            // The stems part by a little over two strokes, so the gap
            // between them reads at the size the counts are set in.
            let part = (weight * 2.4).min(width * 0.5);
            for side in [-1.0f32, 1.0] {
                let stem = centre + side * part / 2.0;
                builder.move_to(stem, flip(top));
                builder.line_to(stem, flip(apex - part / 2.0));
            }
        }
        Arrow::Down | Arrow::Up => {
            builder.move_to(centre, flip(top));
            builder.line_to(centre, flip(apex));
        }
    }
    builder.move_to(centre - reach, flip(shoulders));
    builder.line_to(centre, flip(apex));
    builder.line_to(centre + reach, flip(shoulders));
    Some((builder.to_path(), weight))
}

#[cfg(test)]
mod tests {
    use super::{path, Arrow};

    /// Every arrow fills the glyph's box the same way — the head spanning
    /// it, the point at one end and the stems reaching the other — which
    /// is what makes the three one set.
    #[test]
    fn the_three_arrows_fill_the_same_box() {
        for arrow in [Arrow::Down, Arrow::Up, Arrow::DoubleDown] {
            let (path, weight) = path(arrow, 0.0, 0.0, 8.0, 10.0).unwrap();
            let bounds = path.bounds().unwrap();
            let left = bounds.x();
            let right = bounds.x() + bounds.width();
            assert!((left - weight / 2.0).abs() < 0.01, "{arrow:?} {bounds:?}");
            assert!((right - (8.0 - weight / 2.0)).abs() < 0.01, "{arrow:?}");
            assert!(bounds.height() > 9.0 - weight, "{arrow:?} {bounds:?}");
        }
        // ↓'s point is at the bottom, ↑'s at the top.
        let down = path(Arrow::Down, 0.0, 0.0, 8.0, 10.0).unwrap().0;
        let up = path(Arrow::Up, 0.0, 0.0, 8.0, 10.0).unwrap().0;
        let down = down.bounds().unwrap();
        let up = up.bounds().unwrap();
        assert!(down.y() < 0.01 && up.y() > 0.01, "{down:?} {up:?}");
        assert!(path(Arrow::Down, 0.0, 0.0, 2.0, 10.0).is_none());
    }
}
