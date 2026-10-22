//! The title bar's presentation mode: a projector beside the cloud, and a
//! popover that turns the mode on for a while or ends it
//! (`taste_core::presenting`, which says what the mode holds and why).
//!
//! On, the window keeps the machine awake and its screen lit through the
//! desktop's inhibitor (`GtkApplication::inhibit`, the portal's inside the
//! Flatpak), and the projector wears the accent colour. The mode is the
//! machine's, kept in one file every IDE window reads, so this follows the
//! file: started or ended in another project's window, it is started or
//! ended here too, and each window holds its own inhibitor while it lasts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use taste_core::presenting::Until;

/// The lengths the popover offers, in minutes: none open-ended, and none
/// past the mode's cap (`taste_core::presenting::MAX`, eight hours).
const LENGTHS: [(u64, &str); 5] = [
    (30, "For 30 Minutes"),
    (60, "For 1 Hour"),
    (120, "For 2 Hours"),
    (240, "For 4 Hours"),
    (480, "For 8 Hours"),
];

/// What the mode does, said under its heading in both faces.
const WHAT_IT_DOES: &str = "Keeps this machine awake, and holds the moves and rebuilds the \
                            IDE would start on its own. Agents ask you before theirs.";

pub struct Presentation {
    pub widget: gtk::MenuButton,
    application: glib::WeakRef<gtk::Application>,
    content: gtk::Box,
    until: Cell<Option<Until>>,
    /// The desktop's inhibitor, while this window holds it.
    cookie: Cell<Option<u32>>,
    /// The end, at its time, for a mode that has one.
    timer: RefCell<Option<glib::SourceId>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// A probe has posed the mode; the file does not overwrite it.
    posed: Cell<bool>,
}

impl Presentation {
    pub fn new(application: &impl IsA<gtk::Application>) -> Rc<Self> {
        let widget = gtk::MenuButton::builder()
            .icon_name("display-projector-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Presentation mode")
            .build();
        widget.set_widget_name("presentation");
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .width_request(300)
            .build();
        let popover = gtk::Popover::builder().child(&content).build();
        // A probe target of its own: `window.presentation-menu`.
        popover.set_widget_name("presentation-menu");
        widget.set_popover(Some(&popover));
        let this = Rc::new(Self {
            widget,
            application: application.upcast_ref::<gtk::Application>().downgrade(),
            content,
            until: Cell::new(None),
            cookie: Cell::new(None),
            timer: RefCell::new(None),
            monitor: RefCell::new(None),
            posed: Cell::new(false),
        });
        this.draw();
        this.watch();
        // Kept for as long as its button is: the window holds the button and
        // nothing else, and every handler here holds only a weak reference.
        // Dropped with the window's construction, it took the file's monitor
        // with it, and a click on a length upgraded nothing and did nothing
        // (2026-10-22: "I'm not sure it actually works at all").
        {
            let keep = this.clone();
            this.widget.connect_destroy(move |_| {
                let _ = &keep;
            });
        }
        this
    }

