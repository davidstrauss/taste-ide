//! The shared one-row composer card: [left action] [bordered input field]
//! [right actions]. The commit box and the chat prompt are the SAME
//! widget with different icons and effects — one look, defined once.

use adw::prelude::*;

pub struct Composer {
    pub widget: gtk::Box,
}

impl Composer {
    /// `input` goes inside the bordered field (an Entry with the
    /// `flat-entry` class, or a TextView's scroller — anything editable).
    /// Side actions are anchored to the bottom edge, so a multiline input
    /// growing upward leaves them in place.
    pub fn new(
        left: &impl IsA<gtk::Widget>,
        input: &impl IsA<gtk::Widget>,
        rights: &[gtk::Widget],
    ) -> Self {
        // No inner border: the CARD is the field. Focus anywhere inside
        // rings the whole row, buttons included.
        let field = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field.set_hexpand(true);
        field.append(input);

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        widget.add_css_class("prompt-entry");
        // The search box's anatomy, mirrored: icon inset left, icon inset
        // right, text between — actions vertically centered like a
        // SearchEntry's magnifier.
        let left = left.upcast_ref::<gtk::Widget>();
        left.add_css_class("composer-action");
        left.set_valign(gtk::Align::End);
        left.set_margin_bottom(4);
        left.set_margin_start(2);
        widget.append(left);
        widget.append(&field);
        for action in rights {
            action.add_css_class("composer-action");
            action.set_valign(gtk::Align::End);
            action.set_margin_bottom(4);
            action.set_margin_end(2);
            widget.append(action);
        }
        Self { widget }
    }
}
