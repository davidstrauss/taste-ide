//! The title bar's sync status: the user's folder and Personal's checkout
//! kept in step (`taste_git::mirror`), as one icon and, on a click, the
//! transfers under way and the ones just done.
//!
//! Modelled on the file operations button of GNOME Files (David,
//! 2026-09-23: "Use the GNOME file browser's file transfer UI as the
//! basis"): a pie that fills as a transfer goes, in the header; a popover
//! of rows, each a bold status line, a dim detail line, and a progress
//! bar. Where Files hides the button when nothing is moving, this one
//! stays, because "in step" and "these disagree" are states worth seeing
//! at a glance, and a mirror that stopped is exactly when the button must
//! not be gone.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;
use taste_core::FolderSync;

/// Finished passes kept in the popover, newest first.
const HISTORY: usize = 5;

#[derive(Clone)]
enum Face {
    /// Nothing has been heard yet, or the last pass moved nothing.
    InStep,
    Pending,
    Running {
        fraction: Option<f64>,
    },
    Conflict,
    Failed,
}

pub struct SyncStatus {
    pub widget: gtk::MenuButton,
    pie: gtk::DrawingArea,
    icon: gtk::Image,
    face_stack: gtk::Stack,
    face: RefCell<Face>,
    /// The turning wedge's angle, for a pass with no count to fill by:
    /// shared with the tick callback that turns it.
    spin: Rc<Cell<f64>>,
    ticking: RefCell<Option<gtk::TickCallbackId>>,
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
        let pie = gtk::DrawingArea::builder()
            .content_width(16)
            .content_height(16)
            .build();
        let icon = gtk::Image::builder().pixel_size(16).build();
        let face_stack = gtk::Stack::new();
        face_stack.add_named(&icon, Some("icon"));
        face_stack.add_named(&pie, Some("pie"));
        let widget = gtk::MenuButton::builder()
            .child(&face_stack)
            .css_classes(["flat", "sync-status"])
            .visible(false)
            .build();
        widget.set_widget_name("sync-status");

        let heading = gtk::Label::builder()
            .label("Folder Sync")
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
        let explainer = gtk::Label::builder()
            .label("Your folder and Personal's checkout in the VM, kept in step both ways")
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(34)
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

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .width_request(300)
            .build();
        let head = gtk::Box::new(gtk::Orientation::Vertical, 2);
        head.append(&heading);
        head.append(&explainer);
        content.append(&head);
        content.append(&conflict_row);
        content.append(&current_row);
        content.append(&history_box);
        let popover = gtk::Popover::builder().child(&content).build();
        // A probe target of its own: `window.sync-transfers`.
        popover.set_widget_name("sync-transfers");
        widget.set_popover(Some(&popover));

        let this = Rc::new(Self {
            widget,
            pie,
            icon,
            face_stack,
            face: RefCell::new(Face::InStep),
            spin: Rc::new(Cell::new(0.0)),
            ticking: RefCell::new(None),
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
            this.pie.set_draw_func(move |area, cr, width, height| {
                let Some(this) = weak.upgrade() else { return };
                this.draw_pie(area, cr, width, height);
            });
        }
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

    /// What Resolve… does: the window asks which side to keep.
    pub fn set_on_resolve(&self, hook: impl Fn() + 'static) {
        *self.on_resolve.borrow_mut() = Some(Rc::new(hook));
    }

    /// The folder mirrors a checkout in a VM, so there is a sync to show.
    pub fn show(&self) {
        self.widget.set_visible(true);
    }

    /// One moment of the sync.
    pub fn apply(self: &Rc<Self>, event: &FolderSync) {
        self.show();
        match event {
            FolderSync::Pending => {
                if !matches!(*self.face.borrow(), Face::Running { .. } | Face::Conflict) {
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
                if !matches!(*self.face.borrow(), Face::Conflict) {
                    self.set_face(Face::Running { fraction });
                }
            }
            FolderSync::Done { summary } => {
                self.current_row.set_visible(false);
                if let Some(summary) = summary {
                    self.remember(summary.clone(), false);
                }
                if !matches!(*self.face.borrow(), Face::Conflict) {
                    self.set_face(Face::InStep);
                }
            }
            FolderSync::Failed { reason } => {
                self.current_row.set_visible(false);
                self.remember(format!("Not in step: {reason}"), true);
                if !matches!(*self.face.borrow(), Face::Conflict) {
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
            if matches!(*self.face.borrow(), Face::Conflict) {
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
        let (icon, tooltip, pie) = match &face {
            Face::InStep => (
                "emblem-ok-symbolic",
                "Your folder is in step with Personal",
                false,
            ),
            Face::Pending => ("", "A change was seen — syncing in a moment", true),
            Face::Running { .. } => ("", "Syncing your folder with Personal", true),
            Face::Conflict => (
                "dialog-warning-symbolic",
                "Your folder and Personal disagree — click to resolve",
                false,
            ),
            Face::Failed => (
                "dialog-error-symbolic",
                "Your folder is not in step with Personal — click for why",
                false,
            ),
        };
        *self.face.borrow_mut() = face.clone();
        self.widget.set_tooltip_text(Some(tooltip));
        for class in ["warning", "error"] {
            self.widget.remove_css_class(class);
        }
        match face {
            Face::Conflict => self.widget.add_css_class("warning"),
            Face::Failed => self.widget.add_css_class("error"),
            _ => {}
        }
        if pie {
            self.face_stack.set_visible_child_name("pie");
            self.start_ticking();
            self.pie.queue_draw();
        } else {
            self.icon.set_icon_name(Some(icon));
            self.face_stack.set_visible_child_name("icon");
            self.stop_ticking();
        }
    }

    /// The wedge turns while a pass has no count to fill the pie by.
    fn start_ticking(&self) {
        if self.ticking.borrow().is_some() {
            return;
        }
        let spin = self.spin.clone();
        let id = self.pie.add_tick_callback(move |area, clock| {
            let seconds = clock.frame_time() as f64 / 1_000_000.0;
            spin.set((seconds * 1.5) % std::f64::consts::TAU);
            area.queue_draw();
            glib::ControlFlow::Continue
        });
        *self.ticking.borrow_mut() = Some(id);
    }

    fn stop_ticking(&self) {
        if let Some(id) = self.ticking.borrow_mut().take() {
            id.remove();
        }
    }

    fn draw_pie(&self, area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32) {
        let color = area.color();
        let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
        let r = cx.min(cy) - 1.5;
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            0.3,
        );
        cr.set_line_width(1.5);
        cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        let top = -std::f64::consts::FRAC_PI_2;
        let (start, end) = match &*self.face.borrow() {
            Face::Running { fraction: Some(f) } => {
                (top, top + std::f64::consts::TAU * f.clamp(0.02, 1.0))
            }
            _ => {
                let angle = self.spin.get();
                (top + angle, top + angle + std::f64::consts::FRAC_PI_2)
            }
        };
        cr.move_to(cx, cy);
        cr.arc(cx, cy, r - 1.5, start, end);
        cr.close_path();
        let _ = cr.fill();
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
