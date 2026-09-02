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
//! nothing on the heap — it reads a fixed-size array through an `Rc`,
//! shapes it into two more on the stack, and emits cairo calls — and it
//! runs only when [`Sparkline::set_samples`] is handed something different
//! from what it is already showing.

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
const FILL_ALPHA: f64 = 0.18;

/// How far one bucket's count is allowed to reach into its neighbours
/// before anything is drawn, in buckets. See [`shaped`].
///
/// Two, chosen at size rather than in the abstract: at one, a lone event
/// still drew as a two-pixel triangle eight pixels tall, and a row of
/// those is the picket fence this was supposed to stop being. Two puts a
/// mark's width and its height in the same order, which is what makes it
/// read as a hump instead of as a spike. It is also still small against
/// what the widget can resolve — ten seconds either side of a bucket is a
/// pixel and a half of a forty-four pixel window.
const SPREAD: usize = 2;

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

/// The counts, shaped into what is actually drawn.
///
/// Sixty buckets across forty-four pixels is under a pixel each, so a
/// bucket with nobody either side of it is a *hairline* — and a dozen
/// hairlines scattered along a sidebar row read as dirt on the glass
/// rather than as a measurement. That was the whole of the low-activity
/// complaint: not that quiet drew too much, but that what it drew had no
/// width to be recognised by.
///
/// So two passes, in this order:
///
///  1. **Widen.** Every bucket takes the largest count within [`SPREAD`]
///     of it. A peak is its own maximum and keeps its height exactly; a
///     lone event gains the width it needs to be seen.
///  2. **Round.** A normalised 1-2-1 pass over the widened series. A
///     plateau is unchanged (its neighbours equal it), and the widened
///     point becomes a hump that rises and falls over five buckets
///     instead of a spike with vertical sides.
///
/// Neither pass invents activity: the drawing's own resolution is already
/// coarser than a bucket, so spreading a count across the pixel either
/// side of it is the *rendering* being honest about what it can show —
/// the same reason a 1px stroke is antialiased rather than snapped. The
/// number of events is the tooltip's job, and it still reports the raw
/// counts.
///
/// Silence stays silence: an all-zero series shapes to all zeros, and
/// [`draw`] has already refused it.
fn shaped(samples: &[Count; BUCKETS]) -> [f64; BUCKETS] {
    let mut wide = [0 as Count; BUCKETS];
    for (index, slot) in wide.iter_mut().enumerate() {
        let low = index.saturating_sub(SPREAD);
        let high = (index + SPREAD).min(BUCKETS - 1);
        *slot = samples[low..=high].iter().copied().max().unwrap_or(0);
    }
    let mut out = [0.0; BUCKETS];
    for (index, slot) in out.iter_mut().enumerate() {
        // Clamped at both ends rather than zero-padded: a series that is
        // busy right up to the edge of its window must not droop there,
        // which would read as activity tailing off when it did not.
        let previous = wide[index.saturating_sub(1)];
        let next = wide[(index + 1).min(BUCKETS - 1)];
        *slot = (f64::from(previous) + 2.0 * f64::from(wide[index]) + f64::from(next)) / 4.0;
    }
    out
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
    // The scale is taken from the RAW counts, not the shaped ones: the
    // shaping never raises a peak, so both agree on a busy series — and
    // taking it from the raw ones keeps the floor ([`activity::MIN_SCALE`])
    // meaning what it has always meant.
    let scale = f64::from(activity::scale(samples));
    let shape = shaped(samples);
    // Half a pixel in from each edge: a 1px stroke centred on an integer
    // coordinate straddles two rows of pixels and renders grey.
    let top = 1.5;
    let bottom = f64::from(height) - 1.5;
    let span = bottom - top;
    let step = f64::from(width - 1) / (BUCKETS - 1) as f64;
    let point = |index: usize| {
        let value = shape[index] / scale;
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
    // Only where something happened. A polyline through the zeros draws a
    // hard rule along the baseline at full line alpha, and at this size
    // that reads as a divider someone put there on purpose rather than as
    // an absence of activity — the hero shot is where that showed up. So
    // the pen lifts over a run of empty buckets: a busy series is still one
    // continuous line, a sparse one is the marks it earned, and a bucket
    // that is zero between two that are not still gets its dip.
    //
    // Asked of the SHAPED series, which is what the pen is tracing. One
    // bucket of slack either side of it, as before, so every hump lands on
    // the baseline instead of stopping in mid-air — and, because the
    // shaping already carries a count two buckets out, a lone event now
    // has a rise and a fall rather than a tick.
    let mut down = false;
    for index in 0..BUCKETS {
        let live = shape[index] > 0.0
            || index.checked_sub(1).is_some_and(|prev| shape[prev] > 0.0)
            || shape.get(index + 1).is_some_and(|next| *next > 0.0);
        if !live {
            down = false;
            continue;
        }
        let (x, y) = point(index);
        if down {
            cr.line_to(x, y);
        } else {
            cr.move_to(x, y);
            down = true;
        }
    }
    let _ = cr.stroke();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence shapes to silence. The one property the whole module hangs
    /// on: if the shaping ever put a non-zero anywhere in an empty series,
    /// [`draw`]'s `is_silent` guard would be the only thing standing
    /// between an idle row and a bright rule along its baseline.
    #[test]
    fn shaping_an_empty_window_leaves_it_empty() {
        assert!(shaped(&[0; BUCKETS]).iter().all(|value| *value == 0.0));
    }

    /// A lone event becomes something with width, at the height it
    /// actually had — the whole point of the shaping. Widening cannot
    /// invent a taller peak than the series contains, or an idle
    /// environment that logged one line would draw taller than the build
    /// beside it.
    #[test]
    fn one_event_gains_width_without_gaining_height() {
        let mut samples = [0; BUCKETS];
        samples[30] = 4;
        let shape = shaped(&samples);
        assert_eq!(shape[30], 4.0, "the peak is the count, untouched");
        assert_eq!((shape[29], shape[31]), (4.0, 4.0), "held across the spread");
        assert_eq!((shape[28], shape[32]), (3.0, 3.0), "it falls away");
        assert_eq!((shape[27], shape[33]), (1.0, 1.0), "…and lands");
        assert_eq!(
            (shape[26], shape[34]),
            (0.0, 0.0),
            "and reaches no further: seven buckets is the whole mark"
        );
        assert!(
            shape.iter().all(|value| *value <= 4.0),
            "nothing anywhere outgrows the count it came from"
        );
    }

    /// A plateau is left alone. Busy series looked right before this
    /// change and have to look identical after it, or the shaping is not
    /// a rendering fix but a different measurement.
    #[test]
    fn a_plateau_is_shaped_into_itself() {
        let samples = [12; BUCKETS];
        assert!(shaped(&samples).iter().all(|value| *value == 12.0));
    }

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
