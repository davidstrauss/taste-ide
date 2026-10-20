//! **A step's piece of the timeline, drawn.**
//!
//! The rail beside a step — its line up to the step above, its dot, its
//! line down to the step below — was eleven widgets a row: an overlay, the
//! empty box under it, the line in three boxes, the dot's slot, the dot,
//! and a spinner that was nearly always hidden. Each was styled, measured,
//! and allocated every time a row came on screen, which is most of what
//! scrolling onto rows not seen yet cost. This is one widget that draws the
//! line and the dot in its `snapshot`, with a spinner only for a step that
//! is running.
//!
//! The dot sits on the step's first line of text (`textline`), measured in
//! the snapshot, which runs after every allocation in the frame: on the
//! frame the row is first drawn, not one or twenty frames later.
//!
//! Its colours are the stylesheet's: the line is the text colour at a fifth,
//! as `.rail-line` was, and the dot is the rail's own `color`, which the
//! tone classes set (`rail.ok`, `rail.fail`, `rail.wait`, `rail.live` in
//! main.rs).

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

use super::{RAIL_HOLE_PAD, RAIL_LINE, RAIL_SPINNER, RAIL_WIDTH};

/// What a dot says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    /// A step of the answer: the text colour, set back.
    #[default]
    Plain,
    /// An aside — a note, a thought, the plan: hollow.
    Hollow,
    /// How a call went, in the rail's own colour: `ok`, `fail`, `wait`,
    /// `live`.
    Tone(&'static str),
    /// A call its turn ended under without saying how it went: hollow, and
    /// neither green nor red.
    Cut,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Rail {
        pub top: Cell<bool>,
        pub bottom: Cell<bool>,
        /// The air above the step's content, which the line crosses.
        pub gap: Cell<i32>,
        pub mark: Cell<Mark>,
        /// The step's content, whose first line the dot sits on.
        pub content: glib::WeakRef<gtk::Widget>,
        /// The first line's centre inside the content, as last measured.
        pub mid: Cell<Option<i32>>,
        pub spinner: RefCell<Option<gtk::Spinner>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Rail {
        const NAME: &'static str = "TasteRail";
        type Type = super::Rail;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("rail");
        }
    }

    impl ObjectImpl for Rail {
        fn dispose(&self) {
            if let Some(spinner) = self.spinner.take() {
                spinner.unparent();
            }
        }
    }

    impl Rail {
        /// The dot's centre, from the rail's top.
        pub fn centre(&self) -> i32 {
            self.gap.get() + self.mid.get().unwrap_or(RAIL_LINE / 2)
        }
    }

    impl WidgetImpl for Rail {
        /// As wide as the rail; no height of its own — the content decides
        /// the row's, and the rail is drawn over whatever it comes to.
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (RAIL_WIDTH, RAIL_WIDTH, -1, -1),
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            if let Some(spinner) = self.spinner.borrow().as_ref() {
                let x = (width - RAIL_SPINNER) / 2;
                let y = self.centre() - RAIL_SPINNER / 2;
                spinner.allocate(
                    RAIL_SPINNER,
                    RAIL_SPINNER,
                    -1,
                    Some(
                        gsk::Transform::new().translate(&graphene::Point::new(x as f32, y as f32)),
                    ),
                );
            }
            // The content may have moved its first line with it.
            self.obj().queue_draw();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            // Measured now: every allocation in the frame is done.
            let mid = self
                .content
                .upgrade()
                .and_then(|content| crate::textline::first_line_mid(&content));
            if mid.is_some() && mid != self.mid.get() {
                self.mid.set(mid);
                if self.spinner.borrow().is_some() {
                    widget.queue_allocate();
                }
            }
            let width = widget.width();
            let height = widget.height() as f32;
            let centre = self.centre() as f32;
            let running = self.spinner.borrow().is_some();
            let hole = if running {
                (RAIL_SPINNER + 2 * RAIL_HOLE_PAD) as f32
            } else {
                0.0
            };
            let text = widget
                .parent()
                .map(|parent| parent.color())
                .unwrap_or_else(|| widget.color());
            let line =
                gtk::gdk::RGBA::new(text.red(), text.green(), text.blue(), text.alpha() * 0.22);
            let x = ((width - 1) / 2) as f32;
            if self.top.get() {
                let end = (centre - hole / 2.0).max(0.0);
                snapshot.append_color(&line, &graphene::Rect::new(x, 0.0, 1.0, end));
            }
            if self.bottom.get() {
                let start = centre + hole / 2.0;
                snapshot.append_color(
                    &line,
                    &graphene::Rect::new(x, start, 1.0, (height - start).max(0.0)),
                );
            }
            if let Some(spinner) = self.spinner.borrow().as_ref() {
                widget.snapshot_child(spinner, snapshot);
                return;
            }
            // Seven across, centred on the line's column and on the text's
            // first line, as the dot widget was.
            let own = widget.color();
            let with = |alpha: f32| {
                gtk::gdk::RGBA::new(own.red(), own.green(), own.blue(), own.alpha() * alpha)
            };
            let dot = graphene::Rect::new(((width - 7) / 2) as f32, centre - 4.0, 7.0, 7.0);
            let round = gsk::RoundedRect::from_rect(dot, 3.5);
            match self.mark.get() {
                Mark::Hollow | Mark::Cut => {
                    let ring = with(if self.mark.get() == Mark::Cut {
                        0.45
                    } else {
                        0.35
                    });
                    snapshot.append_border(&round, &[2.0; 4], &[ring; 4]);
                }
                mark => {
                    let fill = if mark == Mark::Plain { with(0.45) } else { own };
                    snapshot.push_rounded_clip(&round);
                    snapshot.append_color(&fill, &dot);
                    snapshot.pop();
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct Rail(ObjectSubclass<imp::Rail>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Rail {
    /// The rail of a step whose content is `content`, under a gap of `gap`.
    pub fn new(content: &impl IsA<gtk::Widget>, gap: i32) -> Self {
        let rail: Self = glib::Object::new();
        rail.imp().content.set(Some(content.upcast_ref()));
        rail.imp().gap.set(gap);
        rail
    }

    /// Whether the line runs up from the dot, and down from it.
    pub fn set_lines(&self, top: bool, bottom: bool) {
        let imp = self.imp();
        if imp.top.replace(top) != top || imp.bottom.replace(bottom) != bottom {
            self.queue_draw();
        }
    }

    /// What the dot says.
    pub fn set_mark(&self, mark: Mark) {
        let was = self.imp().mark.replace(mark);
        if let Mark::Tone(tone) = was {
            self.remove_css_class(tone);
        }
        if let Mark::Tone(tone) = mark {
            self.add_css_class(tone);
        }
        self.queue_draw();
    }

    /// The spinner in the dot's place while a call runs, and the line
    /// broken around it: the line showed through the spinner's ring
    /// (David, 2026-09-16: "The vertical timeline line should not be
    /// visible inside spinners").
    pub fn set_running(&self, running: bool) {
        let imp = self.imp();
        if running == imp.spinner.borrow().is_some() {
            return;
        }
        if running {
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.set_parent(self);
            *imp.spinner.borrow_mut() = Some(spinner);
        } else if let Some(spinner) = imp.spinner.take() {
            spinner.unparent();
        }
        self.queue_allocate();
        self.queue_draw();
    }
}
