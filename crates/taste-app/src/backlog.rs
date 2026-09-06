//! The backlog: one list for the work, and the one place the panes are
//! aimed from.
//!
//! `docs/spikes/issue-is-the-environment.md`: an environment is an issue in
//! progress, so the flank carries one list, not two. The first row is the
//! user's own checkout ("Yours"), pinned. Every other row is an issue, in
//! the order the user keeps them — and an issue that has been *started*
//! has an environment, which the row shows the way the Environments panel
//! used to: a traffic light for what the container is doing, a sparkline
//! for the last five minutes, an amber mark when its chat is waiting on
//! the user, an accent rail when it is flagged for review. Selecting such
//! a row aims the panes at it; the selection IS the aim, and the panel is
//! still the single namer of the selected environment.
//!
//! Rows sort by what they are: the ones with an environment first, then
//! the queue in the user's order, then the resolved ones. Reordering —
//! drag, or the row's own menu — is a gesture on the queue, so it moves a
//! row among its own kind and writes the store position of the neighbour
//! it landed against; a row's stored place among rows it is never drawn
//! beside is not something the user can see, so it is not something the
//! menu offers to change.
//!
//! Sizing: the list shows up to `VISIBLE_ROWS` and scrolls past that, and
//! grows a type-to-filter entry when it outgrows reading. In gadget mode
//! (`set_filling`) it fills the window instead, and a floating "back to
//! top" button appears once the list is scrolled more than a page — the
//! active rows are at the top, and that is where the eye wants to return.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use taste_core::activity::{Activity, BUCKETS};
use taste_core::environment::EnvironmentId;
use taste_core::quota::{describe_age, describe_countdown, QuotaSnapshot};
use taste_core::work::{work_state, Outcome, Runtime, WorkState};
use taste_devcontainer::SupervisorState;
use taste_git::{Issue, IssueMove, IssueState};

use crate::fleet::{FleetRow, Light, ReviewMark};
use crate::sparkline::Sparkline;

/// What the primary row is called. Not the workspace's name: the panel
/// answers "whose checkout is this", and the only honest answer for the
/// row that is not an issue is the user's.
pub const PRIMARY_TITLE: &str = "Yours";

/// Rows the list shows before it scrolls. Seven where the two panels it
/// replaced showed six and five: the flank has one list to give room to
/// now, and seven is still a glance.
pub const VISIBLE_ROWS: i32 = 7;

/// Past this many rows the type-to-filter entry appears. The same number
/// as the rows in view: a list that fits needs no filter, a list that
/// scrolls does.
pub const FILTER_THRESHOLD: usize = VISIBLE_ROWS as usize;

/// One row: the 26px `.backlog-list > row` plus 2px of margin either side.
const ROW_HEIGHT: i32 = 30;

const TICK: Duration = Duration::from_secs(1);

/// The name a fleet row goes by everywhere the user reads one: the primary
/// is "Yours", every other environment is its issue's title.
pub fn title_of(row: &FleetRow) -> String {
    if row.primary {
        PRIMARY_TITLE.to_string()
    } else {
        row.name.clone()
    }
}

pub(crate) fn quota_tooltip(snapshot: &QuotaSnapshot, now: std::time::SystemTime) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let Some(refusal) = snapshot.current_exhaustion(now) {
        let reopens = refusal
            .until
            .and_then(|until| until.duration_since(now).ok())
            .map(|left| format!(" — reopens {}", describe_countdown(left)))
            .unwrap_or_default();
        lines.push(format!("Out of quota{reopens}"));
        if let Some(message) = refusal.message.as_deref() {
            lines.push(message.to_string());
        }
    }

    for (name, plan) in [("Session", &snapshot.session), ("Weekly", &snapshot.weekly)] {
        let Some(used) = plan.used() else { continue };
        let resets = plan
            .resets_in(now)
            .map(|left| format!(", resets {}", describe_countdown(left)))
            .unwrap_or_default();
        lines.push(format!("{name} window {:.0}% used{resets}", used * 100.0));
    }
    if lines.is_empty() {
        if let Some(headline) = snapshot.headline(now) {
            let resets = headline
                .resets_in
                .map(|left| format!(", resets {}", describe_countdown(left)))
                .unwrap_or_default();
            lines.push(format!(
                "API rate limit ({}) {:.0}% used{resets}\nThe plan's own windows were not reported.",
                headline.meter.label(),
                headline.used * 100.0
            ));
        }
    }

    match snapshot.age(now) {
        Some(age) => lines.push(format!(
            "Read off the last agent turn, {}.",
            describe_age(age)
        )),
        None => lines.push("Not yet observed.".into()),
    }
    lines.push("One pool: every environment here, and your own Claude use.".into());
    lines.join("\n")
}

/// The environment half of a row: present when an environment exists on
/// this machine for the issue (or for the primary row, always).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub env: EnvironmentId,
    pub primary: bool,
    pub light: Light,
    pub busy: bool,
    pub awaits_user: bool,
    pub unpublished: bool,
    pub review: ReviewMark,
    /// The panes are aimed here. Drawn as the list's selection.
    pub current: bool,
    /// The fleet row's state line, for the tooltip.
    pub detail: String,
}

/// What the row is, for sorting and for what gestures it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// The user's own checkout: one row, first, never moved.
    Primary,
    /// An issue with an environment here.
    Live,
    /// An open issue with no environment here: queued, or started on a
    /// machine that is not this one. The reorderable band.
    Open,
    /// Completed or declined.
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The issue's id, or the primary environment's.
    pub id: String,
    pub title: String,
    pub work: WorkState,
    /// Who started it, as the store records it — an identity, not an
    /// environment, since the environment is the row itself.
    pub started_by: Option<String>,
    pub updated: i64,
    pub note: Option<String>,
    pub live: Option<Live>,
}

impl Row {
    pub fn group(&self) -> Group {
        match &self.live {
            Some(live) if live.primary => Group::Primary,
            Some(_) => Group::Live,
            None if self.work.is_resolved() => Group::Resolved,
            None => Group::Open,
        }
    }

    pub fn is_issue(&self) -> bool {
        self.group() != Group::Primary
    }

    /// Only the queue is reordered: an environment's place is its state's,
    /// and history keeps the order it happened in.
    pub fn reorderable(&self) -> bool {
        self.group() == Group::Open
    }

    pub fn tooltip(&self) -> String {
        let mut text = match &self.live {
            Some(live) if live.primary => {
                format!("{PRIMARY_TITLE} — your own checkout\n{}", live.detail)
            }
            Some(live) => format!(
                "{} — {}\nIts own clone and devcontainer, read-only to you\n{}",
                self.id, self.title, live.detail
            ),
            None => format!(
                "{} — {}\nLast changed {}.",
                self.id,
                self.title,
                crate::filetree::relative_age(self.updated)
            ),
        };
        if let Some(live) = &self.live {
            if live.awaits_user {
                text.push_str("\nIts chat is waiting for an answer from you.");
            } else if live.busy {
                text.push_str("\nIts chat is working now.");
            }
            match live.review {
                ReviewMark::Flagged => text.push_str(
                    "\nIt says it is done and is waiting for your review. Its container \
                     was stopped because nothing is left to run in it.",
                ),
                ReviewMark::Settled => {
                    text.push_str("\nYou have ruled on this one — it is safe to destroy.")
                }
                ReviewMark::None => {}
            }
            if !live.primary {
                text.push_str(&format!(
                    "\nLast changed {}.",
                    crate::filetree::relative_age(self.updated)
                ));
            }
        } else if let Some(who) = self.started_by.as_deref() {
            if !self.work.is_resolved() {
                text.push_str(&format!(
                    "\nStarted by {who}; this machine has no environment for it."
                ));
            }
        }
        text
    }

    /// The glyph's tooltip, on rows that have a glyph rather than a light.
    pub fn state_tooltip(&self) -> String {
        match self.work {
            WorkState::Queued => "Queued — written down, and nobody has started it.".to_string(),
            WorkState::Completed => "Completed — its work is merged.".to_string(),
            WorkState::Declined => match &self.note {
                Some(note) => format!("Declined — {note}"),
                None => "Declined — it will not be done. The record stays.".to_string(),
            },
            _ => match self.started_by.as_deref() {
                Some(who) => {
                    format!("Started by {who} — this machine has no environment for it.")
                }
                None => "Started — this machine has no environment for it.".to_string(),
            },
        }
    }
}

fn decline_note(issue: &Issue) -> Option<String> {
    let note = issue
        .comments
        .iter()
        .rev()
        .find_map(|comment| comment.body.trim().strip_prefix("Declined:"))?
        .lines()
        .next()?
        .trim();
    (!note.is_empty()).then(|| note.to_string())
}

/// The runtime half of `work_state`, read off the fleet row the way the
/// light is: the supervisor says up, building, broken or off, and the
/// chat says whether "up" is stopped on a person.
fn runtime_of(row: &FleetRow) -> Runtime {
    match row.state {
        SupervisorState::Building | SupervisorState::Starting => Runtime::Starting,
        SupervisorState::Running { .. } => Runtime::Running {
            waiting: row.awaits_user() || row.pending_rebuild,
        },
        SupervisorState::Failed { .. } => Runtime::Failed,
        SupervisorState::NoConfig | SupervisorState::ConfigDetected | SupervisorState::Stopped => {
            Runtime::Off
        }
    }
}

