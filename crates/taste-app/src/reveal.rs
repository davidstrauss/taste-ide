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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

/// Air between a bubble and the thing it points at.
const GAP: i32 = 2;

/// Which set of labels a reveal shows: F1's, or the logo button's (David,
/// 2026-09-08: "If I press F1, just show keyboard shortcuts. If I press
/// the logo button on the controller, just controller ones").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyboard,
    Controller,
}

/// One line of a bubble's table: what you press, and what it does.
pub type Row = (&'static str, &'static str);

struct Callout {
    target: gtk::Widget,
    /// What F1 says about the target; empty for a thing with no key.
    keys: Vec<Row>,
    /// What the logo button says; empty for a thing with no button.
    pad: Vec<Row>,
    side: gtk::PositionType,
}

pub struct Reveal {
    /// The layer over the window; the window adds it as an overlay child.
    pub layer: gtk::Fixed,
    callouts: RefCell<Vec<Callout>>,
    shown: Cell<Option<Kind>>,
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
            shown: Cell::new(None),
        })
    }

    /// Label `target` when revealed — `keys` under F1, `pad` under the
    /// logo button, either empty for none — the bubble on `side` of it,
    /// above (`Top`) or below (`Bottom`), pointing back. Each bubble is a
    /// table, one row per (trigger, effect) (David, 2026-09-08: "Each
    /// popup should be a structured table of keys -> effects"): in a
    /// trigger, `[Ctrl+F]` is drawn as keycaps, `(A)` as a controller
    /// button, and words such as `hold` and `twice` stay words.
    pub fn add(
        &self,
        target: &impl IsA<gtk::Widget>,
        keys: &[Row],
        pad: &[Row],
        side: gtk::PositionType,
    ) {
        self.callouts.borrow_mut().push(Callout {
            target: target.clone().upcast(),
            keys: keys.to_vec(),
            pad: pad.to_vec(),
            side,
        });
    }

    /// Every bubble whose target is on screen, at once. Two that would land
    /// on each other — the search box's and the tab strip's, one row apart —
    /// stack instead: the later slides away from its target until it is
    /// clear, its pointer still saying which way the thing is.
    pub fn show(&self, kind: Kind) {
        self.hide();
        self.shown.set(Some(kind));
        let layer_width = self.layer.width();
        let layer_height = f64::from(self.layer.height());
        let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
        for callout in self.callouts.borrow().iter() {
            let (target, side) = (&callout.target, callout.side);
            let rows = match kind {
                Kind::Keyboard => &callout.keys,
                Kind::Controller => &callout.pad,
            };
            if rows.is_empty() || !target.is_mapped() {
                continue;
            }
            let Some(bounds) = target.compute_bounds(&self.layer) else {
                continue;
            };
            let bubble = bubble(rows, side);
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
        self.shown.set(None);
        while let Some(child) = self.layer.first_child() {
            self.layer.remove(&child);
        }
    }
}

/// One piece of a bubble's row.
#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    Text(&'a str),
    /// `[Ctrl+F]`: one keycap per `+`-separated part.
    Keys(&'a str),
    /// `(A)`: a controller button, drawn generically — a letter in a ring,
    /// the shoulders and the D-pad in rounded boxes.
    Pad(&'a str),
}

fn tokens(line: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let next = rest.find(['[', '(']).unwrap_or(rest.len());
        let text = rest[..next].trim();
        if !text.is_empty() {
            out.push(Token::Text(text));
        }
        rest = &rest[next..];
        let Some(open) = rest.chars().next() else {
            break;
        };
        let close = if open == '[' { ']' } else { ')' };
        match rest[1..].find(close) {
            Some(end) => {
                let inner = &rest[1..1 + end];
                out.push(if open == '[' {
                    Token::Keys(inner)
                } else {
                    Token::Pad(inner)
                });
                rest = &rest[1 + end + 1..];
            }
            None => {
                out.push(Token::Text(rest.trim()));
                break;
            }
        }
    }
    out
}

fn token_widget(token: &Token<'_>) -> gtk::Widget {
    match token {
        Token::Text(text) => gtk::Label::new(Some(text)).upcast(),
        Token::Keys(keys) => {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            for (i, key) in keys.split('+').enumerate() {
                if i > 0 {
                    row.append(&gtk::Label::new(Some("+")));
                }
                row.append(
                    &gtk::Label::builder()
                        .label(key)
                        .css_classes(["keycap"])
                        .build(),
                );
            }
            row.upcast()
        }
        Token::Pad(button) => {
            let (glyph, wide) = match *button {
                "Start" => ("≡", false),
                "Guide" => ("⊙", false),
                "Up" => ("▲", true),
                "Down" => ("▼", true),
                "LB" | "RB" => (*button, true),
                other => (other, false),
            };
            let label = gtk::Label::builder()
                .label(glyph)
                .css_classes(["pad"])
                .build();
            if wide {
                label.add_css_class("pad-wide");
            }
            label.upcast()
        }
    }
}

/// The bubble: a pointer, then the card — or the card, then the pointer —
/// so the same shape reads above and below. The card is a two-column
/// table: the trigger, set right against the column's edge; the effect,
/// set left.
fn bubble(rows: &[Row], side: gtk::PositionType) -> gtk::Box {
    let card = gtk::Grid::builder()
        .row_spacing(3)
        .column_spacing(10)
        .css_classes(["reveal-bubble"])
        .build();
    for (i, (trigger, effect)) in rows.iter().enumerate() {
        let keys = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        keys.set_halign(gtk::Align::End);
        for token in tokens(trigger) {
            keys.append(&token_widget(&token));
        }
        card.attach(&keys, 0, i as i32, 1, 1);
        card.attach(
            &gtk::Label::builder()
                .label(*effect)
                .xalign(0.0)
                .css_classes(["reveal-effect"])
                .build(),
            1,
            i as i32,
            1,
            1,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_pads_and_words_come_apart_in_order() {
        assert_eq!(
            tokens("[Ctrl+F] Find · hold it · (Start) on a controller"),
            [
                Token::Keys("Ctrl+F"),
                Token::Text("Find · hold it ·"),
                Token::Pad("Start"),
                Token::Text("on a controller"),
            ]
        );
        assert_eq!(tokens("(A) (B)"), [Token::Pad("A"), Token::Pad("B")]);
        // An unclosed bracket is text, not a lost label.
        assert_eq!(tokens("[Ctrl"), [Token::Text("[Ctrl")]);
    }
}
