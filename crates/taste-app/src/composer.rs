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
        let field = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field.add_css_class("prompt-field");
        field.set_hexpand(true);
        field.append(input);

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        widget.add_css_class("prompt-entry");
        let left = left.upcast_ref::<gtk::Widget>();
        left.set_valign(gtk::Align::End);
        left.set_margin_bottom(2);
        widget.append(left);
        widget.append(&field);
        for action in rights {
            action.set_valign(gtk::Align::End);
            action.set_margin_bottom(2);
            widget.append(action);
        }
        Self { widget }
    }
}
