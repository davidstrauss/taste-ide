//! Gadget mode: below a breakpoint, the window IS the monitor.
//!
//! ENVIRONMENTS.md → "Gadget mode: the window is the monitor". Supervising
//! a busy fleet should not require keeping a full IDE focused. Shrink the
//! window into a corner and the four panes give way to one compact card —
//! per-chat busy indicators, environment states, the fleet's fuel gauge,
//! the review count. Stretch it back and it is the IDE again, with nothing
//! rearranged: one window, and the layout commitment intact.
//!
//! Three things this deliberately is not:
//!
//! - **Not a second window.** A floating always-on-top gadget is ruled out
//!   in the design: Wayland does not grant apps keep-above, and panes
//!   never float. The window we already have gets small instead.
//! - **Not a model.** Everything rendered here arrives as one
//!   [`taste_fleetlink::Snapshot`] — the same struct the varlink service
//!   publishes, built by `fleet::snapshot` from the same rows the console
//!   renders. The card owns no facts and asks nobody anything.
//! - **Not a control panel.** Rows click through and that is all they do.
//!   Starting containers and destroying environments live in the fleet
//!   view, where there is room to say what would be lost.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use taste_core::environment::EnvironmentId;
use taste_fleetlink::Snapshot;

/// The width, in scalable pixels, at or below which the panes give way.
///
/// Picked to be **unreachable by accident**. The four panes have real
/// minimum widths (`TASTE_MEASURE_MIN=1` prints them) and stop being
/// useful long before they stop fitting, so the number could be much
/// larger — but it also has to sit below every width the desktop hands out
/// on its own. GNOME's keyboard tiling gives a window half the workspace,
/// and the narrowest display anyone runs this on is 1280 logical pixels
/// wide, so a tiled IDE is 640sp; quarter-tiling on a 1280 display is
/// 640×360. 520sp is clear of all of them.
///
/// The consequence is the intended one: gadget mode is entered by
/// dragging a corner, on purpose, and never by tiling an IDE to the side
/// of a browser. In `sp` rather than px so it means the same thing on a
/// HiDPI display.
pub const GADGET_MAX_WIDTH_SP: f64 = 520.0;

/// The size the window returns to when a row is clicked through, if it is
/// smaller than that. Not the IDE's default size — a user who had it at
/// 1100×700 before shrinking it should not be handed 1440×900 — but the
/// floor that makes four panes legible again.
pub const RESTORED_WIDTH: i32 = 1100;
pub const RESTORED_HEIGHT: i32 = 720;

/// Where a click on the card wants to go. Both environment hooks take the
/// environment, never a chat id: the card knows which world a row is, and
/// the chat strip is the authority on which conversation works in it —
/// the same one-way lookup the fleet view uses.
pub type OpenChatHook = Rc<dyn Fn(&EnvironmentId)>;
pub type OpenEnvironmentHook = Rc<dyn Fn(EnvironmentId)>;
pub type OpenIssuesHook = Rc<dyn Fn()>;

pub struct Gadget {
    pub widget: gtk::Box,
    heading: gtk::Label,
    subheading: gtk::Label,
    gauge_label: gtk::Label,
    rows: gtk::ListBox,
    review_row: adw::ActionRow,
    issues_row: adw::ActionRow,
    /// The last snapshot handed in, rendered or not.
    latest: RefCell<Snapshot>,
    /// What is on the widgets right now — the guard against rebuilding
    /// rows for an event that changed nothing.
    rendered: RefCell<Option<Snapshot>>,
    /// False while the breakpoint is not applied. The fleet republishes on
    /// every environment event and the card is not on screen most of the
    /// time; rebuilding hidden rows would be work done purely to throw
    /// away.
    live: Cell<bool>,
    open_chat: RefCell<Option<OpenChatHook>>,
    open_environment: RefCell<Option<OpenEnvironmentHook>>,
    open_issues: RefCell<Option<OpenIssuesHook>>,
}

