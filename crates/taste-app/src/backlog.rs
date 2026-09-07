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
//! A row is two lines: the title, and under it what the work is doing —
//! the environment's state line with its marks, or the queue's word and
//! an age — with the sparkline at the end spanning both. Below the list,
//! permanent, sits the composer (`composer.rs`, the chat's own) for NEW
//! issues only: the first line is the title, the rest the body, and its
//! pill is **Create**. What happens to an issue that exists is done from the
//! header — Start, Stop, Delete act on the selected row — and from the
//! row's menu, whose Edit opens the same composer in a popover on the row.
//! Selecting a row with an environment aims the panes at it.
//!
//! Sizing: the list shows up to `VISIBLE_ROWS` and scrolls past that, and
//! grows a type-to-filter entry when it outgrows reading. In gadget mode
//! (`set_filling`) it fills the window instead, and a floating "back to
//! top" button appears once the list is scrolled more than a page — the
//! active rows are at the top, and that is where the eye wants to return.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use taste_core::activity::{Activity, BUCKETS};
use taste_core::environment::EnvironmentId;
use taste_core::quota::{describe_age, describe_countdown, QuotaSnapshot};
use taste_core::work::{work_state, Outcome, Runtime, WorkState};
use taste_core::Workspace;
use taste_devcontainer::SupervisorState;
use taste_git::{Issue, IssueMove, IssueState, NewAttachment};

use crate::fleet::{FleetRow, Light, ReviewMark};
use crate::sparkline::Sparkline;

/// What the primary row is called. Not the workspace's name: the panel
/// answers "whose checkout is this", and the only honest answer for the
/// row that is not an issue is the user's.
pub const PRIMARY_TITLE: &str = "Yours";

/// Rows the list shows before it scrolls. Six two-line rows is the height
/// the two panels this replaced took together, and still a glance.
pub const VISIBLE_ROWS: i32 = 6;

/// One row: the 40px two-line `.backlog-list > row` plus 2px of margin
/// either side.
const ROW_HEIGHT: i32 = 44;

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
    /// Body and comments, for the query; never drawn.
    pub haystack: String,
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

    /// The row's second line: what the work is doing, in a few words.
    pub fn caption(&self) -> String {
        match &self.live {
            Some(live) if live.primary => live.detail.clone(),
            Some(live) => live.detail.clone(),
            None => {
                let age = crate::filetree::relative_age(self.updated);
                match self.work {
                    WorkState::Queued => format!("queued · {age}"),
                    WorkState::Completed => format!("completed · {age}"),
                    WorkState::Declined => match &self.note {
                        Some(note) => format!("declined — {note}"),
                        None => format!("declined · {age}"),
                    },
                    _ => match self.started_by.as_deref() {
                        Some(who) => format!("started by {who} · not on this machine"),
                        None => "started · not on this machine".to_string(),
                    },
                }
            }
        }
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
        haystack: String::new(),
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
                haystack: {
                    let mut text = issue.body.clone();
                    for comment in &issue.comments {
                        text.push('\n');
                        text.push_str(&comment.body);
                    }
                    text
                },
            }
        })
        .collect();
    // Stable: within a group the store's order is the order.
    issues.sort_by_key(Row::group);
    out.extend(issues);
    out
}

/// The header's count, short: how much is left and how much is moving.
/// The header has three buttons at its right now, so the rest of the
/// breakdown ([`summary`]) is the count's tooltip.
pub fn summary_short(rows: &[Row]) -> String {
    let issues: Vec<&Row> = rows.iter().filter(|row| row.is_issue()).collect();
    if issues.is_empty() {
        return "empty".to_string();
    }
    let open = issues.iter().filter(|row| !row.work.is_resolved()).count();
    let active = issues
        .iter()
        .filter(|row| row.group() == Group::Live && !row.work.is_resolved())
        .count();
    if active > 0 {
        format!("{open} · {active} active")
    } else {
        open.to_string()
    }
}

/// The header's count in full: how much is left, how much of it is moving,
/// and how much is over.
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

