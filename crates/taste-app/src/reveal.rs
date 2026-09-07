//! Hold F1 — or the controller's logo button — and every keyable thing in
//! the window says what triggers it (David, 2026-09-07: "a
//! hotkey/controller mapping reveal while you press and hold F1 or the
//! logo key on the controller … always use a callout box (like a comic
//! speech bubble) consistent with libadwaita guidelines to label all the
//! various things that can be keyboard-triggered"; "the size of the UI
//! element doesn't dictate its prominence in the shortcuts visible on
//! screen").
//!
//! One bubble, whatever it labels: a card in the accent with a pointer at
//! the thing, libadwaita's popover shape drawn INSIDE the window on a layer
//! over everything — not as popups, which are surfaces of their own, take
//! grabs, and cannot be seen in the window's own frame (the probe
//! photographs the window). Nothing here is interactive: the layer takes
//! no clicks, and it empties when the key comes up.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

/// Air between a bubble and the thing it points at.
const GAP: i32 = 2;

pub struct Reveal {
    /// The layer over the window; the window adds it as an overlay child.
    pub layer: gtk::Fixed,
    callouts: RefCell<Vec<(gtk::Widget, String, gtk::PositionType)>>,
}

impl Reveal {
    pub fn new() -> Rc<Self> {
        let layer = gtk::Fixed::builder()
            .can_target(false)
            .can_focus(false)
            .hexpand(true)
            .vexpand(true)
            .build();
        Rc::new(Self {
            layer,
            callouts: RefCell::new(Vec::new()),
        })
    }

    /// Label `target` with `text` when revealed, the bubble on `side` of
    /// it — above (`Top`) or below (`Bottom`), pointing back.
    pub fn add(&self, target: &impl IsA<gtk::Widget>, text: &str, side: gtk::PositionType) {
        self.callouts
            .borrow_mut()
            .push((target.clone().upcast(), text.to_string(), side));
    }

    /// Every bubble whose target is on screen, at once. Two that would land
    /// on each other — the search box's and the tab strip's, one row apart —
    /// stack instead: the later slides away from its target until it is
    /// clear, its pointer still saying which way the thing is.
    pub fn show(&self) {
        self.hide();
        let layer_width = self.layer.width();
        let layer_height = f64::from(self.layer.height());
        let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
        for (target, text, side) in self.callouts.borrow().iter() {
            if !target.is_mapped() {
                continue;
            }
            let Some(bounds) = target.compute_bounds(&self.layer) else {
                continue;
            };
            let bubble = bubble(text, *side);
            let (_, natural_w, _, _) = bubble.measure(gtk::Orientation::Horizontal, -1);
            let (_, natural_h, _, _) = bubble.measure(gtk::Orientation::Vertical, natural_w);
            let centre = bounds.x() + bounds.width() / 2.0;
            let x = (f64::from(centre) - f64::from(natural_w) / 2.0)
                .clamp(4.0, f64::from((layer_width - natural_w - 4).max(4)));
            let (w, h) = (f64::from(natural_w), f64::from(natural_h));
            let mut y = match side {
                gtk::PositionType::Top => f64::from(bounds.y()) - h - f64::from(GAP),
                _ => f64::from(bounds.y() + bounds.height()) + f64::from(GAP),
            }
            .clamp(0.0, (layer_height - h).max(0.0));
            while let Some(&(_, py, _, ph)) = placed
                .iter()
                .find(|(px, py, pw, ph)| x < px + pw && *px < x + w && y < py + ph && *py < y + h)
            {
                y = match side {
                    gtk::PositionType::Top => py - h - f64::from(GAP),
                    _ => py + ph + f64::from(GAP),
                };
                if y < 0.0 || y + h > layer_height {
                    break;
                }
            }
            placed.push((x, y, w, h));
            self.layer.put(&bubble, x, y);
        }
    }

    pub fn hide(&self) {
        while let Some(child) = self.layer.first_child() {
            self.layer.remove(&child);
        }
    }
}

/// The bubble: a pointer, then the card — or the card, then the pointer —
/// so the same shape reads above and below.
fn bubble(text: &str, side: gtk::PositionType) -> gtk::Box {
    let card = gtk::Label::builder()
        .label(text)
        .justify(gtk::Justification::Center)
        .css_classes(["reveal-bubble"])
        .build();
    let pointer = gtk::Label::builder()
        .label(if side == gtk::PositionType::Top {
            "▼"
        } else {
            "▲"
        })
        .css_classes(["reveal-pointer"])
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_can_target(false);
    if side == gtk::PositionType::Top {
        column.append(&card);
        column.append(&pointer);
    } else {
        column.append(&pointer);
        column.append(&card);
    }
    column
}