impl Gadget {
    pub fn new() -> Rc<Self> {
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build();
        let subheading = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        title_box.append(&heading);
        title_box.append(&subheading);
        title_box.set_hexpand(true);

        // The fuel gauge. Deliberately a number and not a percentage: the
        // proxy records spend and does not enforce it, and the API exposes
        // no subscription ceiling, so a bar filling toward a limit would
        // be a fiction. The per-row bars below show which environment is
        // burning it, which is the question a monitor can actually answer.
        // Full weight, not caption: this is the other half of the header,
        // and a fuel gauge whispered in 9pt next to a bold workspace name
        // reads as a footnote rather than as the number you shrank the
        // window to watch.
        let gauge_label = gtk::Label::builder()
            .xalign(1.0)
            .css_classes(["numeric", "heading"])
            .build();
        let gauge_caption = gtk::Label::builder()
            .xalign(1.0)
            .label("subscription spend")
            .css_classes(["dim-label", "caption"])
            .build();
        let gauge_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        gauge_box.append(&gauge_label);
        gauge_box.append(&gauge_caption);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.set_margin_top(12);
        header.set_margin_bottom(6);
        header.set_margin_start(12);
        header.set_margin_end(12);
        header.append(&title_box);
        header.append(&gauge_box);

        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        // Environments waiting on a judgment. Not a place to go — the
        // rows above ARE those environments — so it is a count and not a
        // link: activating it would land on whichever of them, and
        // "whichever" is not an answer a monitor should give.
        let review_row = adw::ActionRow::builder().title("Ready for review").build();
        review_row.set_title_lines(1);
        review_row.set_subtitle_lines(1);
        review_row.add_prefix(&gtk::Image::from_icon_name("view-reveal-symbolic"));
        // The queue's sibling: two things waiting for the user, said the
        // same way. Work handed back, and work written down.
        let issues_row = adw::ActionRow::builder()
            .title("Issues")
            .activatable(true)
            .build();
        issues_row.set_title_lines(1);
        issues_row.set_subtitle_lines(1);
        issues_row.add_prefix(&gtk::Image::from_icon_name("checkbox-symbolic"));
        let footer_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .build();
        footer_list.append(&review_row);
        footer_list.append(&issues_row);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_bottom(12);
        content.append(&rows);
        content.append(&footer_list);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&header);
        widget.append(&scroller);

        let gadget = Rc::new(Self {
            widget,
            heading,
            subheading,
            gauge_label,
            rows,
            review_row: review_row.clone(),
            issues_row: issues_row.clone(),
            latest: RefCell::new(Snapshot::default()),
            rendered: RefCell::new(None),
            live: Cell::new(false),
            open_chat: RefCell::new(None),
            open_environment: RefCell::new(None),
            open_issues: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&gadget);
            issues_row.connect_activated(move |_| {
                if let Some(gadget) = weak.upgrade() {
                    let hook = gadget.open_issues.borrow().clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
            });
        }
        gadget
    }

    pub fn set_hooks(
        &self,
        open_chat: OpenChatHook,
        open_environment: OpenEnvironmentHook,
        open_issues: OpenIssuesHook,
    ) {
        *self.open_chat.borrow_mut() = Some(open_chat);
        *self.open_environment.borrow_mut() = Some(open_environment);
        *self.open_issues.borrow_mut() = Some(open_issues);
    }

    /// The fleet moved. Cheap when the card is not on screen — which is
    /// most of the time — because the snapshot is kept and the widgets are
    /// not touched until the breakpoint brings the card back.
    pub fn publish(&self, snapshot: Snapshot) {
        *self.latest.borrow_mut() = snapshot;
        if self.live.get() {
            self.render();
        }
    }

    /// The breakpoint applied or unapplied. Rendering catches up here, so
    /// stretching the window back and shrinking it again shows the fleet
    /// as it is now rather than as it was when the card was last visible.
    pub fn set_live(&self, live: bool) {
        self.live.set(live);
        if live {
            self.render();
        }
    }

    fn render(&self) {
        let snapshot = self.latest.borrow().clone();
        if self.rendered.borrow().as_ref() == Some(&snapshot) {
            return; // nothing moved
        }

        // The header bar already says which workspace this is, and below
        // the breakpoint it says "fleet monitor" too. Repeating the folder
        // name here would spend the card's most prominent line saying
        // something the window title says two centimetres above it, so the
        // card leads with the fact the user shrank the window FOR.
        let busy = snapshot.busy();
        let environments = snapshot.rows.len();
        self.heading.set_label(&match environments {
            1 => format!("{} of 1 environment up", snapshot.running()),
            n => format!("{} of {n} environments up", snapshot.running()),
        });
        self.heading.set_tooltip_text(Some(&snapshot.workspace));
        self.subheading.set_label(&match busy {
            0 => "no turns in flight".to_string(),
            1 => "1 chat working".to_string(),
            n => format!("{n} chats working"),
        });

        let spend = snapshot.spend();
        self.gauge_label.set_label(&if spend.is_zero() {
            "—".to_string()
        } else {
            format!(
                "{} in / {} out",
                compact(spend.input_tokens),
                compact(spend.output_tokens)
            )
        });
        self.gauge_label.set_tooltip_text(Some(&format!(
            "{} requests through the IDE's auth proxy, across every \
             environment.\nThe same subscription you spend talking to an \
             agent yourself — there is no quota figure to show a \
             percentage of.",
            spend.requests
        )));

        // The gauge's denominator, for the per-row bars: the biggest
        // spender in the fleet. Relative, and honestly so.
        let peak = snapshot
            .rows
            .iter()
            .map(|row| row.spend.input_tokens + row.spend.output_tokens)
            .max()
            .unwrap_or(0);

        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        for row in &snapshot.rows {
            self.rows.append(&self.build_row(row, peak));
        }

        let waiting = snapshot.flagged_for_review();
        self.review_row.set_subtitle(&match waiting {
            0 => "nothing waiting on you".to_string(),
            1 => "1 environment is done and waiting".to_string(),
            n => format!("{n} environments are done and waiting"),
        });
        self.review_row.set_sensitive(waiting > 0);

        self.issues_row.set_subtitle(&match snapshot.open_issues {
            0 => "nothing on the queue".to_string(),
            1 => "1 open".to_string(),
            n => format!("{n} open"),
        });

        *self.rendered.borrow_mut() = Some(snapshot);
    }

