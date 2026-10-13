//! The file tree's banner for the folder's submodules that are not yet the
//! commit the project records, with the one button that sets them up
//! (`taste_git::submodule_setup`): cloned, fetched, or moved forward, in
//! the folder with the user's keys, after which the sync gives them to
//! Personal (David, 2026-10-13: "I'd prefer a banner with a one-click fix if
//! any submodules need initialization or fix-up").
//!
//! Drawn as every bar over a pane is (`.changed-bar`), in the warning colour:
//! the sentence at the start, wrapping, and the answer at the end. Its
//! edges are the tree's own column (`CHROME_INSET`), so the glyph stands
//! over the project folder's.

use gtk::prelude::*;
use taste_git::submodule_setup::{SubmoduleNeed, SubmoduleSetup};

pub struct SubmoduleBar {
    pub widget: gtk::Revealer,
    title: gtk::Label,
    pub button: gtk::Button,
}

impl SubmoduleBar {
    pub fn new(inset: i32, button: gtk::Button) -> Self {
        let glyph = gtk::Image::from_icon_name("dialog-warning-symbolic");
        glyph.add_css_class("warning");
        // On the sentence's first line, whatever it wraps to: the glyph is
        // 16 and the line 18, measured (`first_line` in a probe's dump).
        glyph.set_valign(gtk::Align::Start);
        glyph.set_margin_top(1);
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        button.add_css_class("suggested-action");
        button.remove_css_class("flat");
        button.set_valign(gtk::Align::Center);
        button.set_margin_end(inset);
        button.set_tooltip_text(Some(
            "Clone, fetch, or move each one to the commit this project records, in this \
             folder with your keys; the checkout in the VM then gets them",
        ));
        // The glyph and the sentence together, centred against the button:
        // the glyph then stands on the first line at one line or two. The
        // gap is the project folder's row's, so the sentence starts where
        // its name does.
        let said = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        said.set_valign(gtk::Align::Center);
        said.set_hexpand(true);
        said.set_margin_start(inset);
        said.append(&glyph);
        said.append(&title);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("changed-bar");
        row.add_css_class("submodule-setup");
        row.append(&said);
        row.append(&button);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        Self {
            widget,
            title,
            button,
        }
    }

    /// Show the bar for `setups`, or put it away when there are none — unless
    /// a setup is running, which keeps it where it is until it ends.
    pub fn show(&self, setups: &[SubmoduleSetup], busy: bool) {
        if !setups.is_empty() {
            self.title.set_label(&sentence(setups));
            self.title.set_tooltip_text(Some(&details(setups)));
        }
        self.widget.set_reveal_child(busy || !setups.is_empty());
    }
}

/// What the bar says.
pub fn sentence(setups: &[SubmoduleSetup]) -> String {
    match setups {
        [one] => {
            let name = one.path.display();
            match one.need {
                SubmoduleNeed::Clone => format!("The submodule {name} isn't cloned in this folder"),
                SubmoduleNeed::Fetch => {
                    format!("The submodule {name} doesn't have the commit this project records")
                }
                SubmoduleNeed::Advance => {
                    format!("The submodule {name} is behind the commit this project records")
                }
            }
        }
        many => format!("{} submodules aren't set up in this folder", many.len()),
    }
}

/// Each one and what it needs, a line apiece: the bar's tooltip.
pub fn details(setups: &[SubmoduleSetup]) -> String {
    setups
        .iter()
        .map(|setup| {
            let need = match setup.need {
                SubmoduleNeed::Clone => "not cloned",
                SubmoduleNeed::Fetch => "missing the commit this project records",
                SubmoduleNeed::Advance => "behind the commit this project records",
            };
            format!("{}: {need}", setup.path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_SUBMODULES=tpl:clone,ext/lib:behind`
/// poses those submodules as needing setup (`clone`, `fetch`, or `behind`),
/// since the folder a probe opens has none.
pub fn posed_for_probe() -> Option<Vec<SubmoduleSetup>> {
    std::env::var_os("TASTE_PROBE_CHECK")?;
    let posed = std::env::var("TASTE_PROBE_SUBMODULES").ok()?;
    Some(
        posed
            .split(',')
            .filter_map(|item| {
                let (path, need) = item.split_once(':')?;
                let need = match need {
                    "clone" => SubmoduleNeed::Clone,
                    "fetch" => SubmoduleNeed::Fetch,
                    "behind" => SubmoduleNeed::Advance,
                    _ => return None,
                };
                Some(SubmoduleSetup {
                    path: path.into(),
                    need,
                    pin: taste_git::Oid::zero(),
                    url: None,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(path: &str, need: SubmoduleNeed) -> SubmoduleSetup {
        SubmoduleSetup {
            path: path.into(),
            need,
            pin: taste_git::Oid::zero(),
            url: None,
        }
    }

    #[test]
    fn one_submodule_is_named_with_its_need_and_several_are_counted() {
        assert_eq!(
            sentence(&[setup("amutable-typst-template", SubmoduleNeed::Clone)]),
            "The submodule amutable-typst-template isn't cloned in this folder"
        );
        let two = [
            setup("tpl", SubmoduleNeed::Clone),
            setup("ext/lib", SubmoduleNeed::Advance),
        ];
        assert_eq!(sentence(&two), "2 submodules aren't set up in this folder");
        assert_eq!(
            details(&two),
            "tpl: not cloned\next/lib: behind the commit this project records"
        );
    }
}
