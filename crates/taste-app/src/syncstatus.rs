//! The title bar's cloud: the user's folder and Personal's checkout kept
//! in step (`taste_git::mirror`), and the project's Google Cloud
//! connection (`cloud_form.rs`), as one icon with a coloured badge and, on
//! a click, one popover with the sync on top and the connection below it.
//!
//! The badge is the worse of the two (David, 2026-10-03: "a cloud with a
//! green badge when all sync and auth is looking good. Change to yellow if
//! sync needs to send data around still. Use red if sync has a conflict or
//! if a cloud connection has failed"): green when the folder is in step
//! and the connection is ready or was never set up, yellow while a pass
//! has something to move or the connection is under way or waiting on the
//! user, red on a conflict, a sync that failed, or a connection that did.
//!
//! The sync half is modelled on the file operations button of GNOME Files
//! (David, 2026-09-23: "Use the GNOME file browser's file transfer UI as
//! the basis"): rows of a bold status line, a dim detail line, and a
//! progress bar. The popover as a whole is laid out the way a GNOME
//! popover menu is, sections under dim headers with a separator between.
//! The button is always there: the connection lives behind it whether or
//! not the folder mirrors anything, and "in step" and "these disagree" are
//! states worth seeing at a glance.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use taste_core::FolderSync;

use crate::cloud_form::{CloudForm, CloudLight};

/// Finished passes kept in the popover, newest first.
const HISTORY: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    /// Nothing has been heard yet, or the last pass moved nothing.
    InStep,
    Pending,
    Running,
    Conflict,
    Failed,
}

impl Face {
    fn light(self) -> CloudLight {
        match self {
            Face::InStep => CloudLight::Quiet,
            Face::Pending | Face::Running => CloudLight::Waiting,
            Face::Conflict | Face::Failed => CloudLight::Failed,
        }
    }

    fn sentence(self) -> &'static str {
        match self {
            Face::InStep => "Your folder is in step with Personal",
            Face::Pending => "A change was seen — syncing in a moment",
            Face::Running => "Syncing your folder with Personal",
            Face::Conflict => "Your folder and Personal disagree — click to resolve",
            Face::Failed => "Your folder is not in step with Personal — click for why",
        }
    }
}

pub struct SyncStatus {
    pub widget: gtk::MenuButton,
    badge: gtk::Box,
    face: Cell<Face>,
    /// The folder mirrors a checkout in a VM, so the sync half has
    /// something to say.
    syncing: Cell<bool>,
    sync_section: gtk::Box,
    separator: gtk::Separator,
    content: gtk::Box,
    cloud_light: Cell<CloudLight>,
    cloud_said: RefCell<String>,
    current_title: gtk::Label,
    current_detail: gtk::Label,
    current_bar: gtk::ProgressBar,
    current_row: gtk::Box,
    conflict_row: gtk::Box,
    conflict_detail: gtk::Label,
    history_box: gtk::Box,
    history: RefCell<Vec<(Instant, String, bool)>>,
    on_resolve: RefCell<Option<Rc<dyn Fn()>>>,
}