    fn build_row(&self, row: &taste_fleetlink::Row, peak: u64) -> adw::ActionRow {
        let action = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&row.name))
            .activatable(true)
            .build();

        // Container state, as the one glyph the fleet tab already uses, so
        // the two surfaces read the same.
        let icon = gtk::Image::from_icon_name(match (row.mode.as_str(), row.pending_rebuild) {
            ("container", true) => "taste-container-warn",
            ("container", false) => "taste-container-on",
            _ if row.state == "failed" => "taste-container-warn",
            _ => "taste-container-off",
        });
        icon.set_tooltip_text(Some(&row.detail));
        action.add_prefix(&icon);

        let mut subtitle = row.detail.clone();
        if let Some(chat) = &row.chat {
            subtitle.push_str(&format!(" · {}", chat.label));
        }
        if row.shells > 0 {
            subtitle.push_str(&format!(" · {} live", row.shells));
        }
        if row.published > 0 {
            subtitle.push_str(&format!(" · {} published", row.published));
        }
        action.set_subtitle(&gtk::glib::markup_escape_text(&subtitle));
        // One line each, ellipsized. A gadget-width row must SHRINK, and
        // an AdwActionRow that wraps instead demands the full width of its
        // longest subtitle — which is how a 400px card ends up clipped
        // with its gauge off the right-hand edge (observed, then fixed).
        action.set_title_lines(1);
        action.set_subtitle_lines(1);

        // Per-chat busy: AdwTabPage's spinner is what says this in the
        // strip, and a spinner is what says it here.
        if row.chat.as_ref().is_some_and(|chat| chat.busy) {
            let spinner = gtk::Spinner::new();
            spinner.start();
            spinner.set_tooltip_text(Some("a turn is in flight"));
            action.add_suffix(&spinner);
        }

        // This environment's share of the fleet's spend. An environment
        // that has spent nothing gets no bar at all — an empty gauge says
        // "nothing" more loudly, and less clearly, than the absence does.
        let total = row.spend.input_tokens + row.spend.output_tokens;
        if peak > 0 && total > 0 {
            let bar = gtk::LevelBar::builder()
                .min_value(0.0)
                .max_value(peak as f64)
                .value(total as f64)
                .mode(gtk::LevelBarMode::Continuous)
                .valign(gtk::Align::Center)
                .width_request(56)
                .tooltip_text(format!(
                    "{} in / {} out over {} requests",
                    compact(row.spend.input_tokens),
                    compact(row.spend.output_tokens),
                    row.spend.requests
                ))
                .build();
            action.add_suffix(&bar);
        }

        // Rows click through. A row with a chat lands on that chat —
        // that is what the user was watching; a row without one lands on
        // the environment.
        let has_chat = row.chat.is_some();
        let env = EnvironmentId::parse(&row.environment).ok();
        let open_chat = self.open_chat.borrow().clone();
        let open_environment = self.open_environment.borrow().clone();
        action.connect_activated(move |_| {
            let Some(env) = &env else { return };
            match (has_chat, &open_chat, &open_environment) {
                (true, Some(hook), _) => hook(env),
                (_, _, Some(hook)) => hook(env.clone()),
                _ => {}
            }
        });
        action
    }
}

/// Token counts, compactly — the same scale the fleet view uses, so one
/// number does not read as two different sizes on two surfaces.
fn compact(value: u64) -> String {
    match value {
        0..=9_999 => value.to_string(),
        10_000..=999_999 => format!("{:.0}k", value as f64 / 1000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The breakpoint has to be unreachable by tiling, or an IDE snapped
    /// to half a screen turns into a monitor and the user loses their
    /// panes to a gesture they meant as "put this beside my browser".
    #[test]
    fn the_breakpoint_is_below_every_size_the_desktop_hands_out() {
        // Half and quarter tiling on the narrowest display this targets,
        // and on the common ones.
        for display_width in [1280.0, 1366.0, 1440.0, 1920.0, 2560.0] {
            assert!(
                GADGET_MAX_WIDTH_SP < display_width / 2.0,
                "half-tiling a {display_width}px display would trip the gadget"
            );
        }
        // Quarter-tiling is half-tiling's width, so the loop above
        // already covers it. What is left to check is that the size a
        // click-through restores actually clears the breakpoint — a
        // restore that landed the user back in the card would be a loop.
        assert!(f64::from(RESTORED_WIDTH) > GADGET_MAX_WIDTH_SP * 2.0);
    }
}
