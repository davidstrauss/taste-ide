//! One gauge for "how much of a pool is used", wherever it is drawn.
//!
//! Two things show a fraction that runs out, and since 2026-09-12 they sit
//! side by side in the chat's header: the conversation's context window and
//! the account's subscription window, which used to be a flank away in the
//! environments panel's header. They drew it two ways — one a 32px bar with
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
//! fifths, and red only once the window is closed. Red is for a failure
//! and nothing else (David, 2026-11-18: "Red should only be for
//! failures"): a plan nearly spent is a warning, and a plan the API has
//! closed, which refuses the next request, is the failure. The threshold
//! lives here and nowhere else; the utilization tab's glyph reads it from
//! here too, so no two surfaces can disagree about whether to worry.
//!
//! **Only the plan runs out.** The context window fills, and a full one is
//! compacted and the conversation goes on: that is the window working, not
//! a pool spent (David, 2026-11-18: "Filling up a context window shouldn't
//! result in a red status. It's still a normal, healthy state"). So its
//! gauge is [`set_level`], the same bar in a neutral fill, and the traffic
//! light is [`set`]'s, for the subscription's window alone.

use gtk::prelude::*;

/// Past this, the gauge turns amber: worth noticing. It stays amber up to
/// full; red is only a window the API has closed.
pub const WARN_AT: f64 = 0.6;
/// Past this, a context window's tooltip says the agent will compact soon:
/// words for the reader, never a colour.
pub const NEARLY_FULL: f64 = 0.85;
/// One width for both. Wide enough that the fill's position reads at a
/// glance, narrow enough for a 335px flank's header to keep its words —
/// which is what took it from 48 to 40 when the backlog header gained
/// Refresh (2026-09-06). That header no longer draws one, and the number
/// stays: the chat's own header is now the tighter row of the two, it
/// carries TWO of these, and it is the first thing clipped at the chat
/// column's edge.
const WIDTH: i32 = 40;

/// How worried the gauge is — the one vocabulary for its colour, the
/// utilization glyph, and any tooltip verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Spent,
}

impl Severity {
    /// From a used fraction, and whether the window is closed (a window
    /// the API has refused, whatever the fraction said last), which alone
    /// is `Spent`.
    pub fn of(used: f64, spent: bool) -> Self {
        if spent {
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

/// Show `used` (0..=1) on `bar`, coloured by severity. `spent`, a window
/// the API has closed, is the full red bar, and the only red one; `stale` fades a reading
/// that is still true about the past and nothing about now.
pub fn set(bar: &gtk::LevelBar, used: f64, spent: bool, stale: bool) {
    let used = used.clamp(0.0, 1.0);
    bar.set_value(if spent { 1.0 } else { used });
    clear(bar);
    bar.add_css_class(Severity::of(used, spent).css());
    if stale {
        bar.add_css_class("stale");
    }
    bar.set_visible(true);
}

/// Show `used` (0..=1) on `bar` as a level, in a neutral fill: how full,
/// and no verdict on it. The context window's, which fills as a
/// conversation goes on and is compacted when it is full.
pub fn set_level(bar: &gtk::LevelBar, used: f64) {
    bar.set_value(used.clamp(0.0, 1.0));
    clear(bar);
    bar.add_css_class("level");
    bar.set_visible(true);
}

fn clear(bar: &gtk::LevelBar) {
    for class in ["ok", "warn", "spent", "stale", "level"] {
        bar.remove_css_class(class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The threshold, stated once: green, then amber at three fifths up to
    /// full, and red only for a closed window, whatever the fraction says.
    #[test]
    fn red_is_a_closed_window_and_nothing_else() {
        assert_eq!(Severity::of(0.0, false), Severity::Ok);
        assert_eq!(Severity::of(0.59, false), Severity::Ok);
        assert_eq!(Severity::of(0.6, false), Severity::Warn);
        assert_eq!(Severity::of(0.99, false), Severity::Warn);
        assert_eq!(Severity::of(1.0, false), Severity::Warn);
        assert_eq!(Severity::of(0.1, true), Severity::Spent);
    }
}