fn is_current(env: &EnvironmentId, current: Option<&EnvironmentId>) -> bool {
    match current {
        Some(current) => env == current,
        None => env.is_primary(),
    }
}

fn live_of(row: &FleetRow, current: Option<&EnvironmentId>) -> Live {
    Live {
        env: row.env.clone(),
        primary: row.primary,
        light: row.light(),
        busy: row.chat.as_ref().is_some_and(|chat| chat.busy),
        awaits_user: row.awaits_user(),
        unpublished: row.has_unpublished_work(),
        review: row.review_mark(),
        current: is_current(&row.env, current),
        detail: row.state_text(),
    }
}

/// The primary row, from the fleet when it has been assembled and from
/// nothing when it has not: a panel with no fleet yet still has a first
/// row, and it says so rather than pretending to a state.
fn primary_row(fleet: &[FleetRow], current: Option<&EnvironmentId>) -> Row {
    let live = match fleet.iter().find(|row| row.primary) {
        Some(row) => live_of(row, current),
        None => {
            let env = EnvironmentId::primary();
            Live {
                current: is_current(&env, current),
                env,
                primary: true,
                light: Light::Unknown,
                busy: false,
                awaits_user: false,
                unpublished: false,
                review: ReviewMark::None,
                detail: "state not known yet".to_string(),
            }
        }
    };
    Row {
        id: live.env.to_string(),
        title: PRIMARY_TITLE.to_string(),
        work: WorkState::Working,
        started_by: None,
        updated: 0,
        note: None,
        live: Some(live),
    }
}

/// The list: the primary row, then the issues by group, each group in the
/// order the store keeps. One issue, one environment — the fleet row whose
/// id is the issue's is that issue's environment, by construction.
pub fn rows(issues: &[Issue], fleet: &[FleetRow], current: Option<&EnvironmentId>) -> Vec<Row> {
    let mut out = vec![primary_row(fleet, current)];
    let mut issues: Vec<Row> = issues
        .iter()
        .map(|issue| {
            let env = fleet
                .iter()
                .find(|row| !row.primary && row.env.as_str() == issue.id);
            let outcome = match issue.state() {
                IssueState::Completed => Outcome::Completed,
                IssueState::Declined => Outcome::Declined,
                IssueState::Queued | IssueState::Started => Outcome::Open,
            };
            let (runtime, review) = match env {
                Some(row) => (runtime_of(row), row.review),
                None => (Runtime::Absent, taste_core::ReviewState::Working),
            };
            Row {
                id: issue.id.clone(),
                title: issue.title.clone(),
                work: work_state(outcome, issue.started_by.is_some(), runtime, review),
                started_by: issue.started_by.clone(),
                updated: issue.updated,
                note: decline_note(issue),
                live: env.map(|row| live_of(row, current)),
            }
        })
        .collect();
    // Stable: within a group the store's order is the order.
    issues.sort_by_key(Row::group);
    out.extend(issues);
    out
}

/// The header's count: how much is left, and how much of it is moving.
pub fn summary(rows: &[Row]) -> String {
    let issues: Vec<&Row> = rows.iter().filter(|row| row.is_issue()).collect();
    if issues.is_empty() {
        return "empty".to_string();
    }
    let open = issues.iter().filter(|row| !row.work.is_resolved()).count();
    let active = issues
        .iter()
        .filter(|row| row.group() == Group::Live && !row.work.is_resolved())
        .count();
    let done = issues
        .iter()
        .filter(|row| row.work == WorkState::Completed)
        .count();
    let declined = issues
        .iter()
        .filter(|row| row.work == WorkState::Declined)
        .count();
    let mut text = open.to_string();
    if active > 0 {
        text.push_str(&format!(" · {active} active"));
    }
    if done > 0 {
        text.push_str(&format!(" · {done} done"));
    }
    if declined > 0 {
        text.push_str(&format!(" · {declined} declined"));
    }
    text
}

/// The glyph for a row with no environment here: an empty box for the
/// queue, a ticked one for done, a struck one for declined — and a mixed
/// box for "started, but not here", which is the one state the light
/// cannot show because there is no container to read it from.
pub fn state_icon(work: WorkState) -> &'static str {
    match work {
        WorkState::Queued => "checkbox-symbolic",
        WorkState::Completed => "checkbox-checked-symbolic",
        WorkState::Declined => "action-unavailable-symbolic",
        _ => "checkbox-mixed-symbolic",
    }
}

fn state_classes(work: WorkState) -> Vec<&'static str> {
    if work.is_resolved() || work == WorkState::Queued {
        vec!["backlog-state", "dim-label"]
    } else {
        vec!["backlog-state"]
    }
}

/// The panel is tinted when the panes are aimed away from home.
pub fn away(current: Option<&EnvironmentId>) -> bool {
    current.is_some_and(|env| !env.is_primary())
}

pub fn filter_visible(count: usize) -> bool {
    count > FILTER_THRESHOLD
}

/// Type-to-filter: case-insensitive substring over the title and the id.
/// The primary row always shows — it is the way home, and a filter that
/// hid it would strand the user in a clone.
pub fn matches(row: &Row, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() || !row.is_issue() {
        return true;
    }
    row.title.to_lowercase().contains(&query) || row.id.to_lowercase().contains(&query)
}