    /// Follow the file: read once now, and again whenever it changes. Its
    /// directory is made first, off this thread, so there is something to
    /// watch in before any window has started the mode.
    fn watch(self: &Rc<Self>) {
        let Some(file) = taste_core::presenting::file() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let read = crate::runtime::runtime().spawn_blocking(move || {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            taste_core::presenting::refresh();
            (file, taste_core::presenting::until())
        });
        glib::spawn_future_local(async move {
            let Ok((file, until)) = read.await else {
                return;
            };
            let Some(this) = weak.upgrade() else { return };
            this.apply(until);
            let Ok(monitor) = gio::File::for_path(&file)
                .monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            else {
                return;
            };
            let weak = Rc::downgrade(&this);
            monitor.connect_changed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.reread();
                }
            });
            *this.monitor.borrow_mut() = Some(monitor);
        });
    }

    /// Read the file again, off this thread, and show what it says; a mode
    /// whose time has passed is ended for every window.
    fn reread(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let read = crate::runtime::runtime().spawn_blocking(|| {
            taste_core::presenting::refresh();
            let until = taste_core::presenting::until();
            if until.is_none() {
                // Nothing, or a time already past: the file goes with it.
                let _ = taste_core::presenting::end();
            }
            until
        });
        glib::spawn_future_local(async move {
            if let (Ok(until), Some(this)) = (read.await, weak.upgrade()) {
                this.apply(until);
            }
        });
    }

    /// Start the mode, until `until`, or end it (`None`).
    fn set(self: &Rc<Self>, until: Option<Until>) {
        self.widget.popdown();
        let weak = Rc::downgrade(self);
        let write = crate::runtime::runtime().spawn_blocking(move || match until {
            Some(until) => taste_core::presenting::start(until),
            None => taste_core::presenting::end(),
        });
        glib::spawn_future_local(async move {
            let result = write.await;
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(Ok(())) => this.apply(until),
                Ok(Err(e)) => tracing::warn!("presentation mode: {e:#}"),
                Err(e) => tracing::warn!("presentation mode: {e}"),
            }
        });
    }

    /// Show `until` and hold, or let go of, what the mode holds here.
    fn apply(self: &Rc<Self>, until: Option<Until>) {
        if self.posed.get() {
            return;
        }
        if self.until.replace(until) == until && self.content.first_child().is_some() {
            return;
        }
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        if let Some(Until(at)) = until {
            let now = glib::DateTime::now_utc().map_or(0, |now| now.to_unix().max(0) as u64);
            let weak = Rc::downgrade(self);
            let timer = glib::timeout_add_seconds_local_once(
                (at.saturating_sub(now)).min(u64::from(u32::MAX)) as u32 + 1,
                move || {
                    if let Some(this) = weak.upgrade() {
                        this.timer.borrow_mut().take();
                        this.reread();
                    }
                },
            );
            *self.timer.borrow_mut() = Some(timer);
        }
        self.inhibit(until.is_some());
        self.draw();
    }

    /// Hold the desktop's inhibitor — no suspend, no idle blanking — while
    /// the mode is on.
    fn inhibit(&self, on: bool) {
        let Some(application) = self.application.upgrade() else {
            return;
        };
        match (on, self.cookie.get()) {
            (true, None) => {
                // The window is a hint some desktops take; this one's, once
                // the button is in it.
                let window = self.widget.root().and_downcast::<gtk::Window>();
                let cookie = application.inhibit(
                    window.as_ref(),
                    gtk::ApplicationInhibitFlags::SUSPEND | gtk::ApplicationInhibitFlags::IDLE,
                    Some("Presenting"),
                );
                // 0 is the desktop declining, which is said: a presentation
                // whose machine can still sleep is one the person thinks is
                // covered.
                self.cookie.set((cookie != 0).then_some(cookie));
                taste_core::app_log::push(
                    if cookie == 0 { "warn" } else { "info" },
                    "presentation",
                    if cookie == 0 {
                        "presentation mode is on, but the desktop declined to keep the machine \
                         awake"
                    } else {
                        "presentation mode is on: the machine is kept awake"
                    },
                );
            }
            (false, Some(cookie)) => {
                application.uninhibit(cookie);
                self.cookie.set(None);
                taste_core::app_log::push(
                    "info",
                    "presentation",
                    "presentation mode ended: the machine may sleep again",
                );
            }
            _ => {}
        }
    }

    /// The face and the popover, for the mode as it stands.
    fn draw(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        let until = self.until.get();
        let heading = gtk::Label::builder()
            .label(match until {
                Some(until) => format!("Presenting {}", until.words()),
                None => "Presentation Mode".to_string(),
            })
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
        let what = gtk::Label::builder()
            .label(WHAT_IT_DOES)
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(36)
            .build();
        let head = gtk::Box::new(gtk::Orientation::Vertical, 4);
        head.append(&heading);
        head.append(&what);
        self.content.append(&head);
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 0);
        match until {
            Some(_) => {
                let end = crate::cloud_form::menu_item(
                    "media-playback-stop-symbolic",
                    "End Presentation Mode",
                    "Let the machine sleep again, and the IDE get on with what it held",
                );
                let weak = Rc::downgrade(self);
                end.connect_clicked(move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.set(None);
                    }
                });
                actions.append(&end);
            }
            None => {
                let now = glib::DateTime::now_utc().map_or(0, |now| now.to_unix().max(0) as u64);
                for (minutes, label) in LENGTHS {
                    let item = crate::cloud_form::menu_item(
                        "alarm-symbolic",
                        label,
                        "Presentation mode ends on its own then",
                    );
                    let weak = Rc::downgrade(self);
                    item.connect_clicked(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.set(Some(Until(now_or(now) + minutes * 60)));
                        }
                    });
                    actions.append(&item);
                }
            }
        }
        self.content.append(&actions);
        match until {
            Some(until) => {
                self.widget.add_css_class("accent");
                self.widget
                    .set_tooltip_text(Some(&format!("Presenting {}", until.words())));
            }
            None => {
                self.widget.remove_css_class("accent");
                self.widget.set_tooltip_text(Some("Presentation mode"));
            }
        }
    }

    /// TASTE_PROBE_CHECK only: `TASTE_PROBE_PRESENTING=on` (an hour) or
    /// `off`, posed without the file or the inhibitor, with the popover
    /// open — a probe's machine is not presenting anything.
    pub fn pose_for_probe(self: &Rc<Self>, on: bool) {
        let now = glib::DateTime::now_utc().map_or(0, |now| now.to_unix().max(0) as u64);
        self.posed.set(true);
        self.until.set(on.then_some(Until(now + 3600)));
        self.draw();
        let widget = self.widget.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
            widget.popup();
        });
    }
}

/// The time a length is counted from: when the popover was drawn, or now
/// if that has gone stale (a popover left open over lunch).
fn now_or(drawn: u64) -> u64 {
    let now = glib::DateTime::now_utc().map_or(drawn, |now| now.to_unix().max(0) as u64);
    now.max(drawn)
}
