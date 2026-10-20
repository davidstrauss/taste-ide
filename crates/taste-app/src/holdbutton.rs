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
//!
//! A button can also do something lighter on a click and the heavier
//! thing on a hold ([`HoldButton::set_on_click`]): Pull fast-forwards on a
//! click and rebases on a hold (David, 2026-10-15), and says when a click
//! could not do what was needed, from its own result, since only the
//! fetch the click makes can tell.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

/// How long the button must be held. Two seconds: the toast says the
/// number, so it is one a person can count.
pub const HOLD: Duration = Duration::from_secs(2);

/// The hold for what is worst to get wrong: replacing the default branch's
/// history on the remote, which everyone else's work stands on (David,
/// 2026-10-19: "A force push, if necessary, for the default branch requires
/// holding 10 seconds"). A button holds this long by [`HoldButton::set_hold`].
pub const LONG_HOLD: Duration = Duration::from_secs(10);

/// What the toast says when a press ends early.
pub fn early_release_note(hold: Duration) -> String {
    format!("Hold for {} seconds to confirm", hold.as_secs())
}

/// The ring's diameter, matched to the icon it replaces so the button does
/// not change size under the finger.
pub(crate) const RING_SIZE: i32 = 16;

pub struct HoldButton {
    pub widget: gtk::Button,
    faces: gtk::Stack,
    /// The face the button wears at rest, which the ring stands in for.
    idle: &'static str,
    ring: gtk::DrawingArea,
    progress: Cell<f64>,
    started: Cell<Option<Instant>>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    /// Set by a completed hold, read by the button's own `clicked` on the
    /// release that follows, so that release is not also an early one.
    confirmed: Cell<bool>,
    on_confirm: RefCell<Option<Rc<dyn Fn()>>>,
    on_early: RefCell<Option<Rc<dyn Fn(String)>>>,
    /// What a click does, when the button does something on one
    /// ([`Self::set_on_click`]); otherwise a click is an early release.
    on_click: RefCell<Option<Rc<dyn Fn()>>>,
    /// How long this button is held for: [`HOLD`] unless set otherwise.
    hold: Cell<Duration>,
}

impl HoldButton {
    pub fn new(icon: &str, tooltip: &str) -> Rc<Self> {
        let image = gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(RING_SIZE)
            .build();
        let faces = gtk::Stack::builder()
            .hhomogeneous(true)
            .vhomogeneous(true)
            .transition_type(gtk::StackTransitionType::None)
            .build();
        faces.add_named(&image, Some("icon"));
        faces.set_visible_child_name("icon");
        let widget = gtk::Button::builder()
            .child(&faces)
            .tooltip_text(tooltip)
            .css_classes(["hold-button"])
            .build();
        Self::around(&widget, &faces, "icon")
    }

