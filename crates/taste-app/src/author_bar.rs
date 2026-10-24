//! The file tree's banner for a project that names no git author: it says
//! so, names the user's own identity, and copies it into the repository's
//! own config only when the user says to (David, 2026-10-24: "At most,
//! offer a banner in the IDE that notes no repo identity set and offers to
//! copy in the user's *by permission* and stating what that identity is").
//!
//! The host's `~/.gitconfig` is the user's machine state, never the
//! project's (`taste_git::GitIdentity`), so nothing else in the IDE reads
//! it for a project: without this bar's button, a project that names
//! nobody commits as nobody. A warning rather than a nudge, since commits
//! there fail, drawn at the submodule banner's place and with its edges
//! (`CHROME_INSET`). Closed, it stays closed until the IDE is opened again.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use taste_git::GitIdentity;

pub struct AuthorBar {
    pub widget: gtk::Revealer,
    detail: gtk::Label,
    pub copy: gtk::Button,
    close: gtk::Button,
    /// The identity the bar names, which its button copies.
    offered: RefCell<Option<GitIdentity>>,
    /// Closed by the user, for the rest of this run.
    dismissed: Cell<bool>,
}

impl AuthorBar {
    pub fn new(inset: i32) -> std::rc::Rc<Self> {
        let glyph = gtk::Image::from_icon_name("dialog-warning-symbolic");
        // On the sentence's first line, as the submodule banner's glyph is.
        glyph.set_valign(gtk::Align::Start);
        glyph.set_margin_top(1);
        let title = gtk::Label::builder()
            .label(SENTENCE)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        // The identity in full, on a line of its own: what the button
        // would copy is said before anyone presses it.
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["dim-label"])
            .build();
        let words = gtk::Box::new(gtk::Orientation::Vertical, 2);
        words.set_hexpand(true);
        words.append(&title);
        words.append(&detail);
        let copy = gtk::Button::builder()
            .label("Use Mine")
            .tooltip_text(
                "Write this name and email into this repository's own config \
                 (.git/config); nothing is copied until you choose this",
            )
            .halign(gtk::Align::End)
            .margin_end(inset)
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Not now: commits in this project still need an author")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Start)
            .margin_end(inset)
            .build();
        let said = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        said.set_valign(gtk::Align::Center);
        said.set_hexpand(true);
        said.set_margin_start(inset);
        said.append(&glyph);
        said.append(&words);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        top.append(&said);
        top.append(&close);
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        row.add_css_class("changed-bar");
        row.add_css_class("project-author");
        row.append(&top);
        row.append(&copy);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        let bar = std::rc::Rc::new(Self {
            widget,
            detail,
            copy,
            close,
            offered: RefCell::new(None),
            dismissed: Cell::new(false),
        });
        let weak = std::rc::Rc::downgrade(&bar);
        bar.close.connect_clicked(move |_| {
            let Some(bar) = weak.upgrade() else { return };
            bar.dismissed.set(true);
            bar.widget.set_reveal_child(false);
        });
        bar
    }

    /// Show the bar offering `identity`, or put it away: when the project
    /// names its own author, when the user has no identity to offer, or
    /// when the user closed the bar.
    pub fn show(&self, identity: Option<&GitIdentity>) {
        match identity.filter(|_| !self.dismissed.get()) {
            Some(identity) => {
                self.detail.set_label(&offer(identity));
                *self.offered.borrow_mut() = Some(identity.clone());
                self.widget.set_reveal_child(true);
            }
            None => {
                *self.offered.borrow_mut() = None;
                self.widget.set_reveal_child(false);
            }
        }
    }

    /// The identity the button copies, as the bar names it.
    pub fn offered(&self) -> Option<GitIdentity> {
        self.offered.borrow().clone()
    }
}

/// What the bar says.
pub const SENTENCE: &str = "This project names no git author, so commits in it fail";

/// The identity the bar offers, as it names it: the email on a line of its
/// own, since in a column this narrow an address broken mid-word stops
/// reading as one.
pub fn offer(identity: &GitIdentity) -> String {
    format!("Yours is {}\n{}", identity.name, identity.email)
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_AUTHOR=1` poses the bar with an
/// identity as long as a real one, since the folder a probe opens either
/// names an author or runs where the user's own identity is not the one
/// to put in a screenshot.
pub fn posed_for_probe() -> Option<GitIdentity> {
    std::env::var_os("TASTE_PROBE_CHECK")?;
    std::env::var_os("TASTE_PROBE_AUTHOR")?;
    Some(GitIdentity {
        name: "Ada Lovelace".into(),
        email: "ada.lovelace@analytical-engine.example".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_names_the_identity_it_would_copy() {
        let identity = GitIdentity {
            name: "Ada".into(),
            email: "ada@example.org".into(),
        };
        assert_eq!(offer(&identity), "Yours is Ada\nada@example.org");
    }
}
