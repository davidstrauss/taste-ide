//! A log as a document: the page a Logs row in the file tree opens in the
//! editor's strip.
//!
//! David, 2026-09-06: logs are listed in the left bar beside the files and
//! "open like files (but started at the bottom and tailing by default)".
//! So this is a read-only text page that opens scrolled to its end and
//! follows new lines as they arrive — until the reader scrolls up, which
//! is the gesture for "let me read this part", at which point new lines
//! collect below and a floating button says so (the chat's own
//! stick-to-bottom convention). Follow is also a mode in the editor's
//! display-mode menu, the same menu that switches a Markdown file between
//! Edit and Preview.
//!
//! The lines come from wherever the log lives — the environment's
//! supervisor ring, the IDE's own `app_log` — through
//! [`LogPage::append`]; this page owns none of the sources and does no IO.
//! It is capped, and trims from the top: a build that logs forever must not
//! grow a text buffer forever.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// Which log a page shows. The set is small on purpose: what the tree
/// lists is what exists, and it is the same for every environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogKind {
    /// The environment's build and lifecycle stream — the supervisor's
    /// ring, the same lines the console's environment tab shows.
    Environment,
    /// The IDE's own log: GLib/GTK warnings and the app's tracing, what
    /// `ide_app_log` serves to agents.
    Ide,
}

impl LogKind {
    /// The row's title in the tree and the tab's.
    pub fn title(self) -> &'static str {
        match self {
            LogKind::Environment => "Environment",
            LogKind::Ide => "IDE",
        }
    }

    /// One line under the title, saying what the log carries.
    pub fn subtitle(self) -> &'static str {
        match self {
            LogKind::Environment => "Container build and lifecycle",
            LogKind::Ide => "The app's own warnings and tracing",
        }
    }

    /// The stable part of the tab's key (`log:<env>/<slug>`).
    pub fn slug(self) -> &'static str {
        match self {
            LogKind::Environment => "environment",
            LogKind::Ide => "ide",
        }
    }

    /// Whether this log is one per environment (the tree shows the
    /// selected environment's) or one for the whole IDE.
    pub fn per_environment(self) -> bool {
        matches!(self, LogKind::Environment)
    }
}

/// The glyph every log surface wears: the tree section, the rows, the tab.
pub const LOG_ICON: &str = "format-justify-fill-symbolic";

/// Lines kept on screen; older ones are trimmed from the top.
const MAX_LINES: i32 = 5000;

pub struct LogPage {
    pub widget: gtk::Widget,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    /// Following: new lines scroll into view. Cleared when the reader
    /// scrolls up, set again by the jump button, the mode menu, or
    /// scrolling back to the end by hand.
    follow: Rc<Cell<bool>>,
    /// "New lines below — jump to end", shown while not following and
    /// something has arrived.
    jump: gtk::Revealer,
    /// Set by our own scroll-to-end so the value-changed handler does not
    /// read it as the user scrolling.
    scrolling: Rc<Cell<bool>>,
    state: gtk::Label,
    on_follow_changed: RefCell<Option<Box<dyn Fn(bool)>>>,
}

impl LogPage {
    pub fn new(kind: LogKind, environment: &str, seed: &[String]) -> Rc<Self> {
        // A slim bar like the review tab's: what this is, since the tab is
        // one word and the text below could be anything.
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("review-bar");
        let icon = gtk::Image::from_icon_name(LOG_ICON);
        icon.add_css_class("dim-label");
        icon.set_pixel_size(12);
        bar.append(&icon);
        let what = gtk::Label::builder()
            .label(if kind.per_environment() {
                format!("{} · {environment}", kind.subtitle())
            } else {
                kind.subtitle().to_string()
            })
            .css_classes(["caption-heading"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .build();
        bar.append(&what);
        let state = gtk::Label::builder()
            .label("following")
            .css_classes(["caption", "dim-label"])
            .build();
        bar.append(&state);

        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            // Folds, never scrolls sideways: a log line is as long as
            // somebody's path, and a horizontal scrollbar hides the end of
            // exactly the lines that matter.
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .right_margin(8)
            .top_margin(6)
            .bottom_margin(6)
            .build();
        if !seed.is_empty() {
            view.buffer().set_text(&format!("{}\n", seed.join("\n")));
        }
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hexpand(true)
            .vexpand(true)
            .build();

        let jump_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("go-bottom-symbolic")
                    .label("New lines below — jump to end")
                    .build(),
            )
            .css_classes(["pill", "suggested-action"])
            .halign(gtk::Align::Center)
            .margin_bottom(12)
            .build();
        let jump = gtk::Revealer::builder()
            .child(&jump_button)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .valign(gtk::Align::End)
            .halign(gtk::Align::Center)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroller));
        overlay.add_overlay(&jump);
        overlay.set_vexpand(true);