/// Where a menu move lands in the *store*: the position of the neighbour
/// the row would pass in its own displayed group. `None` when there is no
/// such neighbour, which is also when the menu item is disabled.
pub fn move_target(
    shown: &[Row],
    stored: &[Issue],
    id: &str,
    direction: IssueMove,
) -> Option<usize> {
    let group = shown.iter().find(|row| row.id == id)?.group();
    let band: Vec<&str> = shown
        .iter()
        .filter(|row| row.reorderable() && row.group() == group)
        .map(|row| row.id.as_str())
        .collect();
    let at = band.iter().position(|row| *row == id)?;
    let neighbour = match direction {
        IssueMove::Up => at.checked_sub(1)?,
        IssueMove::Down => (at + 1 < band.len()).then_some(at + 1)?,
        IssueMove::Top => 0,
        IssueMove::Bottom => band.len() - 1,
    };
    if neighbour == at {
        return None;
    }
    stored.iter().position(|issue| issue.id == band[neighbour])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moves {
    pub up: bool,
    pub down: bool,
    pub top: bool,
    pub bottom: bool,
}

pub fn moves(index: usize, len: usize) -> Moves {
    let first = index == 0;
    let last = index + 1 >= len;
    Moves {
        up: !first,
        top: !first,
        down: !last,
        bottom: !last,
    }
}

pub fn drop_index(from: usize, onto: usize, below: bool) -> Option<usize> {
    let insert_at = if below { onto + 1 } else { onto };
    let to = if from < insert_at {
        insert_at.saturating_sub(1)
    } else {
        insert_at
    };
    (to != from).then_some(to)
}

pub type RefreshHook = Box<dyn Fn()>;
pub type ToastHook = Box<dyn Fn(String)>;
pub type SelectHook = Box<dyn Fn(EnvironmentId)>;

/// What the composer hands out when its primary action is Start: enough
/// for the window to make the issue's environment and brief its chat.
#[derive(Debug, Clone)]
pub struct StartedIssue {
    pub id: String,
    pub title: String,
    pub body: String,
}
type StartHook = Box<dyn Fn(StartedIssue)>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Composing {
    New,
    Editing(String),
}

/// One built row, kept so the tick can redraw its sparkline and the
/// probe can find it by id.
struct Listed {
    id: String,
    env: Option<EnvironmentId>,
    widget: gtk::ListBoxRow,
    sparkline: Option<Sparkline>,
}

pub struct BacklogPanel {
    pub widget: gtk::Box,
    count: gtk::Label,
    search: gtk::SearchEntry,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    composer: gtk::Box,
    composer_title: gtk::Entry,
    composer_body: gtk::TextView,
    composer_heading: gtk::Label,
    composer_submit: gtk::Button,
    composer_start: gtk::Button,
    composing: RefCell<Option<Composing>>,
    root: std::path::PathBuf,
    activity: Activity,
    issues: RefCell<Vec<Issue>>,
    fleet: RefCell<Vec<FleetRow>>,
    current: RefCell<Option<EnvironmentId>>,
    shown: RefCell<Vec<Row>>,
    listed: RefCell<Vec<Listed>>,
    confirming: RefCell<Option<String>>,
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
    writing: Cell<bool>,
    selecting: Cell<bool>,
    filling: Cell<bool>,
    /// A render asked for while a row's menu was open. The list is not
    /// rebuilt under a menu — the menu is parented to a row, and the row
    /// the user is looking at must not move — so it waits for the close.
    render_deferred: Cell<bool>,
    quota: gtk::Box,
    quota_bar: gtk::LevelBar,
    quota_snapshot: RefCell<QuotaSnapshot>,
    quota_tooltip: RefCell<String>,
    probe_activity: RefCell<BTreeMap<EnvironmentId, [u16; BUCKETS]>>,
    on_refresh: RefCell<Option<RefreshHook>>,
    on_toast: RefCell<Option<ToastHook>>,
    on_start: RefCell<Option<StartHook>>,
    on_select: RefCell<Option<SelectHook>>,
    on_tick: RefCell<Option<RefreshHook>>,
}

impl BacklogPanel {
    pub fn new(root: std::path::PathBuf, activity: Activity) -> Rc<Self> {
        let title = gtk::Label::builder()
            .label("Backlog")
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .build();
        let count = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "numeric"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let quota_bar = crate::gauge::new();
        let quota = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        quota.append(&quota_bar);
        let add = gtk::Button::builder()
            .child(
                &gtk::Image::builder()
                    .icon_name("list-add-symbolic")
                    .css_classes(["dim-label"])
                    .pixel_size(14)
                    .build(),
            )
            .css_classes(["flat", "circular", "backlog-new"])
            .tooltip_text(
                "Write a new issue. File it for later, or Start it: an environment is \
                 an issue in progress, so making one begins with writing down what it \
                 is for.",
            )
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["backlog-header"])
            .build();
        header.append(&title);
        header.append(&count);
        header.append(&quota);
        header.append(&add);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter…")
            .margin_start(4)
            .margin_end(4)
            .margin_bottom(4)
            .visible(false)
            .build();

        let composer_heading = gtk::Label::builder()
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .build();
        let composer_title = gtk::Entry::builder()
            .placeholder_text("Title")
            .activates_default(false)
            .css_classes(["composer-field"])
            .build();
        let composer_body = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(7)
            .bottom_margin(7)
            .left_margin(9)
            .right_margin(9)
            .height_request(52)
            .build();
        let body_frame = gtk::ScrolledWindow::builder()
            .child(&composer_body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["composer-field"])
            .height_request(52)
            .build();
        let composer_cancel = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat"])
            .build();
        let composer_submit = gtk::Button::builder()
            .label("File")
            .sensitive(false)
            .build();
        let composer_start = gtk::Button::builder()
            .label("Start")
            .css_classes(["suggested-action"])
            .tooltip_text(
                "File this issue and start on it: a fresh clone of the checkout, and a \
                 chat given the issue as its first prompt",
            )
            .sensitive(false)
            .build();
        let composer_actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        composer_actions.append(&composer_cancel);
        composer_actions.append(&composer_submit);
        composer_actions.append(&composer_start);
        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["card", "backlog-composer"])
            .margin_start(4)
            .margin_end(4)
            .margin_bottom(6)
            .visible(false)
            .build();
        composer.append(&composer_heading);
        composer.append(&composer_title);
        composer.append(&body_frame);
        composer.append(&composer_actions);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "backlog-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(VISIBLE_ROWS * ROW_HEIGHT)
            .build();
        // Back to the top: floats over the list's top-right corner once
        // it is scrolled more than a page, because the rows that are
        // moving are at the top and a long queue puts them out of sight.
        let to_top = gtk::Button::builder()
            .icon_name("go-top-symbolic")
            .css_classes(["osd", "circular", "backlog-top"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_end(14)
            .tooltip_text("Back to the top")
            .visible(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&scroller).build();
        overlay.add_overlay(&to_top);
        {
            let adjustment = scroller.vadjustment();
            let button = to_top.clone();
            let show = move |adjustment: &gtk::Adjustment| {
                button.set_visible(adjustment.value() > adjustment.page_size());
            };
            adjustment.connect_value_changed(show.clone());
            adjustment.connect_page_size_notify(show);
        }
        {
            let adjustment = scroller.vadjustment();
            to_top.connect_clicked(move |_| adjustment.set_value(adjustment.lower()));
        }

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("backlog-panel");
        widget.set_widget_name("backlog");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&header);
        widget.append(&search);
        widget.append(&composer);
        widget.append(&overlay);

        let panel = Rc::new(Self {
            widget,
            count: count.clone(),
            search: search.clone(),
            scroller,
            list: list.clone(),
            composer,
            composer_title: composer_title.clone(),
            composer_body: composer_body.clone(),
            composer_heading,
            composer_submit: composer_submit.clone(),
            composer_start: composer_start.clone(),
            composing: RefCell::new(None),
            root,
            activity,
            issues: RefCell::new(Vec::new()),
            fleet: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            shown: RefCell::new(Vec::new()),
            listed: RefCell::new(Vec::new()),
            confirming: RefCell::new(None),
            open_menu: RefCell::new(None),
            writing: Cell::new(false),
            selecting: Cell::new(false),
            filling: Cell::new(false),
            render_deferred: Cell::new(false),
            quota: quota.clone(),
            quota_bar,
            quota_snapshot: RefCell::new(QuotaSnapshot::default()),
            quota_tooltip: RefCell::new(String::new()),
            probe_activity: RefCell::new(BTreeMap::new()),
            on_refresh: RefCell::new(None),
            on_toast: RefCell::new(None),
            on_start: RefCell::new(None),
            on_select: RefCell::new(None),
            on_tick: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&panel);
            list.connect_row_activated(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                if panel.selecting.get() {
                    return;
                }
                let index = row.index();
                if index < 0 {
                    return;
                }
                let env = panel
                    .listed
                    .borrow()
                    .get(index as usize)
                    .and_then(|row| row.env.clone());
                if let Some(env) = env {
                    panel.choose(&env);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            search.connect_search_changed(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.rerender();
                }
            });
        }
        {
            let keys = gtk::EventControllerKey::new();
            let list = list.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Down {
                    if let Some(row) = list.row_at_index(0) {
                        row.grab_focus();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
            search.add_controller(keys);
        }
        {
            let weak = Rc::downgrade(&panel);
            search.connect_activate(move |_| {
                let Some(panel) = weak.upgrade() else { return };
                let first = panel
                    .listed
                    .borrow()
                    .iter()
                    .skip(1)
                    .find_map(|row| row.env.clone());
                if let Some(env) = first {
                    panel.choose(&env);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            add.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.open_composer(Composing::New);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_cancel.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.close_composer();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_submit.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.submit_composer(false);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_start.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.submit_composer(true);
                }
            });
        }
        {
            let submit = composer_submit.clone();
            let start = composer_start.clone();
            composer_title.connect_changed(move |entry| {
                let has_title = !entry.text().trim().is_empty();
                submit.set_sensitive(has_title);
                start.set_sensitive(has_title);
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer_title.connect_activate(move |entry| {
                if entry.text().trim().is_empty() {
                    return;
                }
                if let Some(panel) = weak.upgrade() {
                    panel.submit_composer(false);
                }
            });
        }
        {
            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&panel);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    if let Some(panel) = weak.upgrade() {
                        panel.close_composer();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
            panel.composer.add_controller(keys);
        }
        {
            let weak = Rc::downgrade(&panel);
            glib::timeout_add_local(TICK, move || {
                let Some(panel) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                panel.tick();
                glib::ControlFlow::Continue
            });
        }
        panel.render();
        panel
    }

    pub fn set_on_refresh(&self, hook: impl Fn() + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_start(&self, hook: impl Fn(StartedIssue) + 'static) {
        *self.on_start.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_toast(&self, hook: impl Fn(String) + 'static) {
        *self.on_toast.borrow_mut() = Some(Box::new(hook));
    }

    /// A row with an environment was chosen: aim the panes at it.
    pub fn set_on_select(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(hook));
    }

    /// Once a second, before the sparklines are drawn. The window uses it
    /// to refresh the fleet, so the panel's clock is the app's.
    pub fn set_on_tick(&self, hook: impl Fn() + 'static) {
        *self.on_tick.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_issues(self: &Rc<Self>, issues: &[Issue]) {
        if self.issues.borrow().as_slice() == issues {
            return;
        }
        *self.issues.borrow_mut() = issues.to_vec();
        self.render();
    }

    pub fn set_fleet(self: &Rc<Self>, fleet: &[FleetRow]) {
        if self.fleet.borrow().as_slice() == fleet {
            return;
        }
        *self.fleet.borrow_mut() = fleet.to_vec();
        let live: Vec<EnvironmentId> = fleet.iter().map(|row| row.env.clone()).collect();
        self.activity.retain(&live);
        self.render();
    }

    /// Where the panes are aimed. `None` is home.
    pub fn set_current(self: &Rc<Self>, current: Option<EnvironmentId>) {
        if *self.current.borrow() == current {
            return;
        }
        *self.current.borrow_mut() = current;
        self.apply_face();
        self.render();
    }

    fn apply_face(&self) {
        if away(self.current.borrow().as_ref()) {
            self.widget.add_css_class("away");
        } else {
            self.widget.remove_css_class("away");
        }
    }

    /// Ctrl+Shift+E: the filter when there is one to type into, else the
    /// rows in turn, starting from the one the panes are aimed at.
    pub fn focus(self: &Rc<Self>) {
        if self.search.is_visible() && !self.search.has_focus() {
            self.search.grab_focus();
            return;
        }
        let count = self.listed.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let focused = (0..count).find(|index| {
            self.list
                .row_at_index(*index)
                .is_some_and(|row| row.has_focus())
        });
        let target = match focused {
            Some(index) => (index + 1) % count,
            None => {
                let aimed = self.aimed_at();
                self.listed
                    .borrow()
                    .iter()
                    .position(|row| row.env.as_ref() == Some(&aimed))
                    .unwrap_or(0) as i32
            }
        };
        if let Some(row) = self.list.row_at_index(target) {
            row.grab_focus();
        }
    }

    fn aimed_at(&self) -> EnvironmentId {
        self.current
            .borrow()
            .clone()
            .unwrap_or_else(EnvironmentId::primary)
    }

    fn choose(self: &Rc<Self>, env: &EnvironmentId) {
        self.search.set_text("");
        if let Some(hook) = self.on_select.borrow().as_ref() {
            hook(env.clone());
        }
    }

    fn tick(self: &Rc<Self>) {
        if let Some(tick) = self.on_tick.borrow().as_ref() {
            tick();
        }
        self.draw_activity();
        self.draw_quota();
    }

    pub fn set_quota(self: &Rc<Self>, snapshot: &QuotaSnapshot) {
        if *self.quota_snapshot.borrow() == *snapshot {
            return;
        }
        *self.quota_snapshot.borrow_mut() = snapshot.clone();
        self.draw_quota();
    }

    fn draw_quota(self: &Rc<Self>) {
        let snapshot = self.quota_snapshot.borrow();
        let now = std::time::SystemTime::now();
        let Some(headline) = snapshot.headline(now) else {
            self.quota.set_visible(false);
            return;
        };

        let spent = snapshot.current_exhaustion(now).is_some();
        let stale = snapshot.is_stale(now);
        crate::gauge::set(&self.quota_bar, headline.used, spent, stale);

        let tooltip = quota_tooltip(&snapshot, now);
        if *self.quota_tooltip.borrow() != tooltip {
            self.quota.set_tooltip_text(Some(&tooltip));
            *self.quota_tooltip.borrow_mut() = tooltip;
        }
        self.quota.set_visible(true);
    }

    fn draw_activity(self: &Rc<Self>) {
        let shown = self.shown.borrow();
        for row in self.listed.borrow().iter() {
            let (Some(env), Some(sparkline)) = (&row.env, &row.sparkline) else {
                continue;
            };
            let samples = self.samples_for(env);
            sparkline.set_samples(&samples);
            let tooltip = match shown.iter().find(|shown| shown.id == row.id) {
                Some(shown) => format!("{}\n{}", shown.tooltip(), Sparkline::describe(&samples)),
                None => Sparkline::describe(&samples),
            };
            row.widget.set_tooltip_text(Some(&tooltip));
        }
    }

    fn samples_for(&self, env: &EnvironmentId) -> [u16; BUCKETS] {
        if let Some(samples) = self.probe_activity.borrow().get(env) {
            return *samples;
        }
        self.activity.samples(env)
    }

    /// TASTE_PROBE_CHECK only: give one row a fabricated activity window,
    /// so a headless shot has sparklines in it.
    ///
    /// What is fabricated is the *samples*, not the drawing: the widget,
    /// the scale, the alpha and the theme colour are the real ones. The
    /// live sampler is left alone — a probe window has been up for two
    /// seconds and has no five minutes to have a history in.
    pub fn seed_activity_for_probe(self: &Rc<Self>, env: &EnvironmentId, shape: Shape) {
        self.probe_activity
            .borrow_mut()
            .insert(env.clone(), probe_samples(shape));
        self.draw_activity();
    }

    fn render(self: &Rc<Self>) {
        let rows = rows(
            &self.issues.borrow(),
            &self.fleet.borrow(),
            self.current.borrow().as_ref(),
        );
        if *self.shown.borrow() == rows && !self.list_is_empty_but_should_not_be(&rows) {
            return;
        }
        if self.menu_is_open() {
            self.render_deferred.set(true);
            return;
        }
        self.count.set_label(&summary(&rows));
        self.search.set_visible(filter_visible(rows.len()));
        let query = self.search.text().to_string();

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut listed: Vec<Listed> = Vec::new();
        let mut current_row: Option<gtk::ListBoxRow> = None;
        for row in rows.iter().filter(|row| matches(row, &query)) {
            let (widget, sparkline) = self.build_row(row);
            self.list.append(&widget);
            if row.live.as_ref().is_some_and(|live| live.current) {
                current_row = Some(widget.clone());
            }
            listed.push(Listed {
                id: row.id.clone(),
                env: row.live.as_ref().map(|live| live.env.clone()),
                widget,
                sparkline,
            });
        }
        if rows.len() == 1 {
            let empty = gtk::Label::builder()
                .label("Nothing written down")
                .css_classes(["dim-label", "caption"])
                .xalign(0.0)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(12)
                .margin_end(12)
                .wrap(true)
                .tooltip_text(
                    "Issues are how work outlives a conversation: write one and Start \
                     it, and it gets an environment of its own. An agent that finishes \
                     one cannot close it until its branch is merged.",
                )
                .build();
            let row = gtk::ListBoxRow::builder()
                .child(&empty)
                .activatable(false)
                .selectable(false)
                .build();
            self.list.append(&row);
        }
        let count = (listed.len() as i32 + i32::from(rows.len() == 1)).clamp(1, VISIBLE_ROWS);
        self.scroller.set_max_content_height(-1);
        self.scroller.set_min_content_height(count * ROW_HEIGHT);
        if !self.filling.get() {
            self.scroller
                .set_max_content_height(VISIBLE_ROWS * ROW_HEIGHT);
        }
        *self.listed.borrow_mut() = listed;
        *self.shown.borrow_mut() = rows;

        self.selecting.set(true);
        match &current_row {
            Some(row) => self.list.select_row(Some(row)),
            None => self.list.select_row(gtk::ListBoxRow::NONE),
        }
        self.selecting.set(false);

        self.draw_activity();
    }

    /// Gadget mode: the panel is the window, so the list takes the height
    /// instead of stopping at `VISIBLE_ROWS`.
    pub fn set_filling(self: &Rc<Self>, filling: bool) {
        self.filling.set(filling);
        self.widget.set_vexpand(filling);
        self.scroller.set_vexpand(filling);
        self.scroller.set_propagate_natural_height(!filling);
        self.scroller.set_max_content_height(if filling {
            -1
        } else {
            VISIBLE_ROWS * ROW_HEIGHT
        });
    }

    fn list_is_empty_but_should_not_be(&self, rows: &[Row]) -> bool {
        self.list.first_child().is_none() && !rows.is_empty()
    }

    fn build_row(self: &Rc<Self>, row: &Row) -> (gtk::ListBoxRow, Option<Sparkline>) {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(2);
        box_.set_margin_bottom(2);
        box_.set_margin_start(8);
        box_.set_margin_end(8);

        match &row.live {
            Some(live) => {
                box_.append(
                    &gtk::Box::builder()
                        .css_classes(["env-dot", live.light.css()])
                        .valign(gtk::Align::Center)
                        .build(),
                );
            }
            None => {
                box_.append(
                    &gtk::Image::builder()
                        .icon_name(state_icon(row.work))
                        .css_classes(state_classes(row.work))
                        .pixel_size(13)
                        .valign(gtk::Align::Center)
                        .tooltip_text(row.state_tooltip())
                        .build(),
                );
            }
        }

        let label = gtk::Label::builder()
            .label(&row.title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .build();
        if row.work.is_resolved() {
            label.add_css_class("dim-label");
        }
        if row.work == WorkState::Declined {
            let attrs = gtk::pango::AttrList::new();
            attrs.insert(gtk::pango::AttrInt::new_strikethrough(true));
            label.set_attributes(Some(&attrs));
        }
        box_.append(&label);

        let mut sparkline = None;
        if let Some(live) = &row.live {
            if live.awaits_user {
                box_.append(
                    &gtk::Box::builder()
                        .css_classes(["env-attention"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Its chat is waiting for your answer")
                        .build(),
                );
            }
            if live.current && !live.primary {
                box_.append(
                    &gtk::Image::builder()
                        .icon_name("system-lock-screen-symbolic")
                        .css_classes(["dim-label"])
                        .pixel_size(12)
                        .tooltip_text("Read-only: this is another environment's checkout")
                        .build(),
                );
            }
            if live.unpublished {
                box_.append(
                    &gtk::Box::builder()
                        .css_classes(["env-unpublished"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Work here that no other checkout has")
                        .build(),
                );
            }
            if let Some(icon) = live.review.icon() {
                box_.append(
                    &gtk::Image::builder()
                        .icon_name(icon)
                        .css_classes(match live.review {
                            ReviewMark::Flagged => vec!["env-review"],
                            _ => vec!["dim-label"],
                        })
                        .pixel_size(12)
                        .valign(gtk::Align::Center)
                        .tooltip_text(match live.review {
                            ReviewMark::Flagged => "Done, and waiting for your review",
                            _ => "You have ruled on this one — safe to destroy",
                        })
                        .build(),
                );
            }
            let line = Sparkline::new();
            box_.append(&line.widget);
            sparkline = Some(line);
        }

        if self.confirming.borrow().as_deref() == Some(row.id.as_str()) {
            let actions = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .valign(gtk::Align::Center)
                .css_classes(["backlog-confirm"])
                .build();
            actions.append(
                &gtk::Label::builder()
                    .label("Delete?")
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let cancel = self.icon_button("edit-undo-symbolic", "Keep it", &["flat"]);
            {
                let weak = Rc::downgrade(self);
                cancel.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        *panel.confirming.borrow_mut() = None;
                        panel.rerender();
                    }
                });
            }
            let confirm = self.icon_button(
                "user-trash-symbolic",
                "Delete this issue for good. Closing is how work ends; deleting is how a \
                 mistake is unmade.",
                &["flat", "destructive-action"],
            );
            {
                let weak = Rc::downgrade(self);
                let id = row.id.clone();
                confirm.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        *panel.confirming.borrow_mut() = None;
                        panel.delete(&id);
                    }
                });
            }
            actions.set_sensitive(!self.writing.get());
            actions.append(&cancel);
            actions.append(&confirm);
            box_.append(&actions);
        }

        // Rows with an environment are places: selecting one aims the
        // panes. The rest are records, and the selection cannot land on
        // them, so the aim is never drawn on a row it does not mean.
        let is_place = row.live.is_some();
        let widget = gtk::ListBoxRow::builder()
            .child(&box_)
            .activatable(is_place)
            .selectable(is_place)
            .build();
        if let Some(class) = row.live.as_ref().and_then(|live| live.review.css()) {
            widget.add_css_class(class);
        }

        if row.reorderable() {
            let source = gtk::DragSource::builder()
                .actions(gtk::gdk::DragAction::MOVE)
                .build();
            let id = row.id.clone();
            source.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&id.to_value()))
            });
            let dragged = widget.downgrade();
            source.connect_drag_begin(move |source, _| {
                let Some(row) = dragged.upgrade() else { return };
                source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 0);
                row.add_css_class("dragging");
            });
            let dragged = widget.downgrade();
            source.connect_drag_end(move |_, _, _| {
                if let Some(row) = dragged.upgrade() {
                    row.remove_css_class("dragging");
                }
            });
            let dragged = widget.downgrade();
            source.connect_drag_cancel(move |_, _, _| {
                if let Some(row) = dragged.upgrade() {
                    row.remove_css_class("dragging");
                }
                false
            });
            widget.add_controller(source);

            let target =
                gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
            {
                let weak = Rc::downgrade(self);
                let onto = widget.downgrade();
                target.connect_motion(move |_, _, y| {
                    if let Some(panel) = weak.upgrade() {
                        panel.clear_drop_marks();
                    }
                    if let Some(row) = onto.upgrade() {
                        row.add_css_class(mark_for(&row, y));
                    }
                    gtk::gdk::DragAction::MOVE
                });
            }
            {
                let weak = Rc::downgrade(self);
                target.connect_leave(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        panel.clear_drop_marks();
                    }
                });
            }
            {
                let weak = Rc::downgrade(self);
                let onto_id = row.id.clone();
                let onto = widget.downgrade();
                target.connect_drop(move |_, value, _, y| {
                    let (Some(panel), Some(row)) = (weak.upgrade(), onto.upgrade()) else {
                        return false;
                    };
                    panel.clear_drop_marks();
                    let Ok(dragged) = value.get::<String>() else {
                        return false;
                    };
                    panel.drop_onto(&dragged, &onto_id, mark_for(&row, y) == "drop-below");
                    true
                });
            }
            widget.add_controller(target);
        }

        if row.is_issue() {
            let context = gtk::GestureClick::builder().button(3).build();
            let weak = Rc::downgrade(self);
            let id = row.id.clone();
            let anchor = widget.downgrade();
            context.connect_pressed(move |_, _, x, y| {
                if let (Some(panel), Some(row)) = (weak.upgrade(), anchor.upgrade()) {
                    panel.show_context_menu(&row, &id, Some((x, y)));
                }
            });
            widget.add_controller(context);

            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(self);
            let id = row.id.clone();
            let anchor = widget.downgrade();
            keys.connect_key_pressed(move |_, key, _, state| {
                let asked = key == gtk::gdk::Key::Menu
                    || (key == gtk::gdk::Key::F10
                        && state.contains(gtk::gdk::ModifierType::SHIFT_MASK));
                if !asked {
                    return glib::Propagation::Proceed;
                }
                if let (Some(panel), Some(row)) = (weak.upgrade(), anchor.upgrade()) {
                    panel.show_context_menu(&row, &id, None);
                }
                glib::Propagation::Stop
            });
            widget.add_controller(keys);
        }
        widget.set_tooltip_text(Some(&row.tooltip()));
        (widget, sparkline)
    }

    /// The row's menu: the four moves, then Edit, Decline and Delete in a
    /// section of their own — the items that change what an issue *is* do
    /// not belong in the same group as the ones that only change where it
    /// sits in a list.
    ///
    /// Built per summoning, and every item's closure holds the ISSUE ID.
    /// The list is rebuilt by every write and by every refresh, so an index
    /// captured here would name a different row by the time it was used —
    /// which is exactly the defect the buttons this replaced had. The
    /// position is looked up now, and only to decide what is available.
    fn show_context_menu(
        self: &Rc<Self>,
        anchor: &gtk::ListBoxRow,
        id: &str,
        at: Option<(f64, f64)>,
    ) {
        use gtk::gio;

        let available = {
            let shown = self.shown.borrow();
            let Some(row) = shown.iter().find(|row| row.id == id) else {
                return;
            };
            let band: Vec<&Row> = shown
                .iter()
                .filter(|other| other.reorderable() && other.group() == row.group())
                .collect();
            match band.iter().position(|other| other.id == id) {
                Some(at) if row.reorderable() => moves(at, band.len()),
                _ => Moves {
                    up: false,
                    down: false,
                    top: false,
                    bottom: false,
                },
            }
        };

        let actions = gio::SimpleActionGroup::new();
        let add_action = |name: &str, enabled: bool, callback: Box<dyn Fn() + 'static>| {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(enabled);
            action.connect_activate(move |_, _| callback());
            actions.add_action(&action);
        };

        let menu = gio::Menu::new();
        let move_section = gio::Menu::new();
        for (name, label, direction, live) in [
            ("move-top", "Move to Top", IssueMove::Top, available.top),
            ("move-up", "Move Up", IssueMove::Up, available.up),
            ("move-down", "Move Down", IssueMove::Down, available.down),
            (
                "move-bottom",
                "Move to Bottom",
                IssueMove::Bottom,
                available.bottom,
            ),
        ] {
            move_section.append(Some(label), Some(&format!("row.{name}")));
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                name,
                live,
                Box::new(move || panel.move_issue(&id, direction)),
            );
        }
        menu.append_section(None, &move_section);

        // Whether this issue has already ended, either way. Read off the
        // stored resolution rather than the derived state, because they
        // answer this one identically — `Queued` and `Active` are both
        // `Resolution::Open` — and the stored field is the one the write
        // below will actually be compare-and-swapping.
        let resolved = self
            .issues
            .borrow()
            .iter()
            .find(|issue| issue.id == id)
            .is_some_and(|issue| issue.resolution.is_resolved());

        let edit_section = gio::Menu::new();
        edit_section.append(Some("Edit…"), Some("row.edit"));
        // Decline sits above Delete because the two are the same gesture
        // with opposite consequences, and the choice should be one item
        // apart: declining KEEPS the record — the issue, its body, its
        // comments, and a new one saying it was decided against — while
        // deleting takes the id away and with it any way to find out that
        // the idea was ever had. It asks nothing, because unlike a delete
        // it is undoable: reopening is an edit away. Hence no ellipsis
        // either, where Delete earns one by stopping to confirm.
        edit_section.append(Some("Decline"), Some("row.decline"));
        edit_section.append(Some("Delete…"), Some("row.delete"));
        menu.append_section(None, &edit_section);
        {
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                "edit",
                true,
                Box::new(move || panel.open_composer(Composing::Editing(id.clone()))),
            );
        }
        {
            // Insensitive on an issue that already ended: declining a
            // completed one is meaningless, and declining a declined one
            // twice is a second comment saying what the first said. It
            // stays in the menu rather than vanishing, for the same reason
            // the dead moves do — an item that disappears teaches a
            // different menu each time.
            let panel = self.clone();
            let id = id.to_string();
            add_action("decline", !resolved, Box::new(move || panel.decline(&id)));
        }
        {
            let panel = self.clone();
            let id = id.to_string();
            add_action(
                "delete",
                true,
                Box::new(move || {
                    *panel.confirming.borrow_mut() = Some(id.clone());
                    panel.rerender();
                }),
            );
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        // A probe target of its own: `filetree.backlog-menu` (ui_probe.rs).
        // A popover is its own native surface, so a shot of the pane
        // BEHIND it does not contain it — the menu has to be named to be
        // photographed.
        popover.set_widget_name("backlog-menu");
        popover.insert_action_group("row", Some(&actions));
        popover.set_parent(anchor);
        popover.set_has_arrow(false);
        if let Some((x, y)) = at {
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        }
        {
            let weak = Rc::downgrade(self);
            popover.connect_closed(move |popover| {
                let popover = popover.clone();
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if popover.parent().is_some() {
                        popover.unparent();
                    }
                    // A render that arrived while the menu was up waited
                    // for it; the list catches up now.
                    if let Some(panel) = weak.upgrade() {
                        if panel.render_deferred.replace(false) {
                            panel.rerender();
                        }
                    }
                });
            });
        }
        // Tracked so a rebuild can close it before the row it hangs off is
        // disposed under it — the file tree tracks its own for the same
        // reason, and this list rebuilds far more often.
        *self.open_menu.borrow_mut() = Some(popover.downgrade());
        popover.popup();
    }

    /// Take down an open row menu. A rebuild disposes the row it is
    /// anchored to, and a popover whose anchor died is a GTK warning at
    /// best and a menu acting on a vanished row at worst.
    fn close_context_menu(&self) {
        if let Some(popover) = self.open_menu.borrow_mut().take().and_then(|w| w.upgrade()) {
            popover.popdown();
            // Now, not on the closed signal's idle: the caller is about to
            // take the row apart, and a row finalized with a popover still
            // parented to it is a crash (GTK says so, then segfaults).
            popover.unparent();
        }
    }

    fn menu_is_open(&self) -> bool {
        self.open_menu
            .borrow()
            .as_ref()
            .is_some_and(|menu| menu.upgrade().is_some())
    }

    /// Clear every row's drop indicator. Cheap (the list is capped at what
    /// fits in a flank) and unconditional, because a drag that left one
    /// behind would point at a gap the row is not going to land in.
    fn clear_drop_marks(&self) {
        let mut child = self.list.first_child();
        while let Some(row) = child {
            row.remove_css_class("drop-above");
            row.remove_css_class("drop-below");
            child = row.next_sibling();
        }
    }

    /// Commit a drag: `dragged` lands on the side of `onto` the pointer was
    /// nearest. A drop on itself, or into the gap it already fills, is not
    /// a write — [`drop_index`] is what decides that, and it says `None`.
    fn drop_onto(self: &Rc<Self>, dragged: &str, onto: &str, below: bool) {
        let issues = self.issues.borrow();
        let (Some(from), Some(at)) = (
            issues.iter().position(|issue| issue.id == dragged),
            issues.iter().position(|issue| issue.id == onto),
        ) else {
            return;
        };
        drop(issues);
        let Some(to) = drop_index(from, at, below) else {
            return;
        };
        let was = self.reorder_to(dragged, to);
        let id = dragged.to_string();
        self.write(was, move |git| git.issue_reorder(&id, to).map(|_| ()));
    }

    fn icon_button(&self, icon: &str, tooltip: &str, classes: &[&str]) -> gtk::Button {
        gtk::Button::builder()
            .child(&gtk::Image::builder().icon_name(icon).pixel_size(12).build())
            .css_classes(classes.to_vec())
            .valign(gtk::Align::Center)
            .tooltip_text(tooltip)
            .build()
    }

    /// Rebuild the list even though the model did not move — the delete
    /// confirmation and the in-flight guard are row state, not issue state.
    fn rerender(self: &Rc<Self>) {
        self.close_context_menu();
        self.shown.borrow_mut().clear();
        self.render();
    }

    // --- the composer ----------------------------------------------------

    fn open_composer(self: &Rc<Self>, what: Composing) {
        match &what {
            Composing::New => {
                self.composer_heading.set_label("New issue");
                self.composer_submit.set_label("File");
                self.composer_start.set_visible(true);
                self.composer_title.set_text("");
                self.composer_body.buffer().set_text("");
            }
            Composing::Editing(id) => {
                let issues = self.issues.borrow();
                let Some(issue) = issues.iter().find(|issue| &issue.id == id) else {
                    return;
                };
                self.composer_heading
                    .set_label(&format!("Edit {}", issue.id));
                self.composer_submit.set_label("Save");
                // Start is for an issue nobody has started; a started or
                // settled one is edited, not started again.
                self.composer_start
                    .set_visible(issue.state() == taste_git::IssueState::Queued);
                self.composer_title.set_text(&issue.title);
                self.composer_body.buffer().set_text(&issue.body);
            }
        }
        let has_title = !self.composer_title.text().trim().is_empty();
        self.composer_submit.set_sensitive(has_title);
        self.composer_start.set_sensitive(has_title);
        *self.composing.borrow_mut() = Some(what);
        self.composer.set_visible(true);
        self.composer_title.grab_focus();
    }

    fn close_composer(self: &Rc<Self>) {
        self.composer.set_visible(false);
        *self.composing.borrow_mut() = None;
    }

    fn submit_composer(self: &Rc<Self>, start: bool) {
        let Some(what) = self.composing.borrow().clone() else {
            return;
        };
        let title = self.composer_title.text().trim().to_string();
        if title.is_empty() {
            return;
        }
        let buffer = self.composer_body.buffer();
        let body = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        self.close_composer();
        match what {
            Composing::New => self.create(title, body, start),
            Composing::Editing(id) => self.edit(id, title, body, start),
        }
    }

    /// Hand a written issue to the window to start.
    fn start(&self, id: String, title: String, body: String) {
        if let Some(hook) = self.on_start.borrow().as_ref() {
            hook(StartedIssue { id, title, body });
        }
    }

    // --- writes ----------------------------------------------------------
    //
    // Every one of these is the same shape: go insensitive, do the git work
    // in `spawn_blocking` against a workspace discovered on that thread,
    // then ask for a refresh. The compare-and-swap retry lives in
    // `taste-git` and re-reads the winner's list, so what lands here is
    // always what is really on the ref — which is why the optimistic half
    // is safe: the refresh is the correction.

    fn move_issue(self: &Rc<Self>, id: &str, direction: IssueMove) {
        let to = move_target(&self.shown.borrow(), &self.issues.borrow(), id, direction);
        let Some(to) = to else { return };
        let was = self.reorder_to(id, to);
        let id = id.to_string();
        self.write(was, move |git| git.issue_reorder(&id, to).map(|_| ()));
    }

    /// Move a row in the list we are already showing, so the gesture lands
    /// before the git write does. Nothing is persisted here — this is the
    /// half-second before the refresh — so it answers with the order it
    /// replaced, which is what a failed write has to be put back to.
    fn reorder_to(self: &Rc<Self>, id: &str, to: usize) -> Option<Vec<Issue>> {
        let mut issues = self.issues.borrow_mut();
        let at = issues.iter().position(|issue| issue.id == id)?;
        if at == to || to >= issues.len() {
            return None;
        }
        let was = issues.clone();
        let issue = issues.remove(at);
        issues.insert(to, issue);
        drop(issues);
        self.render();
        Some(was)
    }

    fn delete(self: &Rc<Self>, id: &str) {
        let id = id.to_string();
        self.write(None, move |git| git.issue_delete(&id));
    }

    /// Decline it: the issue stays, and gains a comment saying it was
    /// decided against.
    ///
    /// The author is `primary` for the same reason a filed issue's reporter
    /// is: this menu is in the user's own window, and attributing their
    /// decision to an agent's environment would be a lie the issue carries
    /// forever.
    ///
    /// Nothing to revert: a decline changes a state, not an order, so the
    /// rows it would put back are the rows already on screen.
    fn decline(self: &Rc<Self>, id: &str) {
        let id = id.to_string();
        self.write(None, move |git| {
            git.issue_decline(&id, "primary", None).map(|_| ())
        });
    }

    fn create(self: &Rc<Self>, title: String, body: String, start: bool) {
        let (for_write, for_start) = ((title.clone(), body.clone()), (title, body));
        self.write_then(
            None,
            move |git| {
                // The reporter is the user's own checkout: this composer is
                // in the user's window, and attributing it to an agent's
                // environment would be a lie the issue carries forever.
                git.issue_create(&for_write.0, &for_write.1, &[], "primary")
                    .map(|issue| issue.id)
            },
            move |panel, id| {
                if start {
                    panel.start(id, for_start.0, for_start.1);
                }
            },
        );
    }

    fn edit(self: &Rc<Self>, id: String, title: String, body: String, start: bool) {
        let (for_write, for_start) = ((title.clone(), body.clone()), (title, body));
        let for_hook = id.clone();
        self.write_then(
            None,
            move |git| {
                let target = git.issue_target_branch();
                let change = taste_git::IssueChange {
                    title: Some(for_write.0),
                    body: Some(for_write.1),
                    ..Default::default()
                };
                git.issue_update(&id, &change, &target, "primary")
                    .map(|_| ())
            },
            move |panel, ()| {
                if start {
                    panel.start(for_hook, for_start.0, for_start.1);
                }
            },
        );
    }

    /// The one write path. Off the main thread, one at a time, and every
    /// outcome ends in a refresh.
    ///
    /// `revert_to` is the order the rows had before an optimistic reorder
    /// moved them, and a failure puts it back **here** rather than trusting
    /// the refresh to. The refresh cannot do it: a write that failed left
    /// git saying exactly what it said before, and every reader of the
    /// queue — this panel and the console that feeds it — is
    /// equality-guarded, so nothing announces and the row stays where the
    /// user's gesture optimistically put it. Forever, and wrongly.
    fn write<F>(self: &Rc<Self>, revert_to: Option<Vec<Issue>>, op: F)
    where
        F: FnOnce(&taste_git::GitWorkspace) -> anyhow::Result<()> + Send + 'static,
    {
        self.write_then(revert_to, op, |_, ()| {});
    }

    /// `write`, with what the write produced handed to `then` on this
    /// thread once the ref has it — Start needs the id the store chose.
    fn write_then<T, F, D>(self: &Rc<Self>, revert_to: Option<Vec<Issue>>, op: F, then: D)
    where
        T: Send + 'static,
        F: FnOnce(&taste_git::GitWorkspace) -> anyhow::Result<T> + Send + 'static,
        D: FnOnce(&Rc<Self>, T) + 'static,
    {
        if self.writing.get() {
            // Refusing is right — two compare-and-swaps on one ref is how
            // an order gets decided by a race — but refusing SILENTLY is
            // not: this used to swallow a filed issue whole, composer
            // already closed, with nothing on screen to say so.
            if let Some(toast) = self.on_toast.borrow().as_ref() {
                toast(
                    "The backlog is still saving the last change — try again in a moment.".into(),
                );
            }
            if let Some(order) = revert_to {
                *self.issues.borrow_mut() = order;
                self.render();
            }
            return;
        }
        self.writing.set(true);
        self.rerender();
        let root = self.root.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = taste_git::GitWorkspace::discover(&root)
                    .ok_or_else(|| anyhow::anyhow!("this workspace is not a git repository"))?;
                op(&git)
            });
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(e) => Err(anyhow::anyhow!("the write did not finish: {e}")),
            };
            let Some(panel) = weak.upgrade() else { return };
            panel.writing.set(false);
            let produced = match outcome {
                Ok(value) => Some(value),
                Err(e) => {
                    if let Some(toast) = panel.on_toast.borrow().as_ref() {
                        toast(format!("{e:#}"));
                    }
                    // The rows moved on a promise this write did not keep.
                    if let Some(order) = revert_to {
                        *panel.issues.borrow_mut() = order;
                    }
                    None
                }
            };
            // Always: the ref is the truth, and the optimistic rows are
            // only ever a guess at it.
            if let Some(refresh) = panel.on_refresh.borrow().as_ref() {
                refresh();
            }
            panel.rerender();
            if let Some(value) = produced {
                then(&panel, value);
            }
        });
    }

    /// TASTE_PROBE_CHECK only: open one row's context menu, as a
    /// right-click would.
    ///
    /// What is fabricated is the summoning, and only that: the menu, its
    /// sections, and which of its items this row can actually use are the
    /// real ones, built by the real code path. A drag cannot be
    /// photographed mid-flight, so the menu is what a still frame can show
    /// of what the rows DO — and it is the half a keyboard uses anyway.
    pub fn seed_menu_for_probe(self: &Rc<Self>, id: &str) {
        let index = self.listed.borrow().iter().position(|row| row.id == id);
        let Some(row) = index.and_then(|i| self.list.row_at_index(i as i32)) else {
            return;
        };
        self.show_context_menu(&row, id, None);
    }

    /// TASTE_PROBE_CHECK only: open the composer with something typed in
    /// it, so the two fields can be LOOKED at side by side.
    ///
    /// Through the same door the `+` button uses, and through the real
    /// setters: a fixture that poked the widgets directly would keep
    /// photographing a composer the app had stopped building that way.
    pub fn seed_composer_for_probe(self: &Rc<Self>) {
        self.open_composer(Composing::New);
        self.composer_title
            .set_text("Relocation waits for the container");
        self.composer_body.buffer().set_text(
            "Opening a chat in a stopped environment must not try to \
             relocate: the agent starts outside and moves in when the \
             container comes up.",
        );
        self.composer_submit.set_sensitive(true);
    }
}

