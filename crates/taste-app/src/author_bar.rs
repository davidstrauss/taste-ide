//! The file tree's banner for a project that names no git author: it says
//! so, offers a name and an email to commit as — a suggestion, filled in
//! and editable (`taste_git::suggested_identity`) — and writes them into
//! the repository's own config only when the user says to (David,
//! 2026-10-24: "At most, offer a banner in the IDE that notes no repo
//! identity set and offers to copy in the user's *by permission* and
//! stating what that identity is"). The fields are that statement: what
//! the button writes is what they hold, in full, before anyone presses it
//! — and a project that wants a work address rather than a personal one
//! changes it there.
//!
//! Beside the button, a menu says where the press goes: "Just this
//! project" by default, or, under a "Save to this project and:" heading,
//! the project AND the `.gitconfig` of a folder above it, up to the home
//! folder — the files the suggestion reads first, so every project under
//! the one chosen is offered the same author next time (David, 2026-11-06:
//! "a dropdown that defaults to 'just this project' but then has a
//! dividing line/break and then the heading 'Save to this project and:'
//! and then allow picking a path anywhere up the tree except the XDG and
//! /etc options"). It is a `GMenu` whose second section carries that
//! heading, which is how a popover menu draws a line and a heading.
//!
//! The host's git config is the user's machine state, never the project's
//! (`taste_git::GitIdentity`), so nothing else in the IDE reads it for a
//! project: without this bar's button, a project that names nobody commits
//! as nobody. A warning rather than a nudge, since commits there fail,
//! drawn at the submodule banner's place and with its edges
//! (`CHROME_INSET`). Closed, it stays closed until the IDE is opened again.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use gtk::{gio, glib, prelude::*};
use taste_git::GitIdentity;

/// What the banner is shown with: the author its fields are filled with,
/// and the `.gitconfig` files above the project its menu offers to save
/// into as well (`taste_git::gitconfigs_above`), nearest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorOffer {
    pub identity: GitIdentity,
    pub above: Vec<PathBuf>,
}

impl AuthorOffer {
    /// The offer for the project at `folder`. Blocking: file IO.
    pub fn read(folder: &Path) -> Self {
        Self {
            identity: taste_git::suggested_identity(folder),
            above: taste_git::gitconfigs_above(folder),
        }
    }
}

/// The menu's action, and its target for "just this project".
const SAVE_TO: &str = "save-to";
const JUST_THIS: &str = "";
/// "Just this project": the menu's default, and its first item.
const JUST_THIS_LABEL: &str = "Just this project";