        let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inner.append(&bar);
        inner.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        inner.append(&overlay);
        // Its width is the editor's to give, never this text's to demand:
        // see `chat_column`. The floor is the editor pane's own.
        let widget = crate::chat_column::ChatColumn::with_widths(&inner, 200, 600);

        let page = Rc::new(Self {
            widget: widget.upcast(),
            view,
            scroller,
            follow: Rc::new(Cell::new(true)),
            jump,
            scrolling: Rc::new(Cell::new(false)),
            state,
            on_follow_changed: RefCell::new(None),
        });

        // Scrolling up leaves follow; scrolling back to the end resumes it.
        {
            let weak = Rc::downgrade(&page);
            let adjustment = page.scroller.vadjustment();
            adjustment.connect_value_changed(move |adjustment| {
                let Some(page) = weak.upgrade() else { return };
                if page.scrolling.get() {
                    return;
                }
                let at_end = adjustment.value() + adjustment.page_size()
                    >= adjustment.upper() - 2.0;
                if at_end != page.follow.get() {
                    page.set_follow(at_end);
                }
            });
        }
        {
            let weak = Rc::downgrade(&page);
            jump_button.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.set_follow(true);
                }
            });
        }
        // Opened at the end, once the view has a size to scroll within.
        let weak = Rc::downgrade(&page);
        glib::idle_add_local_once(move || {
            if let Some(page) = weak.upgrade() {
                page.scroll_to_end();
            }
        });
        page
    }

    /// Append lines, trimming the oldest past the cap. Scrolls when
    /// following; otherwise raises the jump button.
    pub fn append(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let buffer = self.view.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, &format!("{}\n", lines.join("\n")));
        let extra = buffer.line_count() - MAX_LINES;
        if extra > 0 {
            let mut start = buffer.start_iter();
            if let Some(mut cut) = buffer.iter_at_line(extra) {
                buffer.delete(&mut start, &mut cut);
            }
        }
        if self.follow.get() {
            self.scroll_to_end();
        } else {
            self.jump.set_reveal_child(true);
        }
    }

    pub fn is_following(&self) -> bool {
        self.follow.get()
    }

    /// Turn following on (scrolls to the end now) or off (the view stays
    /// where it is and new lines collect below).
    pub fn set_follow(&self, follow: bool) {
        if self.follow.replace(follow) == follow && !follow {
            return;
        }
        self.state.set_label(if follow { "following" } else { "paused" });
        if follow {
            self.jump.set_reveal_child(false);
            self.scroll_to_end();
        }
        if let Some(hook) = self.on_follow_changed.borrow().as_ref() {
            hook(follow);
        }
    }

    /// The editor's mode menu mirrors the follow state; this is how it
    /// hears about a change the reader made by scrolling.
    pub fn set_on_follow_changed(&self, hook: impl Fn(bool) + 'static) {
        *self.on_follow_changed.borrow_mut() = Some(Box::new(hook));
    }

    fn scroll_to_end(&self) {
        // After allocation: a value set on an adjustment that has not been
        // sized for the new text is clamped to the old extent.
        let scrolling = self.scrolling.clone();
        let adjustment = self.scroller.vadjustment();
        scrolling.set(true);
        glib::idle_add_local_once(move || {
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
            scrolling.set(false);
        });
    }
}
