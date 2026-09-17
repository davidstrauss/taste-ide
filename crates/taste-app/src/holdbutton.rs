//! Press and hold to confirm: the button for what cannot be undone.
//!
//! A destructive action used to ask twice in two different ways — an
//! inline "Delete?" with a second button on the row, and a panel at the
//! foot of the console that enumerated before it offered. Both are gone
//! (David, 2026-09-16: "The user will confirm by pressing and holding the
//! button. If they release early, throw up a toast saying that
//! confirmation requires holding for X seconds. While held, turn the icon
//! into a completion circle. This circle fills in, radially, starting from
//! the top and going clockwise. At completion, it's confirmed").
//!
//! So: one button, whose press starts a clock. While it is down the icon
//! gives way to a ring that fills clockwise from twelve; when the ring
//! closes, the action fires, once. A release before that fires nothing and
//! says why in a toast, so a tap on a destructive button is a lesson, not
//! a loss. The hold is [`HOLD`] long — long enough that no click is a
//! hold, short enough that a meant one does not feel like a wait.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

/// How long the button must be held. Two seconds: the toast says the
/// number, so it is one a person can count.
pub const HOLD: Duration = Duration::from_secs(2);

/// What the toast says when a press ends early.
pub fn early_release_note() -> String {
    format!("Hold for {} seconds to confirm", HOLD.as_secs())
}

/// The ring's diameter, matched to the icon it replaces so the button does
/// not change size under the finger.
const RING_SIZE: i32 = 16;

pub struct HoldButton {
    pub widget: gtk::Button,
    faces: gtk::Stack,
    ring: gtk::DrawingArea,
    progress: Cell<f64>,
    started: Cell<Option<Instant>>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    /// Set by a completed hold, read by the button's own `clicked` on the
    /// release that follows, so that release is not also an early one.
    confirmed: Cell<bool>,
    on_confirm: RefCell<Option<Rc<dyn Fn()>>>,
    on_early: RefCell<Option<Rc<dyn Fn(String)>>>,
}

impl HoldButton {
    pub fn new(icon: &str, tooltip: &str) -> Rc<Self> {
        let image = gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(RING_SIZE)
            .build();
        let ring = gtk::DrawingArea::builder()
            .content_width(RING_SIZE)
            .content_height(RING_SIZE)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["hold-ring"])
            .build();
        let faces = gtk::Stack::builder()
            .hhomogeneous(true)
            .vhomogeneous(true)
            .transition_type(gtk::StackTransitionType::None)
            .build();
        faces.add_named(&image, Some("icon"));
        faces.add_named(&ring, Some("ring"));
        faces.set_visible_child_name("icon");
        let widget = gtk::Button::builder()
            .child(&faces)
            .tooltip_text(tooltip)
            .css_classes(["hold-button"])
            .build();
        let this = Rc::new(Self {
            widget,
            faces,
            ring,
            progress: Cell::new(0.0),
            started: Cell::new(None),
            tick: RefCell::new(None),
            confirmed: Cell::new(false),
            on_confirm: RefCell::new(None),
            on_early: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&this);
            this.ring.set_draw_func(move |area, cr, width, height| {
                let Some(this) = weak.upgrade() else { return };
                draw_ring(area, cr, width, height, this.progress.get());
            });
        }
        // The press and the release, ahead of the button's own gesture:
        // the button still draws itself pressed, and its `clicked` still
        // fires on release, which is where an early release is told.
        {
            let gesture = gtk::GestureClick::new();
            gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(&this);
            gesture.connect_pressed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.begin();
                }
            });
            let weak = Rc::downgrade(&this);
            gesture.connect_released(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.end();
                }
            });
            let weak = Rc::downgrade(&this);
            gesture.connect_cancel(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.end();
                }
            });
            this.widget.add_controller(gesture);
        }
        {
            let weak = Rc::downgrade(&this);
            this.widget.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                // A keyboard activation, or the release of an unfinished
                // hold: both are a click, and a click is not a confirmation.
                if this.confirmed.replace(false) {
                    return;
                }
                if this.started.get().is_none() {
                    this.say_early();
                }
            });
        }
        this
    }

    /// What a completed hold does.
    pub fn set_on_confirm(&self, hook: impl Fn() + 'static) {
        *self.on_confirm.borrow_mut() = Some(Rc::new(hook));
    }

    /// Where the early-release note goes: the home's toast.
    pub fn set_on_early_release(&self, hook: impl Fn(String) + 'static) {
        *self.on_early.borrow_mut() = Some(Rc::new(hook));
    }

    fn begin(self: &Rc<Self>) {
        if !self.widget.is_sensitive() || self.started.get().is_some() {
            return;
        }
        self.started.set(Some(Instant::now()));
        self.progress.set(0.0);
        self.faces.set_visible_child_name("ring");
        self.ring.queue_draw();
        let weak = Rc::downgrade(self);
        let id = self.ring.add_tick_callback(move |_, _| {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(started) = this.started.get() else {
                return glib::ControlFlow::Break;
            };
            let progress = (started.elapsed().as_secs_f64() / HOLD.as_secs_f64()).min(1.0);
            this.progress.set(progress);
            this.ring.queue_draw();
            if progress >= 1.0 {
                this.complete();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        *self.tick.borrow_mut() = Some(id);
    }

    /// The ring closed: fire once, and show the icon again.
    fn complete(self: &Rc<Self>) {
        self.started.set(None);
        self.tick.borrow_mut().take();
        self.confirmed.set(true);
        self.faces.set_visible_child_name("icon");
        if let Some(hook) = self.on_confirm.borrow().clone() {
            hook();
        }
    }

    /// The finger lifted, or the gesture was cancelled, before the ring
    /// closed: nothing fires, and the button says how long a hold is.
    fn end(self: &Rc<Self>) {
        let Some(_started) = self.started.take() else {
            return;
        };
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
        self.progress.set(0.0);
        self.faces.set_visible_child_name("icon");
        // The button's `clicked` follows this release and says the note;
        // a cancel has no click, so say it here.
        self.say_early();
        self.confirmed.set(true);
    }

    fn say_early(&self) {
        if let Some(hook) = self.on_early.borrow().clone() {
            hook(early_release_note());
        }
    }

    /// TASTE_PROBE_CHECK only: the ring at `progress` of the way round,
    /// which a screenshot cannot otherwise catch — a hold is a gesture.
    #[doc(hidden)]
    pub fn pose_for_probe(&self, progress: f64) {
        self.progress.set(progress.clamp(0.0, 1.0));
        self.faces.set_visible_child_name("ring");
        self.ring.queue_draw();
    }
}

/// The completion circle: a faint full ring for the track, and the filled
/// sector from twelve o'clock clockwise to `progress` of the way round, in
/// the widget's own foreground colour so it follows the theme and the
/// button's destructive tint.
fn draw_ring(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    progress: f64,
) {
    let color = area.color();
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let radius = (cx.min(cy) - 1.0).max(1.0);
    let top = -std::f64::consts::FRAC_PI_2;
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()) * 0.25,
    );
    cr.set_line_width(1.5);
    cr.arc(cx, cy, radius - 0.75, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
    if progress <= 0.0 {
        return;
    }
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()),
    );
    cr.move_to(cx, cy);
    cr.arc(
        cx,
        cy,
        radius,
        top,
        top + std::f64::consts::TAU * progress.min(1.0),
    );
    cr.close_path();
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_note_names_the_seconds() {
        assert_eq!(super::early_release_note(), "Hold for 2 seconds to confirm");
    }
}