pub struct AuthorBar {
    pub widget: gtk::Revealer,
    name: gtk::Entry,
    email: gtk::Entry,
    pub copy: gtk::Button,
    close: gtk::Button,
    /// Where the button writes: its state is the path of a `.gitconfig`
    /// above the project to write as well, or [`JUST_THIS`].
    save_to: gio::SimpleAction,
    /// The menu's button, and the label on it, which names the choice.
    menu: gtk::MenuButton,
    choice: gtk::Label,
    /// The files the menu offers, as it was last built.
    above: RefCell<Vec<PathBuf>>,
    /// The identity the fields were filled with: the suggestion, which a
    /// status refresh offers again every time it runs. Kept so a refresh
    /// fills the fields only while they still hold it, and never over what
    /// the user typed.
    filled: RefCell<Option<GitIdentity>>,
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
        let name = field("The name this repository's commits are made under");
        let email = field("The email this repository's commits are made under");
        email.set_input_purpose(gtk::InputPurpose::Email);
        // The two fields under the sentence, the width of the column, each
        // named by a dim label in front of it: filled in, a field's
        // placeholder never shows, so the label is what says which is which.
        let form = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(8)
            .margin_start(inset)
            .margin_end(inset)
            .build();
        for (row, (label, entry)) in [("Name", &name), ("Email", &email)].into_iter().enumerate() {
            let label = gtk::Label::builder()
                .label(label)
                .xalign(0.0)
                .mnemonic_widget(entry)
                .css_classes(["dim-label"])
                .build();
            form.attach(&label, 0, row as i32, 1, 1);
            form.attach(entry, 1, row as i32, 1, 1);
        }
        let save_to = gio::SimpleAction::new_stateful(
            SAVE_TO,
            Some(glib::VariantTy::STRING),
            &JUST_THIS.to_variant(),
        );
        let group = gio::SimpleActionGroup::new();
        group.add_action(&save_to);
        // The choice on the button, ellipsized in the middle: a path broken
        // mid-word stops reading as one, and its end is the file's name.
        let choice = gtk::Label::builder()
            .label(JUST_THIS_LABEL)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let menu = gtk::MenuButton::builder()
            .child(&choice)
            .always_show_arrow(true)
            .hexpand(true)
            .halign(gtk::Align::Start)
            .tooltip_text(choice_tooltip(JUST_THIS))
            .build();
        let copy = gtk::Button::builder()
            .label("Set Author")
            .tooltip_text(
                "Write this name and email where the menu beside this says; \
                 nothing is written until you choose this",
            )
            .sensitive(false)
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
        said.append(&title);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        top.append(&said);
        top.append(&close);
        // The menu and the button on one line under the fields, at their
        // edges: the menu says where the button's press goes.
        let act = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        act.set_margin_start(inset);
        act.set_margin_end(inset);
        act.append(&menu);
        act.append(&copy);
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        row.add_css_class("changed-bar");
        row.add_css_class("project-author");
        row.append(&top);
        row.append(&form);
        row.append(&act);
        row.insert_action_group("author", Some(&group));
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        let bar = std::rc::Rc::new(Self {
            widget,
            name,
            email,
            copy,
            close,
            save_to,
            menu,
            choice,
            above: RefCell::new(Vec::new()),
            filled: RefCell::new(None),
            dismissed: Cell::new(false),
        });
        let weak = std::rc::Rc::downgrade(&bar);
        bar.save_to.connect_activate(move |action, target| {
            let Some(target) = target.and_then(|target| target.get::<String>()) else {
                return;
            };
            action.set_state(&target.to_variant());
            if let Some(bar) = weak.upgrade() {
                bar.choice.set_label(&choice_label(&target));
                bar.menu.set_tooltip_text(Some(&choice_tooltip(&target)));
            }
        });
        bar.set_above(Vec::new());
        let weak = std::rc::Rc::downgrade(&bar);
        bar.close.connect_clicked(move |_| {
            let Some(bar) = weak.upgrade() else { return };
            bar.dismissed.set(true);
            bar.widget.set_reveal_child(false);
        });
        for entry in [&bar.name, &bar.email] {
            // The button only while there is an author to write: git
            // refuses a commit whose name or email is empty.
            let weak = std::rc::Rc::downgrade(&bar);
            entry.connect_changed(move |_| {
                if let Some(bar) = weak.upgrade() {
                    bar.copy.set_sensitive(bar.offered().is_some());
                }
            });
            // Enter in either field is the button.
            let weak = std::rc::Rc::downgrade(&bar);
            entry.connect_activate(move |_| {
                if let Some(bar) = weak.upgrade() {
                    if bar.copy.is_sensitive() {
                        bar.copy.emit_clicked();
                    }
                }
            });
        }
        bar
    }

    /// Show the bar with its fields filled with the offer's identity —
    /// empty halves for what nothing suggested — and its menu offering the
    /// files above, or put it away: when the project names its own author,
    /// or when the user closed the bar. Fields the user has edited keep
    /// what they typed.
    pub fn show(&self, offer: Option<&AuthorOffer>) {
        match offer.filter(|_| !self.dismissed.get()) {
            Some(AuthorOffer { identity, above }) => {
                if *self.above.borrow() != *above {
                    self.set_above(above.clone());
                }
                let untouched = match self.filled.borrow().as_ref() {
                    None => true,
                    Some(filled) => {
                        self.name.text() == filled.name && self.email.text() == filled.email
                    }
                };
                if untouched {
                    *self.filled.borrow_mut() = Some(identity.clone());
                    self.name.set_text(&identity.name);
                    self.email.set_text(&identity.email);
                    self.pose_for_probe();
                }
                self.widget.set_reveal_child(true);
            }
            None => {
                *self.filled.borrow_mut() = None;
                self.save_to.activate(Some(&JUST_THIS.to_variant()));
                self.widget.set_reveal_child(false);
            }
        }
    }

    /// The identity the button writes: what the fields hold, trimmed, or
    /// `None` while either is empty.
    pub fn offered(&self) -> Option<GitIdentity> {
        identity_from(&self.name.text(), &self.email.text())
    }

    /// The `.gitconfig` above the project the menu says to write the
    /// author into as well, or `None` for the project alone.
    pub fn also_above(&self) -> Option<PathBuf> {
        let chosen = self.save_to.state()?.get::<String>()?;
        (chosen != JUST_THIS).then(|| PathBuf::from(chosen))
    }

    /// Build the menu over `above`: "Just this project", then the files
    /// under the heading. A choice the new list no longer has goes back to
    /// the project alone.
    fn set_above(&self, above: Vec<PathBuf>) {
        let menu = gio::Menu::new();
        let just = gio::Menu::new();
        just.append_item(&item(JUST_THIS_LABEL, JUST_THIS));
        menu.append_section(None, &just);
        if !above.is_empty() {
            let also = gio::Menu::new();
            for file in &above {
                also.append_item(&item(&home_relative(file), &file.to_string_lossy()));
            }
            menu.append_section(Some("Save to this project and:"), &also);
        }
        self.menu.set_menu_model(Some(&menu));
        if let Some(popover) = self.menu.popover() {
            popover.set_widget_name("author-menu");
        }
        let chosen = self.save_to.state().and_then(|state| state.get::<String>());
        if chosen.is_some_and(|chosen| {
            chosen != JUST_THIS && !above.iter().any(|file| file.to_string_lossy() == chosen)
        }) {
            self.save_to.activate(Some(&JUST_THIS.to_variant()));
        }
        *self.above.borrow_mut() = above;
    }

    /// TASTE_PROBE_CHECK only: the poses `TASTE_PROBE_AUTHOR` names beyond
    /// the bar itself — `edited`, the email changed from the suggestion;
    /// `above`, the nearest folder above chosen on the menu; `menu`, the
    /// menu open, shot as `window.author-menu` (a popover, its own
    /// surface). Typing and choosing are gestures a shot has none of.
    fn pose_for_probe(&self) {
        if std::env::var_os("TASTE_PROBE_CHECK").is_none() {
            return;
        }
        match std::env::var("TASTE_PROBE_AUTHOR").as_deref() {
            Ok("edited") => self.email.set_text("ada@analytical-engine.example"),
            Ok("above") => {
                if let Some(file) = self.above.borrow().first() {
                    self.save_to
                        .activate(Some(&file.to_string_lossy().to_variant()));
                }
            }
            Ok("menu") => {
                let menu = self.menu.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                    menu.popup();
                });
            }
            _ => {}
        }
    }
}

