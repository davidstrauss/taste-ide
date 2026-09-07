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
    /// What the container itself writes: its main process's stdout and
    /// stderr, followed with `podman logs` while it runs. The devcontainer
    /// spec has no notion of a log to discover; this stream is the one
    /// thing a container formally has.
    Container,
    /// The IDE's own log: GLib/GTK warnings and the app's tracing, what
    /// `ide_app_log` serves to agents.
    Ide,
}

impl LogKind {
    /// Every log, in the order the tree lists them.
    pub const ALL: [LogKind; 3] = [LogKind::Environment, LogKind::Container, LogKind::Ide];

    /// The row's title in the tree and the tab's.
    pub fn title(self) -> &'static str {
        match self {
            LogKind::Environment => "Environment",
            LogKind::Container => "Container",
            LogKind::Ide => "IDE",
        }
    }

    /// One line under the title, saying what the log carries.
    pub fn subtitle(self) -> &'static str {
        match self {
            LogKind::Environment => "Container build and lifecycle",
            LogKind::Container => "What the container itself writes",
            LogKind::Ide => "The app's own warnings and tracing",
        }
    }

    /// The stable part of the tab's key (`log:<env>/<slug>`).
    pub fn slug(self) -> &'static str {
        match self {
            LogKind::Environment => "environment",
            LogKind::Container => "container",
            LogKind::Ide => "ide",
        }
    }

    /// Whether this log is one per environment (the tree shows the
    /// selected environment's) or one for the whole IDE.
    pub fn per_environment(self) -> bool {
        !matches!(self, LogKind::Ide)
    }
}

/// How much each log has been saying lately — one series per environment
/// and log, in the backlog's own buckets (`taste_core::activity`), so the
/// Logs rows carry the same sparkline the environment rows do (David,
/// 2026-09-06: "sparklines on the logs for activity"). Counts of lines,
/// never the lines.
#[derive(Default)]
pub struct LogActivity {
    series: RefCell<std::collections::HashMap<(taste_core::environment::EnvironmentId, LogKind), taste_core::activity::Series>>,
    epoch: std::cell::OnceCell<std::time::Instant>,
}

impl LogActivity {
    fn bucket(&self) -> u64 {
        let epoch = *self.epoch.get_or_init(std::time::Instant::now);
        epoch.elapsed().as_secs() / taste_core::activity::BUCKET.as_secs()
    }

    /// `lines` more lines arrived in `env`'s `kind` log, now. The IDE log
    /// is keyed under the primary, since it is one for the window.
    pub fn record(&self, env: &taste_core::environment::EnvironmentId, kind: LogKind, lines: usize) {
        if lines == 0 {
            return;
        }
        let bucket = self.bucket();
        let mut series = self.series.borrow_mut();
        let entry = series
            .entry((env.clone(), kind))
            .or_insert_with(|| taste_core::activity::Series::new(bucket));
        for _ in 0..lines.min(u16::MAX as usize) {
            entry.record(bucket);
        }
    }

    /// The last five minutes of one log, oldest first; all zeros for a log
    /// nothing has been written to.
    pub fn samples(
        &self,
        env: &taste_core::environment::EnvironmentId,
        kind: LogKind,
    ) -> [taste_core::activity::Count; taste_core::activity::BUCKETS] {
        let bucket = self.bucket();
        self.series
            .borrow()
            .get(&(env.clone(), kind))
            .map(|series| series.samples(bucket))
            .unwrap_or([0; taste_core::activity::BUCKETS])
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

    /// Everything on screen, for the editor's listing to search.
    pub fn text(&self) -> String {
        let buffer = self.view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Select the query's first match on a line and scroll to it — a hit
    /// chosen in the listing. Following stops, or the next line arriving
    /// would scroll the hit back out of view.
    pub fn highlight_line(&self, line: u32, query: &taste_core::search::Query) {
        self.set_follow(false);
        let buffer = self.view.buffer();
        let Some(mut start) = buffer.iter_at_line(line.saturating_sub(1) as i32) else {
            return;
        };
        let mut end = start;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        let text = buffer.text(&start, &end, false);
        if let Some(&(from, to)) = query.ranges(&text).first() {
            let chars_before = text[..from].chars().count() as i32;
            let chars_in = text[from..to].chars().count() as i32;
            start.set_line_offset(chars_before);
            end = start;
            end.forward_chars(chars_in);
        }
        buffer.select_range(&start, &end);
        self.view.scroll_to_iter(&mut start, 0.1, true, 0.0, 0.4);
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
