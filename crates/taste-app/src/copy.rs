//! Copying to the clipboard. A copy puts the WHOLE of what it is on there,
//! never what is drawn of it: a clipped answer's code block copies the
//! block, a long prompt the prompt (David, 2026-11-06: "The copy button
//! should always copy the entire item — no truncation — or provide an
//! error saying why"). So the one way a copy comes out short is that it
//! does not happen at all, and then the user is told why — never a part
//! of the thing, silently.
//!
//! Every clipboard write in the app goes through [`whole`], or through
//! [`button`] for a copy button, so the rule is kept in one place rather
//! than at each call.

use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// The most one copy takes, in bytes. Past it, a paste hangs most of what
/// it lands in — a terminal, a text field, a chat box — so the copy is
/// refused with the reason rather than handed over to do that.
pub const LIMIT_BYTES: usize = 16_000_000;

/// Why `text` will not be copied, or `None` when it will be.
pub fn refusal(text: &str) -> Option<String> {
    (text.len() > LIMIT_BYTES).then(|| {
        format!(
            "Not copied: it is {}, more than the {} one copy takes — a paste that size hangs most of what it lands in",
            gtk::glib::format_size(text.len() as u64),
            gtk::glib::format_size(LIMIT_BYTES as u64),
        )
    })
}

/// Put all of `text` on `clipboard`, or nothing, and say why.
pub fn onto(clipboard: &gtk::gdk::Clipboard, text: &str) -> Result<(), String> {
    match refusal(text) {
        Some(why) => Err(why),
        None => {
            clipboard.set_text(text);
            Ok(())
        }
    }
}

/// Put all of `text` on `widget`'s clipboard, or nothing, and toast the
/// reason in `widget`'s window when it is nothing.
pub fn whole(widget: &impl IsA<gtk::Widget>, text: &str) -> Result<(), String> {
    let result = onto(&widget.clipboard(), text);
    if let Err(why) = &result {
        tracing::warn!("{why}");
        toast(widget.as_ref(), why);
    }
    result
}

/// Say `message` in `widget`'s window, as a toast.
pub fn toast(widget: &gtk::Widget, message: &str) {
    if let Some(root) = widget.root() {
        if let Some(overlay) = crate::markdown_view::find_toast_overlay(root.upcast_ref()) {
            overlay.add_toast(adw::Toast::new(message));
        }
    }
}

/// Make `copy`, an icon button, copy `text` whole when clicked, and say
/// on itself how that went: a check for a moment, or a warning whose
/// tooltip is the reason, beside the toast saying it.
pub fn button(copy: &gtk::Button, text: String) {
    let icon = copy
        .icon_name()
        .map_or_else(|| "edit-copy-symbolic".to_string(), |icon| icon.to_string());
    let tooltip = copy.tooltip_text();
    let reset: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
    copy.connect_clicked(move |button| {
        let (shown, hold_ms) = match whole(button, &text) {
            Ok(()) => {
                button.set_tooltip_text(tooltip.as_deref());
                ("object-select-symbolic", 1200)
            }
            Err(why) => {
                button.set_tooltip_text(Some(&why));
                ("dialog-warning-symbolic", 6000)
            }
        };
        button.set_icon_name(shown);
        if let Some(previous) = reset.borrow_mut().take() {
            previous.remove();
        }
        let button = button.clone();
        let icon = icon.clone();
        let tooltip = tooltip.clone();
        let reset_slot = reset.clone();
        let id = gtk::glib::timeout_add_local_once(
            std::time::Duration::from_millis(hold_ms),
            move || {
                button.set_icon_name(&icon);
                button.set_tooltip_text(tooltip.as_deref());
                reset_slot.borrow_mut().take();
            },
        );
        *reset.borrow_mut() = Some(id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_up_to_the_limit_goes_whole() {
        assert_eq!(refusal(&"x".repeat(LIMIT_BYTES)), None);
    }

    #[test]
    fn a_copy_past_the_limit_is_refused_with_its_size() {
        let text = "x".repeat(LIMIT_BYTES * 2);
        let why = refusal(&text).expect("refused");
        let size = |bytes: usize| gtk::glib::format_size(bytes as u64).to_string();
        assert!(
            why.starts_with(&format!("Not copied: it is {},", size(text.len()))),
            "{why}"
        );
        assert!(
            why.contains(&format!("the {} one copy takes", size(LIMIT_BYTES))),
            "{why}"
        );
    }
}
