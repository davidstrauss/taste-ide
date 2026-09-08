//! A label the IDE ellipsizes says its whole text on hover — while it is
//! actually clipped, and only then (David, 2026-09-08: "This should show
//! full text on hover"). One rule for every truncated title, path, chip
//! and subtitle, so no place has to remember it: the label answers
//! `query-tooltip` with its own text when its layout is ellipsized, and
//! lets whatever tooltip it was given stand when it is not.

use gtk::prelude::*;

/// Install the behaviour on `label`.
pub fn full_text_on_hover(label: &gtk::Label) {
    label.set_has_tooltip(true);
    label.connect_query_tooltip(|label, _, _, _, tooltip| {
        if label.layout().is_ellipsized() {
            tooltip.set_text(Some(label.text().as_str()));
            true
        } else {
            false
        }
    });
}

/// The same, on a builder's result, so a chain stays a chain.
pub trait FullTextOnHover {
    fn full_text_on_hover(self) -> Self;
}

impl FullTextOnHover for gtk::Label {
    fn full_text_on_hover(self) -> Self {
        full_text_on_hover(&self);
        self
    }
}