impl SyncStatus {
    pub fn new() -> Rc<Self> {
        // The IDE's own cloud, with a dot over its lower right: the stock
        // theme has no cloud with room for a badge.
        let icon = gtk::Image::builder()
            .icon_name("taste-cloud-symbolic")
            .pixel_size(16)
            .build();
        let badge = gtk::Box::builder()
            .css_classes(["sync-badge", "green"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .can_target(false)
            .build();
        let face = gtk::Overlay::builder().child(&icon).build();
        face.add_overlay(&badge);
        let widget = gtk::MenuButton::builder()
            .child(&face)
            .css_classes(["flat", "sync-status"])
            .build();
        widget.set_widget_name("sync-status");

        let heading = gtk::Label::builder()
            .label("Local ↔ Virtualized Container Sync")
            .css_classes(["heading", "dim-label"])
            .xalign(0.0)
            .build();

        // One of Files' progress rows: status, detail, bar.
        let current_title = gtk::Label::builder()
            .css_classes(["heading"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(34)
            .build();
        let current_detail = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "numeric"])
            .xalign(0.0)
            .build();
        let current_bar = gtk::ProgressBar::new();
        let current_row = gtk::Box::new(gtk::Orientation::Vertical, 4);
        current_row.append(&current_title);
        current_row.append(&current_detail);
        current_row.append(&current_bar);
        current_row.add_css_class("sync-transfer");
        current_row.set_visible(false);

        let conflict_title = gtk::Label::builder()
            .label("Your folder and Personal disagree")
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
        let conflict_detail = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(34)
            .build();
        let resolve = gtk::Button::builder()
            .label("Resolve…")
            .css_classes(["suggested-action", "pill-action"])
            .halign(gtk::Align::Start)
            .build();
        let conflict_row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        conflict_row.append(&conflict_title);
        conflict_row.append(&conflict_detail);
        conflict_row.append(&resolve);
        conflict_row.add_css_class("sync-transfer");
        conflict_row.set_visible(false);

        let history_box = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let sync_section = gtk::Box::new(gtk::Orientation::Vertical, 12);
        sync_section.append(&heading);
        sync_section.append(&conflict_row);
        sync_section.append(&current_row);
        sync_section.append(&history_box);
        sync_section.set_visible(false);
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_visible(false);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .width_request(300)
            .build();
        content.append(&sync_section);
        content.append(&separator);
        let popover = gtk::Popover::builder().child(&content).build();
        // A probe target of its own: `window.sync-transfers`.
        popover.set_widget_name("sync-transfers");
        widget.set_popover(Some(&popover));

        let this = Rc::new(Self {
            widget,
            badge,
            face: Cell::new(Face::InStep),
            syncing: Cell::new(false),
            sync_section,
            separator,
            content,
            cloud_light: Cell::new(CloudLight::Quiet),
            cloud_said: RefCell::new(String::new()),
            current_title,
            current_detail,
            current_bar,
            current_row,
            conflict_row,
            conflict_detail,
            history_box,
            history: RefCell::new(Vec::new()),
            on_resolve: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&this);
            resolve.connect_clicked(move |button| {
                let Some(this) = weak.upgrade() else { return };
                if let Some(popover) = button.ancestor(gtk::Popover::static_type()) {
                    popover.downcast_ref::<gtk::Popover>().unwrap().popdown();
                }
                let hook = this.on_resolve.borrow().clone();
                if let Some(hook) = hook {
                    hook();
                }
            });
        }
        // The history's "how long ago" is read when the popover opens.
        {
            let weak = Rc::downgrade(&this);
            popover.connect_show(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.draw_history();
                }
            });
        }
        this.set_face(Face::InStep);
        this
    }

    /// The project's Google Cloud connection, as the popover's second
    /// section, lighting the badge with what it says.
    pub fn attach_cloud(self: &Rc<Self>, cloud: &Rc<CloudForm>) {
        self.content.append(&cloud.widget);
        if let Some(popover) = self.widget.popover() {
            let cloud = Rc::downgrade(cloud);
            popover.connect_show(move |_| {
                if let Some(cloud) = cloud.upgrade() {
                    cloud.popped_up();
                }
            });
        }
        let weak = Rc::downgrade(self);
        cloud.set_on_light(move |light, said| {
            if let Some(this) = weak.upgrade() {
                this.cloud_light.set(light);
                *this.cloud_said.borrow_mut() = said.to_string();
                this.draw_badge();
            }
        });
    }

    /// The popover's content, for the closing page to show over the whole
    /// window (`closing.rs`): the same widgets, so the transfer under way
    /// keeps drawing where the user can see it. The button has nothing to
    /// open after this, which is right, since the window is going.
    pub fn take_over(&self) -> gtk::Widget {
        if let Some(popover) = self.widget.popover() {
            popover.popdown();
            popover.set_child(None::<&gtk::Widget>);
        }
        self.content.set_width_request(-1);
        self.content.set_margin_start(0);
        self.content.set_margin_end(0);
        self.content.clone().upcast()
    }

    /// What Resolve… does: the window asks which side to keep.
    pub fn set_on_resolve(&self, hook: impl Fn() + 'static) {
        *self.on_resolve.borrow_mut() = Some(Rc::new(hook));
    }

    /// The folder mirrors a checkout in a VM, so the sync half has
    /// something to say.
    pub fn show(&self) {
        if !self.syncing.replace(true) {
            self.sync_section.set_visible(true);
            self.separator.set_visible(true);
            self.draw_badge();
        }
    }

