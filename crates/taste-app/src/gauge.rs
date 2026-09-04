//! One gauge for "how much of a pool is used", wherever it is drawn.
//!
//! Two surfaces show a fraction that runs out: the environments panel's
//! header (the subscription window) and the chat's header (the
//! conversation's context). They drew it two ways — one a 32px bar with
//! the percentage spelled beside it, the other a 90px bar recoloured by
//! GTK's stock offsets, whose palette (yellow low, accent high, green
//! full) says the opposite of what running out means — and stated the
//! same two thresholds in three places. David: "The utilization bar/layout
//! should be the same for env and the agent. Drop the explicit percentage,
//! but make the bar use the traffic light colour palette in a way that
//! emphasizes approaching resource exhaustion."
//!
//! So: one widget, one width, no number (the number is in the tooltip,
//! where a reader who wants it hovers), and the traffic light the rest of
//! this UI already speaks — green while there is room, amber past three
//! fifths, red past 85% or once the window is closed. The thresholds live
//! here and nowhere else; the utilization tab's glyph reads them from here
//! too, so no two surfaces can disagree about whether to worry.

use gtk::prelude::*;

/// Past this, the gauge turns amber: worth noticing.
pub const WARN_AT: f64 = 0.6;
/// Past this, red: the pool is nearly gone.
pub const SPENT_AT: f64 = 0.85;
/// One width in both headers. Wide enough that the fill's position reads
/// at a glance, narrow enough for a 320px pane's header to keep its words.
const WIDTH: i32 = 48;

/// How worried the gauge is — the one vocabulary for its colour, the
/// utilization glyph, and any tooltip verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Spent,
}

impl Severity {
    /// From a used fraction, or an explicit "closed" (a window the API has
    /// refused, whatever the fraction said last).
    pub fn of(used: f64, spent: bool) -> Self {
        if spent || used >= SPENT_AT {
            Self::Spent
        } else if used >= WARN_AT {
            Self::Warn
        } else {
            Self::Ok
        }
    }

    /// The CSS class the gauge wears for this severity (main.rs).
    pub fn css(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Spent => "spent",
        }
    }
}

/// A gauge, ready to be placed. Hidden until something is known: an empty
/// bar reads as "nothing spent" rather than "nothing observed".
pub fn new() -> gtk::LevelBar {
    gtk::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .mode(gtk::LevelBarMode::Continuous)
        .width_request(WIDTH)
        .valign(gtk::Align::Center)
        .css_classes(["usage-gauge"])
        .visible(false)
        .build()
}

/// Show `used` (0..=1) on `bar`, coloured by severity. `spent` forces the
/// full red bar for a window the API has closed; `stale` fades a reading
/// that is still true about the past and nothing about now.
pub fn set(bar: &gtk::LevelBar, used: f64, spent: bool, stale: bool) {
    let used = used.clamp(0.0, 1.0);
    bar.set_value(if spent { 1.0 } else { used });
    for class in ["ok", "warn", "spent", "stale"] {
        bar.remove_css_class(class);
    }
    bar.add_css_class(Severity::of(used, spent).css());
    if stale {
        bar.add_css_class("stale");
    }
    bar.set_visible(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The thresholds, stated once: green, then amber at three fifths,
    /// red at 85%, and a closed window is red whatever the fraction says.
    #[test]
    fn severity_follows_the_two_thresholds_and_a_closed_window() {
        assert_eq!(Severity::of(0.0, false), Severity::Ok);
        assert_eq!(Severity::of(0.59, false), Severity::Ok);
        assert_eq!(Severity::of(0.6, false), Severity::Warn);
        assert_eq!(Severity::of(0.84, false), Severity::Warn);
        assert_eq!(Severity::of(0.85, false), Severity::Spent);
        assert_eq!(Severity::of(0.1, true), Severity::Spent);
    }
}
