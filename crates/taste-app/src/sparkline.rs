//! A row's activity, drawn: sixty buckets of [`taste_core::activity`] in
//! about forty pixels.
//!
//! The environment panel's rows have a state dot, and a dot cannot say
//! whether an environment that is *up* is doing anything. This can, in the
//! only space a sidebar row has to spare, and without asking the reader to
//! parse a number that would be stale by the time they did.
//!
//! **It draws in the theme's own foreground colour at reduced alpha**
//! (`WidgetExt::color`, which resolves the CSS colour actually in force —
//! including the selected row's, which differs). That is the whole of its
//! theming: nothing here hard-codes a light or a dark, and a row that is
//! selected draws its line in the selection's foreground because that is
//! what the widget was told its colour is.
//!
//! A [`gtk::DrawingArea`] rather than a `GtkWidget` subclass, per the rule
//! in `command_completion.rs`: subclass only when gtk-rs offers no
//! closure-based adapter, and here it does. The draw closure allocates
//! nothing — it reads a fixed-size array through an `Rc` and emits cairo
//! calls — and it runs only when [`Sparkline::set_samples`] is handed
//! something different from what it is already showing.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use taste_core::activity::{self, Count, BUCKETS};

/// The drawn width, in pixels. Wide enough that sixty buckets are most of
/// a pixel each and a burst has a shape; narrow enough to leave a
/// twelve-character environment name room in a file-tree pane at its
/// minimum width.
const WIDTH: i32 = 44;

/// The drawn height. A sparkline is a texture, not a chart: tall enough to
/// tell a spike from a plateau, short enough that the row stays a row.
const HEIGHT: i32 = 14;

/// How much of the theme foreground the line and its fill take. The line
/// has to read as data beside a name set in the same colour at full
/// strength, and the fill only has to give the line a body.
const LINE_ALPHA: f64 = 0.55;
const FILL_ALPHA: f64 = 0.13;

/// One environment's activity, drawn.
pub struct Sparkline {
    pub widget: gtk::DrawingArea,
    /// What is currently on screen. Compared against on every update so a
    /// 1 Hz tick over an idle fleet queues no draws at all.
    samples: Rc<RefCell<[Count; BUCKETS]>>,
}

impl Sparkline {
    pub fn new() -> Self {
        let samples = Rc::new(RefCell::new([0; BUCKETS]));
        let widget = gtk::DrawingArea::builder()
            .content_width(WIDTH)
            .content_height(HEIGHT)
            .valign(gtk::Align::Center)
            .can_target(false)
            .build();
        let for_draw = samples.clone();
        widget.set_draw_func(move |area, cr, width, height| {
            draw(area, cr, width, height, &for_draw.borrow());
        });
        Self { widget, samples }
    }

    /// Show `next`, if it is not already what is shown.
    ///
    /// The guard is the point: this is called once a second for every row,
    /// and an environment where nothing is happening produces the same
    /// sixty zeros every time. Redrawing them would be a wakeup per row per
    /// second for a picture that did not change.
    pub fn set_samples(&self, next: &[Count; BUCKETS]) {
        if *self.samples.borrow() == *next {
            return;
        }
        *self.samples.borrow_mut() = *next;
        self.widget.queue_draw();
    }

    /// The tooltip: what the picture is, and what it is of.
    pub fn describe(samples: &[Count]) -> String {
        if activity::is_silent(samples) {
            return "Nothing has happened here in the last five minutes".to_string();
        }
        let total: u32 = samples.iter().map(|count| u32::from(*count)).sum();
        format!("Activity over the last five minutes — {total} events")
    }
}

/// The whole render. Silence draws nothing at all: a flat line along the
/// baseline would claim a measurement of zero, and an environment that has
/// only just appeared has no history rather than a history of nothing.
fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    samples: &[Count; BUCKETS],
) {
    if width <= 1 || height <= 2 || activity::is_silent(samples) {
        return;
    }
    let colour = area.color();
    let scale = f64::from(activity::scale(samples));
    // Half a pixel in from each edge: a 1px stroke centred on an integer
    // coordinate straddles two rows of pixels and renders grey.
    let top = 1.5;
    let bottom = f64::from(height) - 1.5;
    let span = bottom - top;
    let step = f64::from(width - 1) / (BUCKETS - 1) as f64;
    let point = |index: usize| {
        let value = f64::from(samples[index]) / scale;
        (index as f64 * step, bottom - value.min(1.0) * span)
    };

    // The body first, so the line sits on top of its own fill.
    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        FILL_ALPHA * f64::from(colour.alpha()),
    );
    cr.move_to(0.0, bottom);
    for index in 0..BUCKETS {
        let (x, y) = point(index);
        cr.line_to(x, y);
    }
    cr.line_to(f64::from(width - 1), bottom);
    cr.close_path();
    let _ = cr.fill();

    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        LINE_ALPHA * f64::from(colour.alpha()),
    );
    cr.set_line_width(1.0);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    for index in 0..BUCKETS {
        let (x, y) = point(index);
        if index == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    let _ = cr.stroke();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tooltip is the only part of this that is testable without a
    /// display, and it is the part that must not lie: silence says
    /// silence, and a count is the count.
    #[test]
    fn the_description_distinguishes_silence_from_a_small_number() {
        assert_eq!(
            Sparkline::describe(&[0; BUCKETS]),
            "Nothing has happened here in the last five minutes"
        );
        let mut samples = [0; BUCKETS];
        samples[3] = 2;
        samples[9] = 40;
        assert_eq!(
            Sparkline::describe(&samples),
            "Activity over the last five minutes — 42 events"
        );
    }
}
