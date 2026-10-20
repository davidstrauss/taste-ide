//! **A sync count's hover card.**
//!
//! The file tree's ⇊, ↓, and ↑ each say what they would do, to which ref,
//! with which numbers, and by which gesture. As tooltip sentences that was
//! a paragraph to read before every press (David, 2026-10-19: "I want all
//! of these tooltips to be less narrative … pop up a more flexible GTK
//! widget with a nice information presentation"). So each is a card: the
//! action as its heading, the facts as label and value, a warning when one
//! of them is a loss, and the gesture last, small.
//!
//! It is drawn INSIDE the window, on a layer over everything ([`Layer`]),
//! not as a tooltip: a tooltip, like a popover, is a surface of its own,
//! and sharing the window in a call shows the window without it (David,
//! 2026-10-19: "I would like these popups to show when I share this
//! 'window' in something like Zoom or Google Meet"). It comes up after a
//! tooltip's pause on hover, goes when the pointer leaves or the button is
//! pressed, and takes no clicks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// What a card shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncCard {
    /// The action, with its ref: "Rebase onto origin/main".
    pub title: String,
    /// The numbers it acts on, as label and value: ("New there", "1 change").
    pub facts: Vec<(String, String)>,
    /// A loss the action would cause, said in the warning colour.
    pub warning: Option<String>,
    /// How to do it: "Hold 2 s to rebase · a click only fast-forwards".
    pub gesture: Option<String>,
}

impl SyncCard {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    pub fn fact(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push((name.into(), value.into()));
        self
    }

    pub fn warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }

    pub fn warning_if(self, warning: Option<String>) -> Self {
        match warning {
            Some(warning) => self.warning(warning),
            None => self,
        }
    }

    pub fn gesture(mut self, gesture: impl Into<String>) -> Self {
        self.gesture = Some(gesture.into());
        self
    }

    /// The card, built: a few labels in a grid on a tooltip's dark
    /// ground, made each time it shows.
    pub fn widget(&self) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("osd");
        card.add_css_class("sync-card");
        let title = gtk::Label::builder()
            .label(&self.title)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        card.append(&title);
        if !self.facts.is_empty() {
            let grid = gtk::Grid::builder()
                .column_spacing(12)
                .row_spacing(2)
                .build();
            for (row, (name, value)) in self.facts.iter().enumerate() {
                let name = gtk::Label::builder()
                    .label(name)
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .build();
                let value = gtk::Label::builder()
                    .label(value)
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(44)
                    .build();
                grid.attach(&name, 0, row as i32, 1, 1);
                grid.attach(&value, 1, row as i32, 1, 1);
            }
            card.append(&grid);
        }
        if let Some(warning) = &self.warning {
            card.append(
                &gtk::Label::builder()
                    .label(warning)
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(44)
                    .css_classes(["warning"])
                    .build(),
            );
        }
        if let Some(gesture) = &self.gesture {
            card.append(
                &gtk::Label::builder()
                    .label(gesture)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }
        card.upcast()
    }
}

/// How long the pointer rests on a button before its card comes up: a
/// tooltip's pause, so the cards come and go as tooltips do.
const PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Air between a card and its button.
const GAP: f64 = 4.0;

/// The layer the cards are drawn on: over everything in the window, taking
/// no clicks. The window makes it and lays it over its content
/// ([`Layer::install`]).
pub struct Layer {
    pub layer: gtk::Fixed,
}

thread_local! {
    static LAYER: RefCell<Option<Rc<Layer>>> = const { RefCell::new(None) };
}

impl Layer {
    /// The layer, made and made the one cards are drawn on; the caller
    /// adds `layer` as an overlay over the window's content.
    pub fn install() -> Rc<Self> {
        let layer = gtk::Fixed::builder()
            .can_target(false)
            .can_focus(false)
            .hexpand(true)
            .vexpand(true)
            .build();
        let this = Rc::new(Self { layer });
        LAYER.with(|cell| *cell.borrow_mut() = Some(this.clone()));
        this
    }

    pub fn current() -> Option<Rc<Self>> {
        LAYER.with(|cell| cell.borrow().clone())
    }

    /// `card` under `target`, kept inside the window: right-aligned to the
    /// target's right edge when there is room, as a tooltip under a button
    /// near a pane's edge would sit.
    pub fn show(&self, target: &gtk::Widget, card: &SyncCard) {
        self.hide();
        let Some(bounds) = target.compute_bounds(&self.layer) else {
            return;
        };
        let widget = card.widget();
        let (_, natural_w, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
        let (_, natural_h, _, _) = widget.measure(gtk::Orientation::Vertical, natural_w);
        let (w, h) = (f64::from(natural_w), f64::from(natural_h));
        let (layer_w, layer_h) = (
            f64::from(self.layer.width()),
            f64::from(self.layer.height()),
        );
        let x = (f64::from(bounds.x()) + f64::from(bounds.width()) / 2.0 - w / 2.0)
            .clamp(4.0, (layer_w - w - 4.0).max(4.0));
        let below = f64::from(bounds.y() + bounds.height()) + GAP;
        let y = if below + h <= layer_h {
            below
        } else {
            (f64::from(bounds.y()) - GAP - h).max(0.0)
        };
        self.layer.put(&widget, x, y);
    }

    pub fn hide(&self) {
        while let Some(child) = self.layer.first_child() {
            self.layer.remove(&child);
        }
    }
}

/// Give `button` a card on hover, and return where to put what it shows:
/// `None` shows nothing.
pub fn install(button: &gtk::Button) -> Rc<RefCell<Option<SyncCard>>> {
    button.set_tooltip_text(None);
    let card: Rc<RefCell<Option<SyncCard>>> = Rc::new(RefCell::new(None));
    let pending: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
    let cancel = {
        let pending = pending.clone();
        move || {
            if let Some(id) = pending.take() {
                id.remove();
            }
            if let Some(layer) = Layer::current() {
                layer.hide();
            }
        }
    };
    let motion = gtk::EventControllerMotion::new();
    {
        let (card, pending, button) = (card.clone(), pending.clone(), button.downgrade());
        motion.connect_enter(move |_, _, _| {
            let (card, button, slot) = (card.clone(), button.clone(), pending.clone());
            let id = glib::timeout_add_local_once(PAUSE, move || {
                slot.take();
                let (Some(button), Some(layer)) = (button.upgrade(), Layer::current()) else {
                    return;
                };
                if let Some(card) = card.borrow().as_ref() {
                    layer.show(button.upcast_ref(), card);
                }
            });
            if let Some(old) = pending.replace(Some(id)) {
                old.remove();
            }
        });
    }
    {
        let cancel = cancel.clone();
        motion.connect_leave(move |_| cancel());
    }
    button.add_controller(motion);
    // A press is an answer to the card, which goes, as a tooltip does.
    let press = gtk::GestureClick::new();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    press.connect_pressed(move |_, _, _, _| cancel());
    button.add_controller(press);
    card
}

/// `count` of `thing`, plural when it is not one: "1 commit", "2 commits".
pub fn count(count: usize, thing: &str) -> String {
    if count == 1 {
        format!("1 {thing}")
    } else {
        format!("{count} {thing}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_is_built_up_in_order() {
        let card = SyncCard::new("Rebase onto origin/main")
            .fact("New there", count(1, "change"))
            .fact("Yours", count(4, "commit"))
            .gesture("Hold 2 s to rebase");
        assert_eq!(
            card.facts,
            vec![
                ("New there".to_string(), "1 change".to_string()),
                ("Yours".to_string(), "4 commits".to_string())
            ]
        );
        assert_eq!(card.warning, None);
    }
}