/// Which edge of `row` a pointer at `y` is asking to drop against. The
/// halfway line, so every point in the list belongs to exactly one gap and
/// the indicator never has to guess.
fn mark_for(row: &gtk::ListBoxRow, y: f64) -> &'static str {
    if y * 2.0 >= f64::from(row.height()) {
        "drop-below"
    } else {
        "drop-above"
    }
}

/// The activity shapes the probe fixture draws. Named after what they are
/// of, because a screenshot is judged against what it claims to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// An agent mid-task in a container that is up: a warm-up, a long
    /// noisy plateau of tool calls and output, and a tail still going.
    Working,
    /// A container building: a burst as each step completes, and nothing
    /// in between.
    Building,
    /// A person at a keyboard: saves, git refreshes, a file watcher — a
    /// low irregular trickle rather than a machine's rhythm.
    Editing,
    /// An agent that asked a question and has been waiting ever since:
    /// three events near the start of the window and nothing after them.
    ///
    /// The floor case, and it is in the fixture on purpose. Almost-nothing
    /// is the shape a sparkline is worst at and the one a fleet is most
    /// often in, so the frame that judges this widget has to contain one —
    /// a set of shots where every row is busy proves only that busy works.
    Waiting,
    /// Nothing at all. Draws no line — see [`crate::sparkline`].
    Silent,
}

