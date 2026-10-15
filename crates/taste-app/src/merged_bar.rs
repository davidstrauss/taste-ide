//! The file tree's banner for a branch that is already in the default
//! branch (`taste_git::merged`), with the switch back to it (David,
//! 2026-10-15: "I'd like a banner if the branch I'm on has already been
//! merged into the default branch. Nudge me to switch back to the
//! default").
//!
//! A nudge, not a warning: drawn as every bar over a pane is
//! (`.changed-bar`), in the accent colour, at the submodule banner's place
//! and with its edges (`CHROME_INSET`). Closed, it stays closed for that
//! branch at that commit; a commit on the branch, or another branch, is
//! asked about again.

use std::cell::RefCell;

use gtk::prelude::*;
use taste_git::merged::{Merged, MergedInto};

pub struct MergedBar {
    pub widget: gtk::Revealer,
    title: gtk::Label,
    pub switch: gtk::Button,
    close: gtk::Button,
    /// What is on the bar now, which its switch acts on.
    shown: RefCell<Option<MergedInto>>,
    /// The branch and commit the user closed the bar on.
    dismissed: RefCell<Option<(String, taste_git::Oid)>>,
}

impl MergedBar {
    pub fn new(inset: i32) -> std::rc::Rc<Self> {
        let glyph = gtk::Image::from_icon_name("object-select-symbolic");
        // On the sentence's first line, as the submodule banner's glyph is.
        glyph.set_valign(gtk::Align::Start);
        glyph.set_margin_top(1);
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        // The answer on a line of its own, under the sentence: beside it,
        // "Switch to main" and the close left a 335-pixel tree a third of
        // its width for the sentence, three lines of it before a branch
        // name of any length. Its end is the column's, as the submodule
        // banner's button's is.
        let switch = gtk::Button::builder()
            .css_classes(["suggested-action"])
            .halign(gtk::Align::End)
            .margin_end(inset)
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Not now: asked again once this branch has a new commit")
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
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        row.add_css_class("changed-bar");
        row.add_css_class("merged-branch");
        row.append(&top);
        row.append(&switch);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        let bar = std::rc::Rc::new(Self {
            widget,
            title,
            switch,
            close,
            shown: RefCell::new(None),
            dismissed: RefCell::new(None),
        });
        let weak = std::rc::Rc::downgrade(&bar);
        bar.close.connect_clicked(move |_| {
            let Some(bar) = weak.upgrade() else { return };
            let shown = bar.shown.borrow().clone();
            if let Some(merged) = shown {
                *bar.dismissed.borrow_mut() = Some((merged.branch, merged.tip));
            }
            bar.widget.set_reveal_child(false);
        });
        bar
    }

    /// Show the bar for `merged`, or put it away: when there is nothing to
    /// say, or the user closed it on this branch at this commit.
    pub fn show(&self, merged: Option<&MergedInto>) {
        let closed =
            |m: &MergedInto| self.dismissed.borrow().as_ref() == Some(&(m.branch.clone(), m.tip));
        match merged.filter(|m| !closed(m)) {
            Some(merged) => {
                self.title.set_label(&sentence(merged));
                self.title.set_tooltip_text(Some(&details(merged)));
                self.switch
                    .set_label(&format!("Switch to {}", merged.default_branch));
                self.switch.set_tooltip_text(Some(&format!(
                    "Check out {}; Pull then brings it up to date with {}",
                    merged.default_branch, merged.tracking
                )));
                *self.shown.borrow_mut() = Some(merged.clone());
                self.widget.set_reveal_child(true);
            }
            None => {
                *self.shown.borrow_mut() = None;
                self.widget.set_reveal_child(false);
            }
        }
    }

    /// The branch the switch goes to.
    pub fn default_branch(&self) -> Option<String> {
        self.shown
            .borrow()
            .as_ref()
            .map(|m| m.default_branch.clone())
    }
}

/// What the bar says.
pub fn sentence(merged: &MergedInto) -> String {
    let MergedInto {
        branch, tracking, ..
    } = merged;
    match merged.how {
        Merged::Contained => format!("{branch} is merged into {tracking}"),
        Merged::Equivalent => format!("Everything on {branch} is already in {tracking}"),
        Merged::Gone => format!("{branch} is gone from the remote"),
    }
}

/// How that was found: the bar's tooltip.
pub fn details(merged: &MergedInto) -> String {
    let MergedInto {
        branch, tracking, ..
    } = merged;
    match merged.how {
        Merged::Contained => format!("{branch}'s last commit is in {tracking}'s history."),
        Merged::Equivalent => format!(
            "{branch}'s commits are not in {tracking}, but merging them would change nothing \
             there, as after a squash or a rebase merge."
        ),
        Merged::Gone => format!(
            "{branch} was pushed, and the remote no longer has it: a forge deletes a merged \
             pull request's branch, though a branch can also be deleted unmerged."
        ),
    }
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_MERGED=contained` (or
/// `equivalent`, `gone`) poses the bar for a branch with a name as long as
/// a forge's usually are, against `origin/main`, since the folder a probe
/// opens is on its own branch.
pub fn posed_for_probe() -> Option<MergedInto> {
    std::env::var_os("TASTE_PROBE_CHECK")?;
    let how = match std::env::var("TASTE_PROBE_MERGED").ok()?.as_str() {
        "contained" => Merged::Contained,
        "equivalent" => Merged::Equivalent,
        "gone" => Merged::Gone,
        _ => return None,
    };
    Some(MergedInto {
        branch: "someone/partner-call-slides".into(),
        tip: taste_git::Oid::zero(),
        default_branch: "main".into(),
        tracking: "origin/main".into(),
        how,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_says_how_the_branch_is_in_the_default() {
        let mut merged = MergedInto {
            branch: "topic".into(),
            tip: taste_git::Oid::zero(),
            default_branch: "main".into(),
            tracking: "origin/main".into(),
            how: Merged::Contained,
        };
        assert_eq!(sentence(&merged), "topic is merged into origin/main");
        merged.how = Merged::Equivalent;
        assert_eq!(
            sentence(&merged),
            "Everything on topic is already in origin/main"
        );
        merged.how = Merged::Gone;
        assert_eq!(sentence(&merged), "topic is gone from the remote");
    }
}