    /// The hold, on a button that already has a face: `faces` is the
    /// stack that is its child, `idle` the page it wears at rest, and the
    /// ring is added beside it. A press while another page is up — a
    /// spinner — starts nothing.
    pub fn around(widget: &gtk::Button, faces: &gtk::Stack, idle: &'static str) -> Rc<Self> {
        let ring = gtk::DrawingArea::builder()
            .content_width(RING_SIZE)
            .content_height(RING_SIZE)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["hold-ring"])
            .build();
        faces.add_named(&ring, Some("ring"));
        let this = Rc::new(Self {
            widget: widget.clone(),
            faces: faces.clone(),
            idle,
            ring,
            progress: Cell::new(0.0),
            started: Cell::new(None),
            tick: RefCell::new(None),
            confirmed: Cell::new(false),
            on_confirm: RefCell::new(None),
            on_early: RefCell::new(None),
            on_click: RefCell::new(None),
            hold: Cell::new(HOLD),
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
                    this.end(false);
                }
            });
            let weak = Rc::downgrade(&this);
            gesture.connect_cancel(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.end(true);
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
                if this.started.get().is_none() && this.at_rest() {
                    this.clicked();
                }
            });
        }
        this
    }

    /// How long a hold of this button takes from now on: what it confirms
    /// can change with the state it is in — a push that replaces the
    /// default branch's history holds for [`LONG_HOLD`]. A hold already
    /// under way keeps the length it began with.
    pub fn set_hold(&self, hold: Duration) {
        self.hold.set(hold);
    }

    /// How long a hold of this button takes.
    pub fn hold(&self) -> Duration {
        self.hold.get()
    }

    /// What a completed hold does.
    pub fn set_on_confirm(&self, hook: impl Fn() + 'static) {
        *self.on_confirm.borrow_mut() = Some(Rc::new(hook));
    }

    /// What a click does — a press released before the ring closes, or a
    /// keyboard activation — in place of the early-release note.
    pub fn set_on_click(&self, hook: impl Fn() + 'static) {
        *self.on_click.borrow_mut() = Some(Rc::new(hook));
    }

    /// Where the early-release note goes: the home's toast.
    pub fn set_on_early_release(&self, hook: impl Fn(String) + 'static) {
        *self.on_early.borrow_mut() = Some(Rc::new(hook));
    }

    /// Wearing the face it rests in: not a spinner, not mid-hold.
    fn at_rest(&self) -> bool {
        self.faces.visible_child_name().as_deref() == Some(self.idle)
    }

    fn begin(self: &Rc<Self>) {
        if !self.widget.is_sensitive() || self.started.get().is_some() || !self.at_rest() {
            return;
        }
        self.started.set(Some(Instant::now()));
        self.progress.set(0.0);
        let hold = self.hold.get();
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
            let progress = (started.elapsed().as_secs_f64() / hold.as_secs_f64()).min(1.0);
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
        self.faces.set_visible_child_name(self.idle);
        if let Some(hook) = self.on_confirm.borrow().clone() {
            hook();
        }
    }

    /// The finger lifted, or the gesture was cancelled, before the ring
    /// closed: the hold's action does not fire. A release is a click; a
    /// cancel is not, and on a button with a click of its own does nothing,
    /// while on one without it says how long a hold is, as a release does.
    fn end(self: &Rc<Self>, cancelled: bool) {
        let Some(_started) = self.started.take() else {
            return;
        };
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
        self.progress.set(0.0);
        self.faces.set_visible_child_name(self.idle);
        // The button's own `clicked` follows a release, and is swallowed
        // (`confirmed`): this is where the click is answered.
        if !cancelled || self.on_click.borrow().is_none() {
            self.clicked();
        }
        self.confirmed.set(true);
    }

    /// A click: the button's own action when it has one, else the note.
    fn clicked(&self) {
        match self.on_click.borrow().clone() {
            Some(hook) => hook(),
            None => self.say_early(),
        }
    }

    fn say_early(&self) {
        if let Some(hook) = self.on_early.borrow().clone() {
            hook(early_release_note(self.hold.get()));
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
/// The ring: a faint track, and a wedge filling clockwise from twelve to
/// `progress`. Shared with the file tree's sync mark (`crate::sync_mark`),
/// so a progress ring is one thing in this app (David, 2026-10-17: "The
/// progress circle should be the same as the one for click and hold").
pub(crate) fn draw_ring(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    progress: f64,
) {
    let color = area.color();
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let radius = (cx.min(cy) - 1.0).max(1.0);
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
    cr.new_path();
    ring_wedge(cr, width, height, progress);
    let _ = cr.fill();
}

/// The wedge [`draw_ring`] fills, added to the current path: for a mark
/// drawn over the ring to be cut out of it.
pub(crate) fn ring_wedge(cr: &gtk::cairo::Context, width: i32, height: i32, progress: f64) {
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let radius = (cx.min(cy) - 1.0).max(1.0);
    let top = -std::f64::consts::FRAC_PI_2;
    cr.move_to(cx, cy);
    cr.arc(
        cx,
        cy,
        radius,
        top,
        top + std::f64::consts::TAU * progress.min(1.0),
    );
    cr.close_path();
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_note_names_the_seconds() {
        assert_eq!(
            super::early_release_note(super::HOLD),
            "Hold for 2 seconds to confirm"
        );
        assert_eq!(
            super::early_release_note(super::LONG_HOLD),
            "Hold for 10 seconds to confirm"
        );
    }
}