/// A fabricated five-minute window. Deterministic — a screenshot that
/// differed run to run could not be judged against the last one — and
/// shaped by arithmetic rather than a table, so the wobble reads as
/// measurement instead of as decoration.
fn probe_samples(shape: Shape) -> [u16; BUCKETS] {
    let mut out = [0; BUCKETS];
    let wobble = |index: usize, spread: u16| ((index * 37) % 13) as u16 % spread.max(1);
    match shape {
        Shape::Working => {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = match index {
                    0..=7 => continue,
                    8..=17 => 5 + wobble(index, 6),
                    18..=46 => 17 + wobble(index, 13) * 2,
                    _ => 8 + wobble(index, 9),
                };
            }
        }
        Shape::Building => {
            for index in [9, 10, 24, 25, 26, 43, 57, 58] {
                out[index] = 4 + wobble(index, 8);
            }
        }
        Shape::Editing => {
            for index in [4, 5, 13, 21, 22, 23, 34, 39, 40, 51, 52, 53, 54] {
                out[index] = 2 + wobble(index, 5);
            }
        }
        Shape::Waiting => {
            // Three events, and the last of them four minutes ago: the
            // turn that ended in a question, and the silence since.
            for index in [6, 7, 15] {
                out[index] = 2 + wobble(index, 4);
            }
        }
        Shape::Silent => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{assemble, ChatBinding, EnvFacts, Spend};
    use taste_core::state::WorkspaceState;
    use taste_git::Resolution;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn issue(id: &str, title: &str, resolution: Resolution, started_by: Option<&str>) -> Issue {
        Issue {
            id: id.into(),
            title: title.into(),
            resolution,
            reporter: "primary".into(),
            started_by: started_by.map(str::to_string),
            created: 0,
            updated: 0,
            labels: Vec::new(),
            links: Vec::new(),
            body: String::new(),
            comments: Vec::new(),
        }
    }

    fn facts(slug: &str, state: SupervisorState) -> EnvFacts {
        EnvFacts {
            env: env(slug),
            state,
            authority: taste_core::ConfigAuthority::Project,
            pending_rebuild: false,
            chat: None,
            git: None,
            disk: None,
            spend: Spend::default(),
            shells: 0,
            review: taste_core::ReviewState::Working,
            working_on: Vec::new(),
        }
    }

    fn running() -> SupervisorState {
        SupervisorState::Running {
            container_id: "abc123".into(),
        }
    }

    fn fleet(facts: Vec<EnvFacts>) -> Vec<FleetRow> {
        assemble(facts, &WorkspaceState::default(), &[])
    }

    /// The issues, in the order a user might keep them: a completed one
    /// first, then two started (one with an environment here, one not),
    /// then a queued one, then a declined one.
    fn issues() -> Vec<Issue> {
        vec![
            issue(
                "i-0004",
                "Keep terminal output",
                Resolution::Completed,
                Some("d@atelier"),
            ),
            issue(
                "i-0007",
                "The composer loses a draft",
                Resolution::Open,
                Some("d@atelier"),
            ),
            issue(
                "i-0002",
                "Cost of a stopped environment",
                Resolution::Open,
                Some("d@laptop"),
            ),
            issue(
                "i-0009",
                "Sparklines across rebuilds",
                Resolution::Open,
                None,
            ),
            issue("i-0011", "Per-project settings", Resolution::Declined, None),
        ]
    }

    #[test]
    fn the_primary_row_is_first_named_yours_and_is_home() {
        let list = rows(&issues(), &fleet(vec![facts("primary", running())]), None);
        let first = &list[0];
        assert_eq!(first.title, PRIMARY_TITLE);
        assert!(!first.is_issue() && first.group() == Group::Primary);
        let live = first.live.as_ref().unwrap();
        assert!(live.primary && live.current && live.light == Light::Green);
        assert!(first.tooltip().contains("your own checkout"));
        assert!(!first.tooltip().contains("read-only"));
        assert!(!away(None), "home is not tinted");

        // No fleet yet: the row still exists and says it does not know.
        let cold = rows(&[], &[], None);
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].live.as_ref().unwrap().light, Light::Unknown);
    }

    #[test]
    fn rows_sort_by_group_and_keep_the_stored_order_within_one() {
        let fleet = fleet(vec![
            facts("primary", running()),
            facts("i-0007", running()),
        ]);
        let rows = rows(&issues(), &fleet, None);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["primary", "i-0007", "i-0002", "i-0009", "i-0004", "i-0011"],
            "live, then open in stored order, then resolved in stored order"
        );
        let groups: Vec<Group> = rows.iter().map(Row::group).collect();
        assert!(groups.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn a_started_issue_with_an_environment_here_is_live() {
        let mut waiting = facts("i-0007", running());
        waiting.chat = Some(ChatBinding {
            label: "Claude Code".into(),
            busy: false,
            awaits_user: true,
            orchestrator: false,
        });
        let fleet = fleet(vec![facts("primary", running()), waiting]);
        let rows = rows(&issues(), &fleet, Some(&env("i-0007")));
        let row = rows.iter().find(|row| row.id == "i-0007").unwrap();
        let live = row.live.as_ref().unwrap();
        assert_eq!(row.work, WorkState::Waiting);
        assert_eq!(live.light, Light::Amber);
        assert!(live.awaits_user && live.current && !live.primary);
        assert!(!row.reorderable(), "an environment's place is its state's");
        let tip = row.tooltip();
        assert!(
            tip.starts_with("i-0007 — The composer loses a draft"),
            "{tip}"
        );
        assert!(tip.contains("read-only to you") && tip.contains("waiting for an answer"));
        assert!(
            away(Some(&env("i-0007"))),
            "aimed away from home tints the panel"
        );
        assert!(!rows[0].live.as_ref().unwrap().current, "one current row");
    }

    #[test]
    fn a_flagged_row_is_in_review_and_a_rejected_one_is_queued_again() {
        let mut flagged = facts("i-0007", SupervisorState::Stopped);
        flagged.review = taste_core::ReviewState::FlaggedForReview;
        let mut rejected = facts("i-0002", SupervisorState::Stopped);
        rejected.review = taste_core::ReviewState::Rejected;
        let fleet = fleet(vec![facts("primary", running()), flagged, rejected]);
        let rows = rows(&issues(), &fleet, None);
        let flagged = rows.iter().find(|row| row.id == "i-0007").unwrap();
        assert_eq!(flagged.work, WorkState::Review);
        assert_eq!(flagged.live.as_ref().unwrap().review, ReviewMark::Flagged);
        assert!(flagged.tooltip().contains("waiting for your review"));
        let rejected = rows.iter().find(|row| row.id == "i-0002").unwrap();
        assert_eq!(
            rejected.work,
            WorkState::Queued,
            "not this attempt is not not this work"
        );
        assert!(
            rejected.live.is_some(),
            "its environment is still here to destroy"
        );
    }

    #[test]
    fn a_started_issue_with_no_environment_here_says_where_it_is() {
        let rows = rows(&issues(), &fleet(vec![facts("primary", running())]), None);
        let row = rows.iter().find(|row| row.id == "i-0002").unwrap();
        assert_eq!(row.work, WorkState::Stopped);
        assert!(row.live.is_none() && row.group() == Group::Open);
        assert_eq!(state_icon(row.work), "checkbox-mixed-symbolic");
        assert!(row.state_tooltip().contains("Started by d@laptop"));
        assert!(row.tooltip().contains("no environment for it"));
    }

    #[test]
    fn a_declined_row_reads_the_decision_off_the_trail() {
        let mut declined = issue("i-0011", "Per-project settings", Resolution::Declined, None);
        declined.comments = vec![taste_git::Comment {
            seq: 1,
            author: "primary".into(),
            created: 0,
            body: "Declined: convention over configuration.\nMore below.".into(),
        }];
        let rows = rows(&[declined], &[], None);
        let row = &rows[1];
        assert_eq!(row.work, WorkState::Declined);
        assert_eq!(row.note.as_deref(), Some("convention over configuration."));
        assert_eq!(
            row.state_tooltip(),
            "Declined — convention over configuration."
        );
        assert_eq!(state_icon(row.work), "action-unavailable-symbolic");
    }

    #[test]
    fn the_header_counts_the_work_that_is_left_and_the_part_that_moves() {
        let fleet = fleet(vec![
            facts("primary", running()),
            facts("i-0007", running()),
            // Merged, and its environment not yet destroyed: done, not active.
            facts("i-0004", SupervisorState::Stopped),
        ]);
        assert_eq!(
            summary(&rows(&issues(), &fleet, None)),
            "3 · 1 active · 1 done · 1 declined"
        );
        assert_eq!(summary(&rows(&[], &fleet, None)), "empty");
        let open = vec![issue("i-0001", "One", Resolution::Open, None)];
        assert_eq!(summary(&rows(&open, &fleet, None)), "1");
    }

    #[test]
    fn the_filter_matches_title_and_id_and_never_hides_the_way_home() {
        let rows = rows(&issues(), &fleet(vec![facts("primary", running())]), None);
        assert!(matches(&rows[0], "zzz"), "the primary row always shows");
        let composer = rows.iter().find(|row| row.id == "i-0007").unwrap();
        assert!(matches(composer, "DRAFT") && matches(composer, "0007"));
        assert!(!matches(composer, "sparkline"));
        assert!(matches(composer, "  "), "blank is no filter");
        assert!(!filter_visible(FILTER_THRESHOLD) && filter_visible(FILTER_THRESHOLD + 1));
    }

    #[test]
    fn a_menu_move_stays_in_the_queue_and_names_the_store_position() {
        // Stored: i-0004 (done), i-0007 (live), i-0002 (open), i-0009 (open), i-0011.
        let stored = issues();
        let fleet = fleet(vec![
            facts("primary", running()),
            facts("i-0007", running()),
        ]);
        let shown = rows(&stored, &fleet, None);
        // The queue band is i-0002, i-0009. Up from the top of it: nothing.
        assert_eq!(move_target(&shown, &stored, "i-0002", IssueMove::Up), None);
        // Down from i-0002 passes i-0009, which is stored at index 3.
        assert_eq!(
            move_target(&shown, &stored, "i-0002", IssueMove::Down),
            Some(3)
        );
        assert_eq!(
            move_target(&shown, &stored, "i-0002", IssueMove::Bottom),
            Some(3)
        );
        // Up from i-0009 passes i-0002, stored at index 2 — never i-0007.
        assert_eq!(
            move_target(&shown, &stored, "i-0009", IssueMove::Up),
            Some(2)
        );
        assert_eq!(
            move_target(&shown, &stored, "i-0009", IssueMove::Top),
            Some(2)
        );
        // A live or resolved row has no band to move in.
        assert_eq!(
            move_target(&shown, &stored, "i-0007", IssueMove::Down),
            None
        );
        assert_eq!(move_target(&shown, &stored, "i-0004", IssueMove::Up), None);
    }

    #[test]
    fn a_drop_lands_in_the_gap_it_was_aimed_at() {
        assert_eq!(drop_index(0, 2, false), Some(1));
        assert_eq!(drop_index(0, 2, true), Some(2));
        assert_eq!(drop_index(3, 1, false), Some(1));
        assert_eq!(drop_index(3, 1, true), Some(2));
    }

    #[test]
    fn a_drop_that_changes_nothing_is_not_a_write() {
        assert_eq!(drop_index(1, 1, false), None);
        assert_eq!(drop_index(1, 1, true), None);
        assert_eq!(drop_index(1, 0, true), None);
        assert_eq!(drop_index(1, 2, false), None);
    }

    #[test]
    fn the_ends_of_the_list_cannot_move_further_out() {
        let first = moves(0, 3);
        assert!(!first.up && !first.top && first.down && first.bottom);
        let last = moves(2, 3);
        assert!(last.up && last.top && !last.down && !last.bottom);
        let only = moves(0, 1);
        assert!(!only.up && !only.down && !only.top && !only.bottom);
    }

    #[test]
    fn the_state_glyphs_are_distinct() {
        let icons = [
            state_icon(WorkState::Queued),
            state_icon(WorkState::Stopped),
            state_icon(WorkState::Completed),
            state_icon(WorkState::Declined),
        ];
        for (i, a) in icons.iter().enumerate() {
            for b in &icons[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