    /// One moment of the sync.
    pub fn apply(self: &Rc<Self>, event: &FolderSync) {
        self.show();
        match event {
            FolderSync::Pending => {
                if !matches!(self.face.get(), Face::Running | Face::Conflict) {
                    self.set_face(Face::Pending);
                }
            }
            FolderSync::Running { step, done, total } => {
                self.current_title.set_label(step);
                let fraction = (*total > 0).then(|| *done as f64 / *total as f64);
                match fraction {
                    Some(f) => {
                        self.current_bar.set_fraction(f);
                        self.current_detail
                            .set_label(&format!("{done} of {total} files"));
                    }
                    None => {
                        self.current_bar.pulse();
                        self.current_detail.set_label("Working…");
                    }
                }
                self.current_row.set_visible(true);
                if self.face.get() != Face::Conflict {
                    self.set_face(Face::Running);
                }
            }
            FolderSync::Done { summary } => {
                self.current_row.set_visible(false);
                if let Some(summary) = summary {
                    self.remember(summary.clone(), false);
                }
                if self.face.get() != Face::Conflict {
                    self.set_face(Face::InStep);
                }
            }
            FolderSync::Failed { reason } => {
                self.current_row.set_visible(false);
                self.remember(format!("Not in step: {reason}"), true);
                if self.face.get() != Face::Conflict {
                    self.set_face(Face::Failed);
                }
            }
        }
    }

    /// The paths both sides changed differently; empty when resolved.
    pub fn set_conflicts(&self, paths: &[std::path::PathBuf]) {
        self.show();
        if paths.is_empty() {
            self.conflict_row.set_visible(false);
            if self.face.get() == Face::Conflict {
                self.set_face(Face::InStep);
            }
            return;
        }
        let names: Vec<String> = paths
            .iter()
            .take(3)
            .map(|p| p.display().to_string())
            .collect();
        let more = paths.len().saturating_sub(names.len());
        self.conflict_detail.set_label(&format!(
            "{}{} changed on both sides. Your folder stops following Personal until you choose.",
            names.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
        self.conflict_row.set_visible(true);
        self.set_face(Face::Conflict);
    }

    fn remember(&self, line: String, failed: bool) {
        let mut history = self.history.borrow_mut();
        history.insert(0, (Instant::now(), line, failed));
        history.truncate(HISTORY);
        drop(history);
        self.draw_history();
    }

    fn draw_history(&self) {
        while let Some(child) = self.history_box.first_child() {
            self.history_box.remove(&child);
        }
        let history = self.history.borrow();
        if history.is_empty() {
            self.history_box.append(
                &gtk::Label::builder()
                    .label("In step. Nothing has needed moving yet.")
                    .css_classes(["dim-label"])
                    .xalign(0.0)
                    .build(),
            );
            return;
        }
        for (at, line, failed) in history.iter() {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
            row.add_css_class("sync-transfer");
            let title = gtk::Label::builder()
                .label(line)
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(34)
                .build();
            if *failed {
                title.add_css_class("error");
            }
            row.append(&title);
            row.append(
                &gtk::Label::builder()
                    .label(ago(at.elapsed()))
                    .css_classes(["caption", "dim-label"])
                    .xalign(0.0)
                    .build(),
            );
            self.history_box.append(&row);
        }
    }

    fn set_face(&self, face: Face) {
        self.face.set(face);
        self.draw_badge();
    }

    /// The badge is the worse of the sync and the connection, and the
    /// tooltip says both.
    fn draw_badge(&self) {
        let sync = if self.syncing.get() {
            self.face.get().light()
        } else {
            CloudLight::Quiet
        };
        let light = sync.max(self.cloud_light.get());
        for class in ["green", "amber", "red"] {
            self.badge.remove_css_class(class);
        }
        self.badge.add_css_class(match light {
            CloudLight::Quiet => "green",
            CloudLight::Waiting => "amber",
            CloudLight::Failed => "red",
        });
        let mut said = Vec::new();
        if self.syncing.get() {
            said.push(self.face.get().sentence().to_string());
        }
        let cloud = self.cloud_said.borrow();
        let cloud = cloud
            .split(" · ")
            .next()
            .and_then(|s| s.split(" — ").next())
            .unwrap_or("");
        if !cloud.is_empty() {
            said.push(format!("Google Cloud: {cloud}"));
        }
        if said.is_empty() {
            said.push("Sync and Google Cloud".into());
        }
        self.widget.set_tooltip_text(Some(&said.join("\n")));
    }
}

/// "just now", "12s ago", "3 min ago", "2 h ago".
fn ago(elapsed: std::time::Duration) -> String {
    let s = elapsed.as_secs();
    match s {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::ago;
    use std::time::Duration;

    #[test]
    fn how_long_ago_reads_plainly() {
        assert_eq!(ago(Duration::from_secs(2)), "just now");
        assert_eq!(ago(Duration::from_secs(42)), "42s ago");
        assert_eq!(ago(Duration::from_secs(185)), "3 min ago");
        assert_eq!(ago(Duration::from_secs(7300)), "2 h ago");
    }
}
