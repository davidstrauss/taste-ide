//! The window, closing: what is left to do before it can go, over the
//! whole window (David, 2026-10-03: "On close, I'd like the current
//! sync/cloud status to take over the entire window to show remaining
//! work before shutdown").
//!
//! A close is not instant: the tabs and chats are saved, the VMs are told
//! to stop after a grace, Personal's uncommitted work is snapshotted, and
//! the folder is brought up to date with it — bounded, so a VM that does
//! not answer cannot hold the window open (`window.rs`). That used to
//! happen behind a window that simply did not close for a while. Now the
//! window's content gives way to this page: the steps as a checklist, in
//! the startup page's own rows (`startup::StepRow`), so a window's start
//! and its end read as one kind of page; and under them the title bar's
//! cloud popover itself, moved here rather than copied (`SyncStatus::
//! take_over`), so the transfer under way, its bar, and the connection are
//! the same widgets the user has been reading all along.
//!
//! "Close Now" is there because the wait is the IDE's, not the user's:
//! what it skips stays safe in Personal's checkout, and the folder catches
//! up at the next launch.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::startup::{Status, StepRow};

/// What a close does, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The open tabs and the chats, written to the workspace's state.
    Save,
    /// The workspace's VMs told to stop once the grace is up.
    StopVms,
    /// Personal's uncommitted work kept as a snapshot.
    Snapshot,
    /// The folder brought up to date with Personal.
    Sync,
}

impl Step {
    pub const ALL: [Step; 4] = [Step::Save, Step::StopVms, Step::Snapshot, Step::Sync];

    fn title(self) -> &'static str {
        match self {
            Step::Save => "Save your tabs and chats",
            Step::StopVms => "Schedule the VMs to stop",
            Step::Snapshot => "Keep Personal's uncommitted work",
            Step::Sync => "Bring your folder up to date",
        }
    }
}

pub struct ClosingPage {
    pub widget: gtk::Widget,
    rows: Vec<(Step, StepRow)>,
    close_now: gtk::Button,
    on_close_now: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ClosingPage {
    /// The page, around `status` — the cloud popover's content, taken
    /// over. `mirrored` is whether the folder mirrors a checkout in a VM;
    /// when it does not, there is nothing to snapshot or bring over, and
    /// those steps are not listed.
    pub fn new(status: &gtk::Widget, mirrored: bool) -> Rc<Self> {
        let icon = gtk::Image::builder()
            .icon_name("taste-cloud-symbolic")
            .pixel_size(48)
            .css_classes(["dim-label"])
            .build();
        let heading = gtk::Label::builder()
            .label("Finishing before closing")
            .css_classes(["title-2"])
            .build();
        let lede = gtk::Label::builder()
            .label("Your work is put where it belongs first. The window closes when it is done.")
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();

        let steps = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let rows: Vec<(Step, StepRow)> = Step::ALL
            .into_iter()
            .filter(|step| mirrored || matches!(step, Step::Save | Step::StopVms))
            .map(|step| {
                let row = StepRow::titled(step.title());
                steps.append(&row.row);
                (step, row)
            })
            .collect();

        let close_now = gtk::Button::builder()
            .label("Close Now")
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .tooltip_text(
                "Close without waiting: nothing is lost, since your work stays in Personal's \
                 checkout, and your folder catches up the next time this project opens",
            )
            .build();

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(48)
            .margin_bottom(48)
            .margin_start(16)
            .margin_end(16)
            .halign(gtk::Align::Center)
            .width_request(360)
            .build();
        let top = gtk::Box::new(gtk::Orientation::Vertical, 12);
        top.append(&icon);
        top.append(&heading);
        top.append(&lede);
        column.append(&top);
        column.append(&steps);
        column.append(&close_now);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(status);
        let clamp = adw::Clamp::builder()
            .maximum_size(420)
            .child(&column)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        // The window keeps its title bar, and with it its controls: the
        // page is the window's content, not a replacement for the window.
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroller));
        // A probe target of its own: `window.closing`.
        view.set_widget_name("closing");
        view.add_css_class("background");
        // Focus on the one thing the page offers, so Enter closes now and
        // no label of what was taken over is left selected by GTK's
        // first-focus.
        let focus = close_now.clone();
        view.connect_map(move |_| {
            focus.grab_focus();
        });

        let page = Rc::new(Self {
            widget: view.upcast(),
            rows,
            close_now,
            on_close_now: RefCell::new(None),
        });
        let weak = Rc::downgrade(&page);
        page.close_now.connect_clicked(move |button| {
            button.set_sensitive(false);
            let Some(page) = weak.upgrade() else { return };
            let hook = page.on_close_now.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
        });
        page
    }

    pub fn set_on_close_now(&self, hook: impl Fn() + 'static) {
        *self.on_close_now.borrow_mut() = Some(Rc::new(hook));
    }

    fn row(&self, step: Step) -> Option<&StepRow> {
        self.rows
            .iter()
            .find(|(s, _)| *s == step)
            .map(|(_, row)| row)
    }

    /// `step` is under way, doing `what` if it says.
    pub fn begin(&self, step: Step, what: Option<&str>) {
        if let Some(row) = self.row(step) {
            row.set(Status::Active, what);
        }
    }

    /// `step` is done, and came to `conclusion`.
    pub fn finish(&self, step: Step, conclusion: &str) {
        if let Some(row) = self.row(step) {
            row.conclude(conclusion);
        }
    }

    /// `step` did not finish, for `why`; the close goes on regardless.
    pub fn fail(&self, step: Step, why: &str) {
        if let Some(row) = self.row(step) {
            row.set(Status::Failed, Some(why));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Step;

    #[test]
    fn steps_are_named_as_acts_and_in_order() {
        assert_eq!(Step::ALL[0], Step::Save);
        assert_eq!(Step::ALL[3], Step::Sync);
        for step in Step::ALL {
            let title = step.title();
            assert!(title.chars().next().unwrap().is_uppercase(), "{title}");
            assert!(!title.ends_with('.'), "{title}");
        }
    }
}