/// The one query, over what the row shows and what its issue holds: title,
/// id, body and comments. The primary row always matches — it is the way
/// home, and a filter that hid it would strand the user in a clone.
pub fn row_matches(row: &Row, query: &crate::search::Query) -> bool {
    if query.is_empty() || !row.is_issue() {
        return true;
    }
    query.matches(&row.title) || query.matches(&row.id) || query.matches(&row.haystack)
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

/// The composer's convention, the commit box's: the first line is the
/// title, what follows (blank lines dropped at the seam) is the body.
pub fn split_issue_text(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    let title = lines
        .find(|line| !line.trim().is_empty())?
        .trim()
        .to_string();
    let rest: Vec<&str> = lines.collect();
    let body = rest.join("\n").trim().to_string();
    Some((title, body))
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

/// One built row, kept so the tick can redraw its sparkline and the
/// probe can find it by id.
struct Listed {
    id: String,
    /// The issue this row is, or `None` for the primary row.
    issue: Option<String>,
    env: Option<EnvironmentId>,
    widget: gtk::ListBoxRow,
    sparkline: Option<Sparkline>,
}

pub struct BacklogPanel {
    pub widget: gtk::Box,
    count: gtk::Label,
    /// The one query (search.rs) as last broadcast, and how many hits sit
    /// inside each issue's environment — its chat, its terminals — which
    /// keep a row that did not match itself (reachability).
    query: RefCell<crate::search::Query>,
    inner_hits: RefCell<HashMap<String, usize>>,
    searching: gtk::LevelBar,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    composer: Rc<crate::composer::Composer>,
    start_button: gtk::Button,
    stop_button: gtk::Button,
    delete_button: gtk::Button,
    workspace: Workspace,
    root: std::path::PathBuf,
    activity: Activity,
    issues: RefCell<Vec<Issue>>,
    fleet: RefCell<Vec<FleetRow>>,
    current: RefCell<Option<EnvironmentId>>,
    shown: RefCell<Vec<Row>>,
    listed: RefCell<Vec<Listed>>,
    confirming: RefCell<Option<String>>,
    open_menu: RefCell<Option<glib::WeakRef<gtk::Popover>>>,
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
    on_stop: RefCell<Option<SelectHook>>,
    on_destroy: RefCell<Option<SelectHook>>,
    on_tick: RefCell<Option<RefreshHook>>,
    /// A click on the row the panes already aim at, while it has hits
    /// inside: step through them (David, 2026-09-06).
    on_step_hits: RefCell<Option<SelectHook>>,
}

impl BacklogPanel {
    pub fn new(root: std::path::PathBuf, activity: Activity, workspace: &Workspace) -> Rc<Self> {
        // The flank's section header — arrow, glyph, title — the same row
        // Logs and Ports wear (`filetree::section_header`); the count, the
        // gauge and the actions follow on it.
        let header = crate::filetree::section_header("view-list-ordered-symbolic", "Backlog");
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
        // The actions on the selected row, at the header's right: Start a
        // queued issue, Stop a running environment, Delete an issue (or
        // destroy its environment, which is the console's intervention).
        let action = |icon: &str, tip: &str| {
            gtk::Button::builder()
                .child(
                    &gtk::Image::builder()
                        .icon_name(icon)
                        .css_classes(["dim-label"])
                        .pixel_size(14)
                        .build(),
                )
                .css_classes(["flat", "circular", "backlog-new"])
                .tooltip_text(tip)
                .sensitive(false)
                .build()
        };
        let start_button = action(
            "media-playback-start-symbolic",
            "Start the selected issue: a fresh clone of the checkout, and a chat given \
             the issue as its first prompt",
        );
        let stop_button = action(
            "media-playback-stop-symbolic",
            "Stop the selected issue's container (its clone stays)",
        );
        let delete_button = action(
            "user-trash-symbolic",
            "Delete the selected issue — or, when it has an environment, destroy that \
             environment (asked in the console first)",
        );

        // The search running inside environments (chats, terminals): one
        // rule beside the subscription gauge, in the accent colour, filling
        // as environments finish. Two rules of one shape, one of which
        // appears only while typing.
        let searching = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::Center)
            .css_classes(["search-rule"])
            .tooltip_text("Searching inside environments — chats and terminals")
            .visible(false)
            .build();
        searching.set_size_request(48, 4);
        header.append(&count);
        header.append(&quota);
        header.append(&searching);
        header.append(&start_button);
        header.append(&stop_button);
        header.append(&delete_button);

        // The composer (composer.rs): the chat's own field, chips and action
        // row, here with one pill, Create. Permanent, under the list, in the
        // rows' inset, and only ever for a NEW issue: editing happens on the
        // row (`edit_issue`), starting from the header.
        let composer = crate::composer::Composer::new(workspace, "Create", &[]);
        composer.primary.set_tooltip_text(Some(
            "Create this issue on the queue (Ctrl+Enter). Start it from the header once \
             it is written down.",
        ));
        composer.set_placeholder("Title, then details");
        composer.widget.set_margin_start(4);
        composer.widget.set_margin_end(4);
        composer.widget.set_margin_top(4);
        composer.widget.set_margin_bottom(6);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "backlog-list"])
            // A click selects; a double-click or Enter activates, which
            // opens the editor on the row. Renaming an issue is the most
            // common edit and should not need the menu.
            .activate_on_single_click(false)
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

        // The list and the composer fold under the header like any
        // section's body; the header's actions stay.
        // No expand of its own: the panel is pinned to the pane's bottom
        // because the file list above takes the slack. In gadget mode the
        // scroller expands (`set_filling`) and that propagates up through
        // this box; an explicit expand here made the panel take the slack
        // at full width and left its composer floating mid-pane.
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&overlay);
        body.append(&composer.widget);
        crate::filetree::wire_collapse(&header, &body);
        // Folded, the header keeps its facts and loses its actions: Start,
        // Stop and Delete act on the selected row, and a row nobody can see
        // is not one to act on (David, 2026-09-06).
        {
            let actions = [start_button.clone(), stop_button.clone(), delete_button.clone()];
            body.connect_visible_notify(move |body| {
                for action in &actions {
                    action.set_visible(body.is_visible());
                }
            });
        }

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("backlog-panel");
        widget.set_widget_name("backlog");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&header);
        widget.append(&body);

        let panel = Rc::new(Self {
            widget,
            count: count.clone(),
            query: RefCell::new(crate::search::Query::default()),
            inner_hits: RefCell::new(HashMap::new()),
            searching: searching.clone(),
            scroller,
            list: list.clone(),
            composer: composer.clone(),
            start_button: start_button.clone(),
            stop_button: stop_button.clone(),
            delete_button: delete_button.clone(),
            workspace: workspace.clone(),
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
            on_stop: RefCell::new(None),
            on_destroy: RefCell::new(None),
            on_tick: RefCell::new(None),
            on_step_hits: RefCell::new(None),
        });
        {
            // Selecting a row that is already selected fires nothing, so a
            // second click is caught here: on the aimed row with hits
            // inside, it steps through them.
            let weak = Rc::downgrade(&panel);
            let click = gtk::GestureClick::new();
            click.connect_released(move |gesture, _, _, y| {
                let Some(panel) = weak.upgrade() else { return };
                let Some(list) = gesture.widget().and_downcast::<gtk::ListBox>() else {
                    return;
                };
                let Some(row) = list.row_at_y(y as i32) else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let (env, id) = {
                    let listed = panel.listed.borrow();
                    let Some(listed) = listed.get(index as usize) else { return };
                    (listed.env.clone(), listed.id.clone())
                };
                let Some(env) = env else { return };
                if panel.current.borrow().as_ref() != Some(&env) {
                    return;
                }
                if panel.inner_hits.borrow().get(&id).copied().unwrap_or(0) == 0 {
                    return;
                }
                let hook = panel.on_step_hits.borrow();
                if let Some(hook) = hook.as_ref() {
                    hook(env);
                }
                drop(hook);
            });
            list.add_controller(click);
        }

        {
            let weak = Rc::downgrade(&panel);
            list.connect_row_activated(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let issue = panel
                    .listed
                    .borrow()
                    .get(index as usize)
                    .and_then(|listed| listed.issue.clone());
                if let Some(id) = issue {
                    panel.edit_issue(&id);
                }
            });
        }
        // Selecting a row is the gesture: the header's actions take it, and
        // a row with an environment aims the panes at it besides.
        {
            let weak = Rc::downgrade(&panel);
            list.connect_row_selected(move |_, row| {
                let Some(panel) = weak.upgrade() else { return };
                if panel.selecting.get() {
                    return;
                }
                let Some(row) = row else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let env = panel
                    .listed
                    .borrow()
                    .get(index as usize)
                    .and_then(|listed| listed.env.clone());
                panel.sync_actions();
                if let Some(env) = env {
                    panel.choose(&env);
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            start_button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.start_selected();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            stop_button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.stop_selected();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            delete_button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.delete_selected();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer.primary.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.submit();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer.set_on_change(move || {
                if let Some(panel) = weak.upgrade() {
                    panel.sync_composer();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            composer.set_on_notice(move |text| {
                if let Some(panel) = weak.upgrade() {
                    if let Some(toast) = panel.on_toast.borrow().as_ref() {
                        toast(text);
                    }
                }
            });
        }
        {
            // Ctrl+Enter files; Enter alone is a new line, because an issue
            // has a body.
            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&panel);
            keys.connect_key_pressed(move |_, key, _, state| {
                let enter = key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter;
                if enter && state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    if let Some(panel) = weak.upgrade() {
                        panel.submit();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            composer.entry.add_controller(keys);
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

    /// The header's Stop: the row's environment, for the console to stop.
    pub fn set_on_step_hits(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_step_hits.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_stop(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_stop.borrow_mut() = Some(Box::new(hook));
    }

    /// The header's Delete on a row with an environment: the console's
    /// destroy intervention, which asks first.
    pub fn set_on_destroy(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_destroy.borrow_mut() = Some(Box::new(hook));
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
        self.count.set_label(&summary_short(&rows));
        self.count.set_tooltip_text(Some(&format!(
            "{} — open, active, done, declined",
            summary(&rows)
        )));
        let query = self.query.borrow().clone();
        let inner = self.inner_hits.borrow().clone();

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut listed: Vec<Listed> = Vec::new();
        let mut current_row: Option<gtk::ListBoxRow> = None;
        // The selection follows the user; a rebuild puts it back on the row
        // they had, else on the row the panes are aimed at.
        let selected = self.selected_issue();
        for row in rows.iter() {
            let own = row_matches(row, &query);
            let within = inner.get(&row.id).copied().unwrap_or(0);
            if !own && within == 0 && !query.ghost && !query.is_empty() {
                continue;
            }
            let (widget, sparkline) = self.build_row(row, within);
            if !query.is_empty() && !own && within == 0 {
                widget.add_css_class("search-dim");
            }
            if within > 0 {
                widget.set_tooltip_text(Some(&format!(
                    "{}\n{within} match{} inside its environment — click again to step through them",
                    row.tooltip(),
                    if within == 1 { "" } else { "es" }
                )));
            }
            self.list.append(&widget);
            let is_selected = selected.as_deref() == Some(row.id.as_str());
            let is_aim = row.live.as_ref().is_some_and(|live| live.current);
            if is_selected || (selected.is_none() && is_aim) {
                current_row = Some(widget.clone());
            }
            listed.push(Listed {
                id: row.id.clone(),
                issue: row.is_issue().then(|| row.id.clone()),
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
        self.sync_composer();
        self.sync_actions();
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

    /// One row. `within` is the query's hit count inside the row's
    /// environment (its chat and terminals), worn as the flank's badge.
    fn build_row(self: &Rc<Self>, row: &Row, within: usize) -> (gtk::ListBoxRow, Option<Sparkline>) {
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
        // Line two: what the work is doing, and the marks that qualify it.
        let caption = gtk::Label::builder()
            .label(row.caption())
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .css_classes(["caption", "dim-label"])
            .build();
        let marks = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        marks.append(&caption);
        if within > 0 {
            marks.append(&crate::search::hit_badge(within));
        }
        let lines = gtk::Box::new(gtk::Orientation::Vertical, 0);
        lines.set_hexpand(true);
        lines.set_valign(gtk::Align::Center);
        lines.append(&label);
        lines.append(&marks);
        box_.append(&lines);

        let mut sparkline = None;
        if let Some(live) = &row.live {
            if live.awaits_user {
                marks.append(
                    &gtk::Box::builder()
                        .css_classes(["env-attention"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Its chat is waiting for your answer")
                        .build(),
                );
            }
            if live.current && !live.primary {
                marks.append(
                    &gtk::Image::builder()
                        .icon_name("system-lock-screen-symbolic")
                        .css_classes(["dim-label"])
                        .pixel_size(12)
                        .tooltip_text("Read-only: this is another environment's checkout")
                        .build(),
                );
            }
            if live.unpublished {
                marks.append(
                    &gtk::Box::builder()
                        .css_classes(["env-unpublished"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Work here that no other checkout has")
                        .build(),
                );
            }
            if let Some(icon) = live.review.icon() {
                marks.append(
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

        // Every row can be selected: the selection is what the composer is
        // on, and for a row with an environment also where the panes aim.
        let widget = gtk::ListBoxRow::builder()
            .child(&box_)
            .activatable(row.is_issue())
            .selectable(true)
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
            add_action("edit", true, Box::new(move || panel.edit_issue(&id)));
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
        *self.open_menu.borrow_mut() = Some(popover.clone().upcast::<gtk::Popover>().downgrade());
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

    /// The backlog's answer to the one query (search.rs): rows that match
    /// stay, rows with a hit inside their environment stay with a count,
    /// the rest hide — or dim, when the ghost is on.
    pub fn attach_search(self: &Rc<Self>, search: &Rc<crate::search::Search>) {
        let weak = Rc::downgrade(self);
        search.subscribe("backlog", move |query, _| {
            let Some(panel) = weak.upgrade() else { return };
            *panel.query.borrow_mut() = query.clone();
            panel.inner_hits.borrow_mut().clear();
            panel.searching.set_visible(false);
            panel.rerender();
        });
    }

    /// Hits inside environments (chats, terminals), as they land; `done`
    /// of `total` environments have answered. The rule in the header shows
    /// the progress and goes when the last one lands.
    pub fn set_inner_hits(
        self: &Rc<Self>,
        hits: HashMap<String, usize>,
        done: usize,
        total: usize,
    ) {
        if self.query.borrow().is_empty() {
            return;
        }
        let finished = done >= total;
        self.searching.set_value(if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        });
        self.searching.set_visible(!finished);
        if *self.inner_hits.borrow() != hits {
            *self.inner_hits.borrow_mut() = hits;
            self.rerender();
        }
    }

    /// Ctrl+Shift+I: dictate a new issue into the field, or stop and
    /// transcribe. The field is the new-issue field, so what is said becomes
    /// a title and a body the user reads before pressing Create.
    pub fn toggle_dictation(&self) {
        self.composer.toggle_dictation();
    }

    /// The issue the selected row is, if the selection is on one.
    fn selected_issue(&self) -> Option<String> {
        let row = self.list.selected_row()?;
        let index = usize::try_from(row.index()).ok()?;
        self.listed.borrow().get(index)?.issue.clone()
    }

    /// The header's three buttons, from the selected row: Start a queued
    /// issue, Stop a running environment, Delete any issue.
    fn sync_actions(&self) {
        let selected = self.selected_issue();
        let shown = self.shown.borrow();
        let row = selected
            .as_deref()
            .and_then(|id| shown.iter().find(|row| row.id == id));
        let startable = row.is_some_and(|row| row.work == WorkState::Queued && row.live.is_none());
        let stoppable = row.is_some_and(|row| {
            row.live
                .as_ref()
                .is_some_and(|live| matches!(live.light, Light::Green | Light::Amber))
        });
        self.start_button.set_sensitive(startable);
        self.stop_button.set_sensitive(stoppable);
        self.delete_button.set_sensitive(row.is_some());
    }

    fn start_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_issue() else {
            return;
        };
        let issues = self.issues.borrow();
        let Some(issue) = issues.iter().find(|issue| issue.id == id) else {
            return;
        };
        let (title, body) = (issue.title.clone(), issue.body.clone());
        drop(issues);
        self.start(id, title, body);
    }

    fn stop_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_issue() else {
            return;
        };
        let env = self
            .listed
            .borrow()
            .iter()
            .find(|row| row.issue.as_deref() == Some(id.as_str()))
            .and_then(|row| row.env.clone());
        if let (Some(env), Some(hook)) = (env, self.on_stop.borrow().as_ref()) {
            hook(env);
        }
    }

    /// Delete asks on the row (the inline "Delete?") for an issue with no
    /// environment; an issue that has one is destroyed through the
    /// console's intervention, which names what the clone holds first.
    fn delete_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_issue() else {
            return;
        };
        let env = self
            .listed
            .borrow()
            .iter()
            .find(|row| row.issue.as_deref() == Some(id.as_str()))
            .and_then(|row| row.env.clone());
        match env {
            Some(env) => {
                if let Some(hook) = self.on_destroy.borrow().as_ref() {
                    hook(env);
                }
            }
            None => {
                *self.confirming.borrow_mut() = Some(id);
                self.rerender();
            }
        }
    }

    /// The menu's Edit: the composer again, in a popover on the row, with
    /// the issue's text in it and Save as its pill. The permanent field
    /// below the list is never repurposed for this — it is where a new
    /// issue is being written, possibly right now.
    fn edit_issue(self: &Rc<Self>, id: &str) {
        let index = self
            .listed
            .borrow()
            .iter()
            .position(|row| row.issue.as_deref() == Some(id));
        let Some(anchor) = index.and_then(|i| self.list.row_at_index(i as i32)) else {
            return;
        };
        let text = {
            let issues = self.issues.borrow();
            let Some(record) = issues.iter().find(|record| record.id == id) else {
                return;
            };
            if record.body.trim().is_empty() {
                record.title.clone()
            } else {
                format!("{}\n\n{}", record.title, record.body)
            }
        };
        self.close_context_menu();

        let cancel = gtk::Button::builder().label("Cancel").build();
        let composer =
            crate::composer::Composer::new(&self.workspace, "Save", std::slice::from_ref(&cancel));
        composer.set_text(&text);
        composer.set_primary_ready(true);
        composer.widget.set_size_request(300, -1);
        let heading = gtk::Label::builder()
            .label(format!("Editing {id}"))
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);
        content.append(&heading);
        content.append(&composer.widget);
        let popover = gtk::Popover::builder()
            .child(&content)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .build();
        popover.set_parent(&anchor);
        popover.set_widget_name("backlog-editor");
        {
            let weak_composer = Rc::downgrade(&composer);
            composer.set_on_change(move || {
                if let Some(composer) = weak_composer.upgrade() {
                    let ready = split_issue_text(&composer.text()).is_some();
                    composer.set_primary_ready(ready);
                }
            });
        }
        {
            let popover = popover.clone();
            cancel.connect_clicked(move |_| popover.popdown());
        }
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            let editor = composer.clone();
            let id = id.to_string();
            composer.primary.connect_clicked(move |_| {
                let Some(panel) = weak.upgrade() else { return };
                let Some((title, body)) = split_issue_text(&editor.text()) else {
                    return;
                };
                let attachments: Vec<NewAttachment> = editor
                    .take_attachments()
                    .into_iter()
                    .filter_map(|attachment| attachment.as_file())
                    .map(|(name, bytes)| NewAttachment { name, bytes })
                    .collect();
                popover.popdown();
                panel.edit(id.clone(), title, body, attachments, false);
            });
        }
        {
            let keys = gtk::EventControllerKey::new();
            let primary = composer.primary.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let enter = key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter;
                if enter && state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    if primary.is_sensitive() {
                        primary.emit_clicked();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            composer.entry.add_controller(keys);
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
                    if let Some(panel) = weak.upgrade() {
                        if panel.render_deferred.replace(false) {
                            panel.rerender();
                        }
                    }
                });
            });
        }
        // Parked in the same slot as the context menu, so a fleet tick does
        // not rebuild the row this is parented to while it is up.
        *self.open_menu.borrow_mut() = Some(popover.downgrade());
        popover.popup();
        composer.entry.grab_focus();
    }

    /// Whether File may act: a title, or something attached.
    fn sync_composer(&self) {
        let ready = split_issue_text(&self.composer.text()).is_some()
            || self.composer.attachment_count() > 0;
        self.composer.set_primary_ready(ready);
    }

    /// File what the composer holds as a new issue.
    fn submit(self: &Rc<Self>) {
        let text = self.composer.text();
        let (title, body) = match split_issue_text(&text) {
            Some(parts) => parts,
            None if self.composer.attachment_count() > 0 => {
                ("Attachment".to_string(), String::new())
            }
            None => return,
        };
        let attachments: Vec<NewAttachment> = self
            .composer
            .take_attachments()
            .into_iter()
            .filter_map(|attachment| attachment.as_file())
            .map(|(name, bytes)| NewAttachment { name, bytes })
            .collect();
        self.composer.clear();
        self.create(title, body, attachments, false);
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

    fn create(
        self: &Rc<Self>,
        title: String,
        body: String,
        attachments: Vec<NewAttachment>,
        start: bool,
    ) {
        let (for_write, for_start) = ((title.clone(), body.clone()), (title, body));
        self.write_then(
            None,
            move |git| {
                // The reporter is the user's own checkout: this composer is
                // in the user's window, and attributing it to an agent's
                // environment would be a lie the issue carries forever.
                git.issue_create_with(&for_write.0, &for_write.1, &[], "primary", &attachments)
                    .map(|issue| issue.id)
            },
            move |panel, id| {
                if start {
                    panel.start(id, for_start.0, for_start.1);
                }
            },
        );
    }

    fn edit(
        self: &Rc<Self>,
        id: String,
        title: String,
        body: String,
        attachments: Vec<NewAttachment>,
        start: bool,
    ) {
        let (for_write, for_start) = ((title.clone(), body.clone()), (title, body));
        let for_hook = id.clone();
        self.write_then(
            None,
            move |git| {
                let target = git.issue_target_branch();
                let change = taste_git::IssueChange {
                    title: Some(for_write.0),
                    body: Some(for_write.1),
                    attach: attachments,
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
    /// TASTE_PROBE_CHECK only: the editor popover on one row.
    pub fn seed_editor_for_probe(self: &Rc<Self>, id: &str) {
        self.edit_issue(id);
    }

    pub fn seed_composer_for_probe(self: &Rc<Self>) {
        self.composer.set_text(
            "Relocation waits for the container\n\nOpening a chat in a stopped environment \
             must not try to relocate: the agent starts outside and moves in when the \
             container comes up.",
        );
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
            agent: None,
            model: None,
            created: 0,
            updated: 0,
            labels: Vec::new(),
            links: Vec::new(),
            body: String::new(),
            comments: Vec::new(),
            attachments: Vec::new(),
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
        assert_eq!(
            summary_short(&rows(&issues(), &fleet, None)),
            "3 · 1 active",
            "the header keeps what fits beside three buttons"
        );
        let open = vec![issue("i-0001", "One", Resolution::Open, None)];
        assert_eq!(summary(&rows(&open, &fleet, None)), "1");
    }

    #[test]
    fn the_query_matches_title_id_and_body_and_never_hides_the_way_home() {
        use crate::search::Query;
        let mut with_body = issues();
        with_body[1].body = "the sparkline flickers on rebuild".into();
        let rows = rows(&with_body, &fleet(vec![facts("primary", running())]), None);
        assert!(
            row_matches(&rows[0], &Query::new("zzz")),
            "the primary row always shows"
        );
        let composer = rows.iter().find(|row| row.id == "i-0007").unwrap();
        assert!(
            row_matches(composer, &Query::new("draft"))
                && row_matches(composer, &Query::new("0007"))
        );
        assert!(
            row_matches(composer, &Query::new("flickers")),
            "the body is searched"
        );
        assert!(!row_matches(composer, &Query::new("varlink")));
        assert!(
            row_matches(composer, &Query::new("  ")),
            "blank is no filter"
        );
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
    fn the_second_line_says_what_the_work_is_doing() {
        let fleet = fleet(vec![
            facts("primary", running()),
            facts("i-0007", running()),
        ]);
        let rows = rows(&issues(), &fleet, None);
        let by = |id: &str| rows.iter().find(|row| row.id == id).unwrap();
        assert!(
            by("i-0007").caption().contains("running"),
            "{}",
            by("i-0007").caption()
        );
        assert!(by("i-0009").caption().starts_with("queued · "));
        assert!(by("i-0004").caption().starts_with("completed · "));
        assert!(by("i-0011").caption().starts_with("declined"));
        assert_eq!(
            by("i-0002").caption(),
            "started by d@laptop · not on this machine"
        );
    }

    #[test]
    fn the_first_line_is_the_title_and_the_rest_is_the_body() {
        assert_eq!(
            split_issue_text("Fix the gauge\n\nIt reads 0% when stale.\nAlways."),
            Some((
                "Fix the gauge".into(),
                "It reads 0% when stale.\nAlways.".into()
            ))
        );
        assert_eq!(
            split_issue_text("\n  Only a title  \n"),
            Some(("Only a title".into(), String::new()))
        );
        assert_eq!(split_issue_text("  \n\n"), None);
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
