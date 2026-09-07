//! The floating jump: one pill for every scrolling area that has somewhere
//! to take you.
//!
//! Three places had grown one each, and no two alike — the chat's
//! "New messages below" was a full-width row under the transcript, the
//! backlog's back-to-top a circular OSD button in a corner, the log page's
//! a suggested-action pill (David, 2026-09-07: "We should use a
//! standardized positioning and visual language for these overlay/inset
//! buttons"). This is the one shape: an OSD pill, an icon and a word or
//! two, floating INSIDE the scrolling area, centred, on the edge it points
//! at — the bottom for "the newest is below", the top for "back up" —
//! and set in from that edge by the height of whatever is pinned there
//! (the chat's pinned prompt), so it sits just inside the part that
//! actually scrolls. It slides in from its edge and out again.
//!
//! Who shows it, and when, stays with the owner: this module is the pill
//! and its placement, nothing about scrolling.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

/// Which edge of the scrolling area the pill floats on, which is also the
/// direction it points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
}

/// The pill's distance from its edge with nothing pinned there.
const EDGE_MARGIN: i32 = 8;

pub struct Jump {
    /// The revealer to add as an overlay child; it carries the placement.
    pub widget: gtk::Revealer,
    button: gtk::Button,
    edge: Edge,
    /// What is pinned on this edge, in pixels, over and above the margin.
    inset: Cell<i32>,
}

impl Jump {
    pub fn new(edge: Edge, icon_name: &str, text: &str, tooltip: &str) -> Rc<Self> {
        let icon = gtk::Image::from_icon_name(icon_name);
        let label = gtk::Label::builder()
            .label(text)
            .css_classes(["caption"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.append(&icon);
        content.append(&label);
        let button = gtk::Button::builder()
            .child(&content)
            .tooltip_text(tooltip)
            .css_classes(["osd", "pill", "inset-jump"])
            .build();
        let widget = gtk::Revealer::builder()
            .child(&button)
            .halign(gtk::Align::Center)
            .build();
        match edge {
            Edge::Top => {
                widget.set_valign(gtk::Align::Start);
                widget.set_margin_top(EDGE_MARGIN);
                widget.set_transition_type(gtk::RevealerTransitionType::SlideDown);
            }
            Edge::Bottom => {
                widget.set_valign(gtk::Align::End);
                widget.set_margin_bottom(EDGE_MARGIN);
                widget.set_transition_type(gtk::RevealerTransitionType::SlideUp);
            }
        }
        // The revealer takes no clicks when hidden, or it would sit over
        // the content it floats on.
        widget.set_can_target(false);
        {
            let widget = widget.clone();
            widget
                .clone()
                .connect_child_revealed_notify(move |revealer| {
                    widget.set_can_target(revealer.is_child_revealed());
                });
        }
        Rc::new(Self {
            widget,
            button,
            edge,
            inset: Cell::new(0),
        })
    }

    /// Slide the pill in or out.
    pub fn show(&self, shown: bool) {
        if shown {
            // Targetable at once: a click during the slide-in must land.
            self.widget.set_can_target(true);
        }
        self.widget.set_reveal_child(shown);
    }

    /// How much is pinned on this pill's edge — a band the pill has to sit
    /// under (or over) rather than behind.
    pub fn set_inset(&self, pixels: i32) {
        if self.inset.replace(pixels) == pixels {
            return;
        }
        match self.edge {
            Edge::Top => self.widget.set_margin_top(EDGE_MARGIN + pixels),
            Edge::Bottom => self.widget.set_margin_bottom(EDGE_MARGIN + pixels),
        }
    }

    /// The pill's own height once shown, for a second pill that has to
    /// stack on the same edge.
    pub fn height(&self) -> i32 {
        self.button.height().max(26) + 6
    }

    pub fn connect_clicked(&self, act: impl Fn() + 'static) {
        self.button.connect_clicked(move |_| act());
    }
}