/// A menu item choosing `target` for [`SAVE_TO`].
fn item(label: &str, target: &str) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(
        Some(&format!("author.{SAVE_TO}")),
        Some(&target.to_variant()),
    );
    item
}

/// What the menu's button says for a choice: the project alone, or the
/// file it saves to as well.
fn choice_label(target: &str) -> String {
    if target == JUST_THIS {
        JUST_THIS_LABEL.into()
    } else {
        format!("And {}", home_relative(Path::new(target)))
    }
}

/// The button's tooltip for a choice, the path whole.
fn choice_tooltip(target: &str) -> String {
    if target == JUST_THIS {
        "Saved in this repository's own config (.git/config) only".into()
    } else {
        format!("Saved in this repository's own config (.git/config) and in {target}")
    }
}

/// A field of the form: as narrow as the column lets it be, since the
/// file tree's column is narrower than an entry's natural width.
fn field(tooltip: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .hexpand(true)
        .width_chars(1)
        .tooltip_text(tooltip)
        .build()
}

/// `path` with the home folder written `~`, as the user knows it.
fn home_relative(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What the bar says.
pub const SENTENCE: &str = "This project names no git author, so commits in it fail";

/// The identity a name and an email make, trimmed, or `None` when either
/// is empty once it is.
pub fn identity_from(name: &str, email: &str) -> Option<GitIdentity> {
    let (name, email) = (name.trim(), email.trim());
    (!name.is_empty() && !email.is_empty()).then(|| GitIdentity {
        name: name.into(),
        email: email.into(),
    })
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_AUTHOR` poses the bar with an
/// identity as long as a real one, since the folder a probe opens either
/// names an author or runs where the user's own identity is not the one
/// to put in a screenshot; the menu offers the real folders above
/// `folder`. Blocking, as [`AuthorOffer::read`] is.
pub fn posed_for_probe(folder: &Path) -> Option<AuthorOffer> {
    std::env::var_os("TASTE_PROBE_CHECK")?;
    std::env::var_os("TASTE_PROBE_AUTHOR")?;
    Some(AuthorOffer {
        identity: GitIdentity {
            name: "Ada Lovelace".into(),
            email: "ada.lovelace@example.org".into(),
        },
        above: taste_git::gitconfigs_above(folder),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_button_names_the_choice() {
        assert_eq!(choice_label(JUST_THIS), "Just this project");
        assert_eq!(
            choice_label("/srv/work/.gitconfig"),
            "And /srv/work/.gitconfig"
        );
    }

    #[test]
    fn the_fields_make_a_trimmed_identity() {
        assert_eq!(
            identity_from("  Ada ", "ada@example.org\n"),
            Some(GitIdentity {
                name: "Ada".into(),
                email: "ada@example.org".into(),
            })
        );
    }

    #[test]
    fn an_empty_field_makes_no_identity() {
        assert_eq!(identity_from("Ada", "  "), None);
        assert_eq!(identity_from("", "ada@example.org"), None);
    }
}
