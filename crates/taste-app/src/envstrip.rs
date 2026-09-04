//! The environment panel: where the window says which worlds exist, which
//! one you are in, and what is happening in the rest.
//!
//! ENVIRONMENTS.md → "Watching an environment". Aiming the panes at
//! another environment changes the meaning of every other pane — the tree,
//! the git views, what a save is allowed to do — so it is top-level
//! context, and top-level context gets a permanent place rather than a
//! control buried in a tab. The panel is pinned to the very bottom of the
//! file-tree pane, below the intervention panel, and is the only indicator
//! of where the panes are aimed (VS Code's remote-indicator corner is the
//! acknowledged precedent).
//!
//! **Every environment is a row, and every row is always visible.** It was
//! one row plus a popover switcher, which meant the fleet existed only
//! while a menu was open: switching cost a click to reveal and a click to
//! choose, and between them the panel could not say that another
//! environment was building, or waiting on you, or had gone down. A
//! persistent list costs vertical space in the tree flank and buys back
//! both — one click to switch, and a fleet you can see without asking.
//! The popover is deleted rather than kept dormant; the filter it grew past
//! six environments survives, in the panel, under the same rule.
//!
//! Each row carries the two things a glance is for — plus, when there is
//! one, the sentence they are in service of:
//!
//! - a **traffic light** ([`crate::fleet::Light`]) — green means work can
//!   happen here, amber means it wants you, red means nothing runs. The
//!   mapping lives in `fleet.rs` beside the assembly, because a panel that
//!   coloured its own dots from the same seven supervisor states would be a
//!   second state machine to keep in agreement with the fleet view's.
//! - a **sparkline** ([`crate::sparkline`]) — five minutes of
//!   [`taste_core::activity`], because a state cannot distinguish an
//!   environment that is up and hammering from one that is up and idle.
//!
//! And under the name, when the environment holds a claim, **what it is
//! working on**: the issue's title, dim, one line. This is the panel's
//! side of the env↔issue link, and the reason the backlog below stopped
//! drawing the other side. Both panels used to draw it — a queue row
//! naming an environment eight pixels under an environment row that would
//! have named the issue — and of the two directions this is the one worth
//! the pixels. "What is `calm-1` doing" is the question you look at this
//! panel to answer. "Which world has i-0007" is a question about one
//! issue, and a tooltip on that issue's state glyph is the right size of
//! answer for it.
//!
//! Two *signals* per row and no more — the work line is a sentence, not a
//! signal, and is read rather than glanced at. The switcher's busy spinner
//! did not survive the move: a row is about a hundred and eighty pixels
//! wide, a spinner is a permanently animating element in the corner of the
//! eye, and in any still frame it draws as a half-finished ring that reads as
//! breakage. What it said, a live sparkline says better. The honest cost
//! is stated rather than hidden: a chat that is thinking without producing
//! anything draws as an idle row here, so `busy` reaches the reader
//! through the row's tooltip, and the fleet view — which has the width for
//! a column — keeps its spinner.
//!
//! The one badge that joins them is *waiting on you*. It is not a third
//! reading of activity but the opposite of activity — a chat that will not
//! move again until a person answers — and the light cannot say it alone,
//! because amber is also "rebuilding" and also "safe mode (baseline)", a
//! steady state a whole fleet can sit in. So an unanswered question gets a
//! mark of its own, and it is the only thing on a row that is urgent
//! rather than informative.
//!
//! The file keeps the name `envstrip.rs`, and `TASTE_PROBE_VIEW=envstrip`
//! keeps its name too: the anchor and the probe target are the stable
//! handles here, and renaming them would only make every existing
//! reference to this surface wrong.
//!
//! It renders [`FleetRow`]s and derives nothing of its own: the console
//! assembles the fleet from the six places an environment's facts live,
//! and this is one more renderer of those same rows — a panel that
//! disagreed with the fleet view about what an environment is called or
//! whether it is running would be worse than no panel.
//!
//! Everything above [`EnvPanel`] is pure and tested: what the panel says
//! about where you are, what the list contains, and when the list is long
//! enough to earn a filter.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use taste_core::activity::{Activity, BUCKETS};
use taste_core::environment::EnvironmentId;
use taste_core::quota::{describe_age, describe_countdown, QuotaSnapshot};

use crate::fleet::{FleetRow, Light, ReviewMark};
use crate::sparkline::Sparkline;

/// What the user's own checkout is called, everywhere the UI has to name
/// it. The git interface already says "Keep Yours" of the user's side of a
/// conflict; the primary environment is the same "yours".
/// What the header gauge says when you rest on it.
///
/// Written to be readable as prose rather than as a readout, because the
/// one thing it must get across is not a number: these figures are as of
/// a moment that has passed, and the pool they describe is shared with
/// the user's own Claude use. A tooltip that listed percentages without
/// saying when would be a more confident lie than saying nothing.
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
        // No plan window was reported, so the headline came off a
        // per-minute rate limit. Say which — it is a real limit, but it
        // is not the plan, and letting it wear the plan's clothes would
        // be the whole feature quietly lying.
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

pub const PRIMARY_TITLE: &str = "Yours";

/// Past this many environments the panel grows a filter. Two or three
/// environments are read, not searched, and a search entry over them is
/// chrome that costs a row of height and gives nothing back. Past it, the
/// list is scrolling anyway and reading has stopped working.
pub const FILTER_THRESHOLD: usize = 6;

/// How many rows the panel is willing to be tall. Past this it scrolls
/// inside itself: the file tree is the pane's job, and a panel that grows
/// to eat it has mistaken which one the user opened the window for.
pub const VISIBLE_ROWS: i32 = 6;

/// One row's height, for the scroller's ceiling. Not a layout constraint —
/// rows size themselves from their content — just the arithmetic behind
/// "about six rows".
const ROW_HEIGHT: i32 = 30;

/// What a row costs on top of [`ROW_HEIGHT`] when it carries a work line.
///
/// The caption is a second line of text under the name, so a row with one
/// is taller — and the ceiling has to know, or "about six rows" quietly
/// becomes "about four" the moment the fleet is doing anything. See
/// [`list_height`].
const CAPTION_HEIGHT: i32 = 15;

/// How tall the list should be allowed to get: the natural height of the
/// first [`VISIBLE_ROWS`] rows, whatever those rows happen to be.
///
/// A flat `VISIBLE_ROWS * ROW_HEIGHT` was right while every row was one
/// line. It is wrong now: it would cap the panel at six *short* rows'
/// worth of pixels, so a fleet where four environments are working — the
/// case the work line exists for — would scroll at four. The ceiling is a
/// promise about how many rows you can see, not about pixels, so it is
/// measured in the rows that are actually there.
fn list_height<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> i32 {
    entries
        .into_iter()
        .take(VISIBLE_ROWS as usize)
        .map(|entry| {
            ROW_HEIGHT
                + if entry.working_on.is_some() {
                    CAPTION_HEIGHT
                } else {
                    0
                }
        })
        .sum::<i32>()
        .max(ROW_HEIGHT)
}

/// How often the panel re-reads the world: the sparklines' redraw and the
/// fleet's own refresh, coalesced onto one timer.
///
/// 1 Hz, and it must stay coarse. A bucket is five seconds
/// ([`taste_core::activity::BUCKET`]) so nothing finer would draw
/// differently, and everything this tick does is bounded and equality-
/// guarded: the fleet assembly returns early when nothing moved, and a
/// sparkline redraws only when its samples changed. An idle fleet costs one
/// wakeup a second and no frames.
const TICK: Duration = Duration::from_secs(1);

/// What the panel itself shows about the context you are in, and whether
/// that is home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    /// The environment's name, or [`PRIMARY_TITLE`] for the user's own.
    pub title: String,
    pub light: Light,
    /// The view is read-only — true of every non-primary environment.
    pub locked: bool,
    /// Tint the panel: you are not home, and peripheral vision should say
    /// so before anything is read.
    pub away: bool,
    /// The tooltip: the same fact, spelled out.
    pub detail: String,
}

/// One row of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub env: EnvironmentId,
    pub title: String,
    pub light: Light,
    /// The user's own checkout — the row that is the way back.
    pub primary: bool,
    /// Its chat is mid-turn. Carried, and deliberately not drawn — see
    /// the module doc: it reaches the reader through the tooltip.
    pub busy: bool,
    /// Its chat is stopped on a question only the user can answer. The
    /// panel is where a conversation the user cannot see asks for them, so
    /// this is the marker that has to carry weight.
    pub awaits_user: bool,
    /// It holds work no other checkout has a copy of.
    pub unpublished: bool,
    /// Where it stands in the review arc, as a list marks it. The second
    /// mark on a row, and deliberately not a fourth light — see
    /// [`ReviewMark`]. Order is untouched: a row that jumped to the top
    /// when an agent finished would move the list under the pointer.
    pub review: ReviewMark,
    /// The panes are aimed here.
    pub current: bool,
    /// The state, for the row's tooltip.
    pub detail: String,
    /// The issue this environment holds a claim on, when it holds one.
    ///
    /// **This is the panel's half of the env↔issue link, and it is the
    /// half that is worth drawing.** A backlog row says what state an
    /// issue is in; a row here says what a world is *doing*, which is the
    /// question you open this panel with. It used to be readable only the
    /// other way round — the queue named the environment — and that put
    /// the answer in the panel you were not looking at.
    pub working_on: Option<taste_git::Claim>,
}

impl Entry {
    /// The row's tooltip: what it is, what state it is in, and — when it is
    /// the reason the light is amber — that it is waiting on the reader.
    pub fn tooltip(&self) -> String {
        let mut text = if self.primary {
            format!("{} — your own checkout\n{}", self.title, self.detail)
        } else {
            format!(
                "{} — its own clone and devcontainer, read-only to you\n{}",
                self.title, self.detail
            )
        };
        // What it is working on, in full: the row's caption is one
        // ellipsized line, and the id is what a person types into a chat
        // message.
        if let Some(claim) = &self.working_on {
            text.push_str(&format!("\nWorking on {} — {}", claim.id, claim.title));
        }
        if self.awaits_user {
            text.push_str("\nIts chat is waiting for an answer from you.");
        } else if self.busy {
            text.push_str("\nIts chat is working now.");
        }
        // Said last because it is the sentence that outranks the rest: an
        // environment that has finished is not waiting on its chat, it is
        // waiting on a person, and the container being down is a
        // consequence rather than a fault.
        match self.review {
            ReviewMark::Flagged => text.push_str(
                "\nIt says it is done and is waiting for your review. Its container was \
                 stopped because nothing is left to run in it.",
            ),
            ReviewMark::Settled => {
                text.push_str("\nYou have ruled on this one — it is safe to destroy.")
            }
            ReviewMark::None => {}
        }
        text
    }
}

/// Which row the panes are aimed at. `None` — the tree's way of saying
/// "the user's own checkout" — is the primary row.
fn is_current(row: &FleetRow, current: Option<&EnvironmentId>) -> bool {
    match current {
        Some(env) => row.env == *env,
        None => row.primary,
    }
}

/// What one environment is called in this UI. The primary is "Yours"
/// wherever it appears, because "primary" is the registry's word for it
/// and the user's word is the one on screen.
pub fn title_of(row: &FleetRow) -> String {
    if row.primary {
        PRIMARY_TITLE.to_string()
    } else {
        row.name.clone()
    }
}

/// What the panel says about where you are, given the fleet and where the
/// panes are aimed.
pub fn face(rows: &[FleetRow], current: Option<&EnvironmentId>) -> Face {
    let Some(row) = rows.iter().find(|row| is_current(row, current)) else {
        // Before the first assembly — and for the moment after an
        // environment is destroyed out from under the view — say where we
        // are and admit the state is not known yet, rather than claiming a
        // state the fleet has not confirmed.
        let title = match current {
            Some(env) => env.to_string(),
            None => PRIMARY_TITLE.to_string(),
        };
        return Face {
            detail: format!("{title} · state not known yet"),
            title,
            light: Light::Unknown,
            locked: current.is_some(),
            away: current.is_some(),
        };
    };
    let title = title_of(row);
    let away = !row.primary;
    let detail = if away {
        format!(
            "Viewing {title} — read-only, it is another environment's checkout\n{}",
            row.state_text()
        )
    } else {
        format!("Your own checkout\n{}", row.state_text())
    };
    Face {
        title,
        light: row.light(),
        locked: away,
        away,
        detail,
    }
}

/// The panel's rows, in the order the fleet assembled them: the primary
/// first — it is the way back — then by what the others are called. That
/// order is [`crate::fleet::assemble`]'s, and is deliberately not
/// re-derived here.
pub fn entries(rows: &[FleetRow], current: Option<&EnvironmentId>) -> Vec<Entry> {
    rows.iter()
        .map(|row| Entry {
            title: title_of(row),
            light: row.light(),
            primary: row.primary,
            busy: row.chat.as_ref().is_some_and(|chat| chat.busy),
            awaits_user: row.awaits_user(),
            unpublished: row.has_unpublished_work(),
            review: row.review_mark(),
            current: is_current(row, current),
            detail: row.state_text(),
            // The first claim only. An environment with two is rare and
            // the row has one line to spend; the count reaches the reader
            // through the tooltip, and the console header spells the lot.
            working_on: row.working_on.first().cloned(),
            env: row.env.clone(),
        })
        .collect()
}

/// Whether the panel shows its filter. Only past [`FILTER_THRESHOLD`].
pub fn filter_visible(count: usize) -> bool {
    count > FILTER_THRESHOLD
}

/// Type-to-filter: case-insensitive substring, over what the row shows and
/// over the slug behind it — a user who typed `calm-1` means that row even
/// when it has been renamed.
pub fn matches(entry: &Entry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    entry.title.to_lowercase().contains(&query)
        || entry.env.as_str().to_lowercase().contains(&query)
}

/// How the panel asks the window to aim the panes somewhere.
pub type SelectHook = Box<dyn Fn(EnvironmentId)>;
/// How the panel's header button creates an environment. Takes the button,
/// which goes insensitive while the clone runs.
pub type NewEnvironmentHook = Box<dyn Fn(gtk::Button)>;
/// Called on the panel's own tick, so the list says what is true now.
pub type RefreshHook = Box<dyn Fn()>;

/// One row's widgets, kept so the tick can update them without rebuilding
/// the list. Rebuilding once a second would drop the user's focus and
/// restart every spinner.
struct Row {
    env: EnvironmentId,
    widget: gtk::ListBoxRow,
    sparkline: Sparkline,
}

pub struct EnvPanel {
    /// The panel itself: a permanent child at the bottom of the file-tree
    /// pane, below the intervention panel.
    pub widget: gtk::Box,
    scroller: gtk::ScrolledWindow,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    activity: Activity,
    rows: RefCell<Vec<FleetRow>>,
    current: RefCell<Option<EnvironmentId>>,
    /// The rows on screen, in the order they are on screen. Rebuilt only
    /// when the entries change, which is what makes the tick cheap.
    listed: RefCell<Vec<Row>>,
    /// What the list was built from. The rebuild guard: a fleet refresh
    /// that only moved a disk figure must not rebuild a list that would
    /// read identically.
    shown: RefCell<Vec<Entry>>,
    /// Suppresses the activation that `select_row` would otherwise provoke
    /// while the panel is aiming itself at the current environment.
    selecting: std::cell::Cell<bool>,
    /// The subscription gauge in the header, and what it is drawing.
    quota: gtk::Box,
    quota_bar: gtk::LevelBar,
    quota_snapshot: RefCell<QuotaSnapshot>,
    /// The tooltip last set, so a tick that would rewrite it identically
    /// does not.
    quota_tooltip: RefCell<String>,
    /// TASTE_PROBE_CHECK only: fabricated activity windows, consulted in
    /// place of the live sampler for the environments they name.
    probe_activity: RefCell<std::collections::BTreeMap<EnvironmentId, [u16; BUCKETS]>>,
    on_select: RefCell<Option<SelectHook>>,
    on_new_environment: RefCell<Option<NewEnvironmentHook>>,
    on_refresh: RefCell<Option<RefreshHook>>,
}

impl EnvPanel {
    pub fn new(activity: Activity) -> Rc<Self> {
        // The header names the surface and holds the one action that is not
        // "go somewhere". With a single environment the panel would
        // otherwise be one unlabelled row, which reads as a fragment rather
        // than as a fleet of one.
        let title = gtk::Label::builder()
            .label("Environments")
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        let new_button = gtk::Button::builder()
            // The icon is built by hand rather than set by name so it can
            // be dimmed: at full strength a white glyph is the brightest
            // thing in the panel, and it is the least important.
            .child(
                &gtk::Image::builder()
                    .icon_name("list-add-symbolic")
                    .css_classes(["dim-label"])
                    .pixel_size(14)
                    .build(),
            )
            .css_classes(["flat", "circular", "env-new"])
            .tooltip_text(
                "Clone the workspace into a new environment. It gets its own \
                 checkout and devcontainer, and no chat until you give it one.",
            )
            .build();
        // The pool every row in this panel spends out of. It belongs in
        // the header and not in the rows because it is not per
        // environment: one subscription, one set of windows, shared with
        // whatever the user is doing in Claude themselves. A row-level
        // copy would be the same number four times.
        //
        // Hidden until something has been observed. There is no reading
        // without traffic, and an empty bar would read as "nothing spent"
        // rather than as "nothing known".
        // The same gauge the chat header draws for its context window
        // (`crate::gauge`): one width, traffic-light colours, and no
        // percentage beside it — the number is in the tooltip. It used to
        // spell "68%" next to a bar that already said so, in a header with
        // room for neither.
        let quota_bar = crate::gauge::new();
        let quota = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        quota.append(&quota_bar);

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["env-panel-header"])
            .build();
        header.append(&title);
        header.append(&quota);
        header.append(&new_button);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter environments…")
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(4)
            .visible(false)
            .build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar", "env-list"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            // Past six rows the panel scrolls rather than growing into the
            // file tree above it.
            .max_content_height(VISIBLE_ROWS * ROW_HEIGHT)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("env-panel");
        // A probe target of its own: `filetree.envpanel` (ui_probe.rs).
        widget.set_widget_name("envpanel");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&header);
        widget.append(&search);
        widget.append(&scroller);

        let panel = Rc::new(Self {
            widget,
            scroller: scroller.clone(),
            search: search.clone(),
            list: list.clone(),
            activity,
            rows: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            listed: RefCell::new(Vec::new()),
            shown: RefCell::new(Vec::new()),
            selecting: std::cell::Cell::new(false),
            quota: quota.clone(),
            quota_bar: quota_bar.clone(),
            quota_snapshot: RefCell::new(QuotaSnapshot::default()),
            quota_tooltip: RefCell::new(String::new()),
            probe_activity: RefCell::new(std::collections::BTreeMap::new()),
            on_select: RefCell::new(None),
            on_new_environment: RefCell::new(None),
            on_refresh: RefCell::new(None),
        });

        // One activation path for the pointer and the keyboard alike: a
        // ListBox activates its row on a click and on Enter.
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
                .map(|row| row.env.clone());
            if let Some(env) = env {
                panel.choose(&env);
            }
        });

        let weak = Rc::downgrade(&panel);
        search.connect_search_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.rebuild(true);
            }
        });
        // Down out of the filter and into the list, the way every
        // type-to-filter list behaves.
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
        // Enter in the filter takes the first row that survived it.
        let weak = Rc::downgrade(&panel);
        search.connect_activate(move |_| {
            let Some(panel) = weak.upgrade() else { return };
            let first = panel.listed.borrow().first().map(|row| row.env.clone());
            if let Some(env) = first {
                panel.choose(&env);
            }
        });

        let weak = Rc::downgrade(&panel);
        new_button.connect_clicked(move |button| {
            let Some(panel) = weak.upgrade() else { return };
            let hook = panel.on_new_environment.borrow();
            if let Some(hook) = hook.as_ref() {
                hook(button.clone());
            }
        });

        // The one timer. Everything it does is bounded and guarded; see
        // TICK.
        let weak = Rc::downgrade(&panel);
        glib::timeout_add_local(TICK, move || {
            let Some(panel) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            panel.tick();
            glib::ControlFlow::Continue
        });

        panel
    }

    /// Aim the panel: which environment the panes are on (`None` = the
    /// user's own). Called by the file tree, which is where that fact
    /// changes — never by an event.
    pub fn set_current(self: &Rc<Self>, current: Option<EnvironmentId>) {
        if *self.current.borrow() == current {
            return;
        }
        *self.current.borrow_mut() = current;
        self.apply_face();
        self.rebuild(false);
    }

    /// The assembled fleet. Cheap and coarse: it lands on fleet changes,
    /// does nothing when the rows it is handed are the ones it has, and
    /// rebuilds the list only when what the list would SAY has changed.
    pub fn set_rows(self: &Rc<Self>, rows: &[FleetRow]) {
        if self.rows.borrow().as_slice() == rows {
            return;
        }
        *self.rows.borrow_mut() = rows.to_vec();
        // Bounded memory: a destroyed environment's activity ring goes with
        // it, and nothing else ever removes a key.
        let live: Vec<EnvironmentId> = rows.iter().map(|row| row.env.clone()).collect();
        self.activity.retain(&live);
        self.apply_face();
        self.rebuild(false);
    }

    pub fn set_on_select(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_new_environment(&self, hook: impl Fn(gtk::Button) + 'static) {
        *self.on_new_environment.borrow_mut() = Some(Box::new(hook));
    }

    pub fn set_on_refresh(&self, hook: impl Fn() + 'static) {
        *self.on_refresh.borrow_mut() = Some(Box::new(hook));
    }

    /// The keyboard's way in (Ctrl+Shift+E). Nothing opens, because there
    /// is nothing to open: the first press puts focus on the row the panes
    /// are aimed at, and pressing again walks down the list, wrapping.
    /// Enter on a focused row is what actually switches — so the shortcut
    /// is safe to lean on, and the arrow keys work from there.
    pub fn focus(self: &Rc<Self>) {
        // Past the threshold the filter is the fastest way through a long
        // list, so that is where the first press goes.
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
            // Not in the panel yet: land on where the panes actually are.
            None => {
                let aimed = self.aimed_at();
                self.listed
                    .borrow()
                    .iter()
                    .position(|row| Some(&row.env) == aimed.as_ref())
                    .unwrap_or(0) as i32
            }
        };
        if let Some(row) = self.list.row_at_index(target) {
            row.grab_focus();
        }
    }

    /// The environment the panes are aimed at, as a row identity — the
    /// primary's own id rather than `None`, which is the tree's spelling.
    fn aimed_at(&self) -> Option<EnvironmentId> {
        match self.current.borrow().clone() {
            Some(env) => Some(env),
            None => self
                .rows
                .borrow()
                .iter()
                .find(|row| row.primary)
                .map(|row| row.env.clone()),
        }
    }

    fn face(&self) -> Face {
        face(&self.rows.borrow(), self.current.borrow().as_ref())
    }

    /// The panel-level half of the face: the tint that says you are not
    /// home. The per-row half is in [`EnvPanel::build_row`].
    fn apply_face(&self) {
        let face = self.face();
        if face.away {
            self.widget.add_css_class("away");
        } else {
            self.widget.remove_css_class("away");
        }
    }

    /// Once a second: ask for a fresh fleet, then repaint the sparklines.
    ///
    /// The refresh hook is what keeps a persistent list honest. The fleet
    /// is otherwise reassembled on devcontainer events and completed git
    /// passes, and neither fires when a chat merely starts streaming — with
    /// a popover that did not matter, because the list was built at the
    /// moment it opened. A list that is always on screen has no such
    /// moment, so it takes one every second. The assembly is pure and
    /// equality-guarded, so a fleet that did not move costs one comparison.
    fn tick(self: &Rc<Self>) {
        if let Some(refresh) = self.on_refresh.borrow().as_ref() {
            refresh();
        }
        self.draw_activity();
        // The snapshot itself only changes when a turn finishes, but how
        // old it is changes every second — and that is half of what this
        // gauge is claiming. Redrawn here so "4 min ago" becomes "5 min
        // ago" without anyone having to spend a token to make it.
        self.draw_quota();
    }

    /// The account's limit state, from the console's read of the proxy.
    pub fn set_quota(self: &Rc<Self>, snapshot: &QuotaSnapshot) {
        if *self.quota_snapshot.borrow() == *snapshot {
            return;
        }
        *self.quota_snapshot.borrow_mut() = snapshot.clone();
        self.draw_quota();
    }

    /// Draw the header gauge, or hide it.
    ///
    /// Three states, and the distinction between the first two is the
    /// whole point of the feature: nothing observed yet is not the same
    /// as nothing spent. Only a snapshot that actually carries a
    /// utilization gets a bar; one that carried only headers we could not
    /// make a fraction of gets nothing rather than a guess.
    fn draw_quota(self: &Rc<Self>) {
        let snapshot = self.quota_snapshot.borrow();
        let now = std::time::SystemTime::now();
        let Some(headline) = snapshot.headline(now) else {
            self.quota.set_visible(false);
            return;
        };

        let spent = snapshot.current_exhaustion(now).is_some();
        let stale = snapshot.is_stale(now);
        // Colour and thresholds are the gauge's (`crate::gauge`), shared
        // with the chat's, so the two never disagree about whether the
        // user should be worried.
        crate::gauge::set(&self.quota_bar, headline.used, spent, stale);

        let tooltip = quota_tooltip(&snapshot, now);
        if *self.quota_tooltip.borrow() != tooltip {
            self.quota.set_tooltip_text(Some(&tooltip));
            *self.quota_tooltip.borrow_mut() = tooltip;
        }
        self.quota.set_visible(true);
    }

    /// The tick's drawing half, without the fleet refresh — also called
    /// after a rebuild, so new rows arrive already carrying their history
    /// instead of flashing an empty line for up to a second.
    fn draw_activity(self: &Rc<Self>) {
        for row in self.listed.borrow().iter() {
            let samples = self.samples_for(&row.env);
            row.sparkline.set_samples(&samples);
            row.widget
                .set_tooltip_text(Some(&self.row_tooltip(&row.env, &samples)));
        }
    }

    /// One row's window: the live sampler, or the probe's fabrication when
    /// TASTE_PROBE_CHECK has planted one for this environment.
    fn samples_for(&self, env: &EnvironmentId) -> [u16; BUCKETS] {
        if let Some(samples) = self.probe_activity.borrow().get(env) {
            return *samples;
        }
        self.activity.samples(env)
    }

    /// A row's full tooltip: what the entry says, plus what its sparkline
    /// is. Recomputed on the tick because the activity half of it moves.
    fn row_tooltip(&self, env: &EnvironmentId, samples: &[u16; BUCKETS]) -> String {
        let entry = self
            .shown
            .borrow()
            .iter()
            .find(|entry| entry.env == *env)
            .cloned();
        match entry {
            Some(entry) => format!("{}\n{}", entry.tooltip(), Sparkline::describe(samples)),
            None => Sparkline::describe(samples),
        }
    }

    /// Rebuild the list, if what it would say has changed.
    ///
    /// `force` is for the filter, whose text is not part of an entry: the
    /// entries are identical and the list still has to shrink.
    fn rebuild(self: &Rc<Self>, force: bool) {
        let entries = entries(&self.rows.borrow(), self.current.borrow().as_ref());
        if !force && *self.shown.borrow() == entries {
            return;
        }
        self.search.set_visible(filter_visible(entries.len()));
        let query = self.search.text().to_string();

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let visible: Vec<&Entry> = entries
            .iter()
            .filter(|entry| matches(entry, &query))
            .collect();
        let mut listed: Vec<Row> = Vec::new();
        let mut current_row: Option<gtk::ListBoxRow> = None;
        for entry in visible.iter().copied() {
            let (widget, sparkline) = self.build_row(entry);
            self.list.append(&widget);
            if entry.current {
                current_row = Some(widget.clone());
            }
            listed.push(Row {
                env: entry.env.clone(),
                widget,
                sparkline,
            });
        }
        // Claim the height the rows need, as a MINIMUM.
        //
        // The file list above this expands, so a box hands it every spare
        // pixel and hands the panel its minimum — and a ScrolledWindow's
        // minimum is a few pixels whatever it contains. Propagating the
        // natural height is not enough against an expanding sibling: it has
        // to be the minimum, or the panel photographs as two and a half
        // rows with the rest scrolled away. Capped at VISIBLE_ROWS, which
        // is where it starts scrolling instead of growing.
        //
        // Both numbers come from the rows themselves now, because a row
        // that is working on something is two lines tall — see
        // [`list_height`].
        let height = list_height(visible.iter().copied());
        // Order matters, and it matters in both directions: GTK asserts
        // min <= max on EVERY call, not just once both are set. Growing
        // with min first trips it (the new min is above the old max), and
        // shrinking with max first trips it the other way — every probe
        // run logged the Gtk-CRITICAL from the first render after the rows
        // arrived. Lifting the cap for the moment between the two makes
        // either order legal, without a branch on which way the list moved.
        self.scroller.set_max_content_height(-1);
        self.scroller.set_min_content_height(height);
        self.scroller.set_max_content_height(height);
        *self.listed.borrow_mut() = listed;
        *self.shown.borrow_mut() = entries;

        // Selecting is how the active row gets its styling, and it must not
        // read as the user choosing it.
        self.selecting.set(true);
        match &current_row {
            Some(row) => self.list.select_row(Some(row)),
            None => self.list.select_row(gtk::ListBoxRow::NONE),
        }
        self.selecting.set(false);
        // Deliberately no `grab_focus` here. The panel rebuilds whenever
        // the fleet moves, and a surface that took focus every time a
        // container changed state would steal the editor's caret.

        self.draw_activity();
    }

    fn build_row(self: &Rc<Self>, entry: &Entry) -> (gtk::ListBoxRow, Sparkline) {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(3);
        box_.set_margin_bottom(3);
        box_.set_margin_start(8);
        box_.set_margin_end(8);

        let dot = gtk::Box::builder()
            .css_classes(["env-dot", entry.light.css()])
            .valign(gtk::Align::Center)
            .build();
        box_.append(&dot);

        let label = gtk::Label::builder()
            .label(&entry.title)
            .xalign(0.0)
            .hexpand(true)
            // A long environment name must not widen the tree: the pane's
            // minimum decides whether GNOME will tile this window.
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(10)
            .build();
        // The row the panes are aimed at is carried by the list's selection
        // styling alone: a typeface that changes with the aim reads as the
        // text itself changing, not as a state.
        //
        // The name, and under it what this world is doing.
        //
        // A second LINE rather than a suffix after the name, and that was
        // decided by looking at both. A flank is about 180px wide at its
        // minimum: after the dot, the name, the marks and the sparkline
        // there are perhaps fifty pixels left on the same line, which
        // ellipsizes an issue title to three words and a box — the caption
        // was there and said nothing. Stacked, it gets the row's whole
        // width and reads. The cost is honest and paid for in
        // [`list_height`]: rows that are working are taller.
        let names = gtk::Box::new(gtk::Orientation::Vertical, 0);
        names.set_hexpand(true);
        names.set_valign(gtk::Align::Center);
        names.append(&label);
        if let Some(claim) = &entry.working_on {
            // The title alone — no id. The id is how you ADDRESS an issue
            // and this is not where you do that; the console header carries
            // "working on i-0007 — …" in full, and so does this row's
            // tooltip. Here the question is only "doing what?".
            names.append(
                &gtk::Label::builder()
                    .label(&claim.title)
                    .css_classes(["caption", "dim-label"])
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(10)
                    .build(),
            );
        }
        box_.append(&names);
        // Waiting on the user is the one fact the light cannot carry on its
        // own: amber also means rebuilding, and means baseline, which is a
        // steady state a whole fleet can sit in. A question nobody has
        // answered gets its own mark, or it drowns.
        if entry.awaits_user {
            box_.append(
                &gtk::Box::builder()
                    .css_classes(["env-attention"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Its chat is waiting for your answer")
                    .build(),
            );
        }

        // The lock rides the CURRENT row alone. Every non-primary
        // environment is read-only, but what the user needs told is what
        // the view they are in permits — the panel's tint carries the rest.
        if entry.current && !entry.primary {
            box_.append(
                &gtk::Image::builder()
                    .icon_name("system-lock-screen-symbolic")
                    .css_classes(["dim-label"])
                    .pixel_size(12)
                    .tooltip_text("Read-only: this is another environment's checkout")
                    .build(),
            );
        }
        if entry.unpublished {
            box_.append(
                &gtk::Box::builder()
                    .css_classes(["env-unpublished"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Work here that no other checkout has")
                    .build(),
            );
        }
        // Where it stands in the review arc. A glyph, because every circle
        // on this row is 8px and means "state" — a fourth one in a fourth
        // colour would read as a fourth traffic light. The row itself takes
        // an accent rail (`.review-flagged`), which is what makes a
        // finished environment findable in a fleet without moving it.
        if let Some(icon) = entry.review.icon() {
            box_.append(
                &gtk::Image::builder()
                    .icon_name(icon)
                    .css_classes(match entry.review {
                        ReviewMark::Flagged => vec!["env-review"],
                        _ => vec!["dim-label"],
                    })
                    .pixel_size(12)
                    .valign(gtk::Align::Center)
                    .tooltip_text(match entry.review {
                        ReviewMark::Flagged => "Done, and waiting for your review",
                        _ => "You have ruled on this one — safe to destroy",
                    })
                    .build(),
            );
        }

        let sparkline = Sparkline::new();
        box_.append(&sparkline.widget);

        let row = gtk::ListBoxRow::builder()
            .child(&box_)
            .activatable(true)
            .build();
        if let Some(class) = entry.review.css() {
            row.add_css_class(class);
        }
        row.set_tooltip_text(Some(&entry.tooltip()));
        (row, sparkline)
    }

    fn choose(self: &Rc<Self>, env: &EnvironmentId) {
        self.search.set_text("");
        if let Some(hook) = self.on_select.borrow().as_ref() {
            hook(env.clone());
        }
    }

    /// TASTE_PROBE_CHECK only: give one row a fabricated activity window,
    /// so a headless shot has sparklines in it.
    ///
    /// What is fabricated is the *samples*, not the drawing: the widget,
    /// the scale, the alpha and the theme colour are the real ones, exactly
    /// as `seed_watching_for_probe` fabricates a binding and lets the locks
    /// and refusals be genuine. The live sampler is left alone — a probe
    /// window has been up for two seconds and has no five minutes to have
    /// a history in.
    pub fn seed_activity_for_probe(self: &Rc<Self>, env: &EnvironmentId, shape: Shape) {
        self.probe_activity
            .borrow_mut()
            .insert(env.clone(), probe_samples(shape));
        self.draw_activity();
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
    use crate::fleet::{ChatBinding, EnvFacts, EnvGit, Spend};
    use taste_core::state::WorkspaceState;
    use taste_devcontainer::SupervisorState;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn running() -> SupervisorState {
        SupervisorState::Running {
            container_id: "abc123".into(),
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

    /// The rows the panel renders are the fleet's own, assembled the one
    /// way they are assembled anywhere.
    fn fleet(facts: Vec<EnvFacts>) -> Vec<FleetRow> {
        let mut state = WorkspaceState::default();
        state.set_environment_name(&env("spry-2"), Some("the refactor"));
        crate::fleet::assemble(facts, &state, &["agents/calm-1/topic".to_string()])
    }

    /// At home: the user's own checkout is "Yours", nothing is locked, and
    /// nothing is tinted — being home is the resting state, not a mode.
    #[test]
    fn the_panel_names_the_primary_yours_and_leaves_it_untinted() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("calm-1", SupervisorState::Building),
        ]);
        let face = face(&rows, None);
        assert_eq!(face.title, "Yours");
        assert_eq!(face.light, Light::Green);
        assert!(!face.locked && !face.away);
        assert!(face.detail.contains("Your own checkout\nrunning"));
    }

    /// Away: the name of where you are, the lock, and the tint — all three
    /// derived from the row, none of them from the tree's own state.
    #[test]
    fn watching_an_environment_names_it_locks_it_and_tints_the_panel() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("spry-2", SupervisorState::Building),
        ]);
        let face = face(&rows, Some(&env("spry-2")));
        assert_eq!(face.title, "the refactor", "the name the user gave it");
        assert_eq!(face.light, Light::Amber, "a building container is not up");
        assert!(face.locked, "every non-primary view is read-only");
        assert!(face.away);
        assert!(face.detail.contains("read-only"));
    }

    /// The fleet has not been assembled yet — or the environment being
    /// watched has just been destroyed. The panel still says where the
    /// panes are, and does not invent a state for it.
    #[test]
    fn a_panel_with_no_rows_yet_says_where_it_is_and_nothing_more() {
        assert_eq!(
            face(&[], None),
            Face {
                title: "Yours".into(),
                light: Light::Unknown,
                locked: false,
                away: false,
                detail: "Yours · state not known yet".into(),
            }
        );
        let orphan = face(&[], Some(&env("calm-1")));
        assert_eq!(orphan.title, "calm-1");
        assert!(orphan.locked && orphan.away, "not home is still not home");
        assert!(entries(&[], None).is_empty(), "and it lists nothing");
    }

    /// The list's order is the fleet's order — primary first as the return
    /// path, then by what the others are called — and each row carries the
    /// facts that decide whether to go there.
    #[test]
    fn the_panel_lists_the_fleet_in_its_order_with_the_primary_first() {
        let rows = fleet(vec![
            facts("spry-2", SupervisorState::Stopped),
            EnvFacts {
                chat: Some(ChatBinding {
                    label: "Claude 2".into(),
                    busy: true,
                    awaits_user: false,
                    orchestrator: false,
                }),
                git: Some(EnvGit {
                    branch: Some("topic/inbox".into()),
                    unpublished: 2,
                    dirty: 0,
                }),
                ..facts("calm-1", running())
            },
            facts("primary", running()),
        ]);
        let entries = entries(&rows, Some(&env("calm-1")));
        assert_eq!(
            entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            ["Yours", "calm-1", "the refactor"],
        );
        assert!(entries[0].primary && entries[0].light == Light::Green);
        assert!(!entries[0].current, "the panes are not at home");
        let calm = &entries[1];
        assert!(calm.current, "the row the panes are aimed at is marked");
        assert!(calm.busy, "its chat is mid-turn");
        assert!(calm.unpublished, "work only that checkout has");
        assert_eq!(calm.detail, "running");
        assert_eq!(
            entries[2].light,
            Light::Red,
            "a stopped environment can run nothing"
        );
        assert!(
            !entries[2].busy && !entries[2].unpublished,
            "a row carries its own facts and no one else's"
        );
    }

    /// With the panes at home, the primary is the marked row — the tree
    /// says `None`, and no row has to be told it is the primary twice.
    #[test]
    fn none_marks_the_primary_row() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("calm-1", running()),
        ]);
        let entries = entries(&rows, None);
        assert!(entries[0].current && !entries[1].current);
    }

    /// A row that is waiting on the user says so twice: in the light, so a
    /// glance catches it, and in the tooltip, so it is answerable.
    #[test]
    fn a_row_waiting_on_the_user_shows_amber_and_says_why() {
        let rows = fleet(vec![EnvFacts {
            chat: Some(ChatBinding {
                label: "Claude 2".into(),
                busy: true,
                awaits_user: true,
                orchestrator: false,
            }),
            ..facts("calm-1", running())
        }]);
        let entry = entries(&rows, None).remove(0);
        assert_eq!(entry.light, Light::Amber);
        assert!(entry.awaits_user);
        assert!(entry.tooltip().contains("waiting for an answer from you"));
        assert!(
            entry.tooltip().contains("read-only to you"),
            "and it is still someone else's checkout"
        );
    }

    /// Busy is not drawn, so it has to be said. A row whose chat is
    /// working says so where the reader can still find it.
    #[test]
    fn a_busy_row_says_so_in_its_tooltip_since_it_has_no_spinner() {
        let rows = fleet(vec![EnvFacts {
            chat: Some(ChatBinding {
                label: "Claude 2".into(),
                busy: true,
                awaits_user: false,
                orchestrator: false,
            }),
            ..facts("calm-1", running())
        }]);
        let entry = entries(&rows, None).remove(0);
        assert!(entry.busy && entry.tooltip().contains("working now"));
        // Waiting outranks working: a chat stopped on a question is not
        // making progress, and saying both would bury the one that needs
        // an answer.
        let waiting = Entry {
            awaits_user: true,
            ..entry
        };
        assert!(waiting.tooltip().contains("waiting for an answer"));
        assert!(!waiting.tooltip().contains("working now"));
    }

    /// The primary's tooltip names it as the user's own — the one row where
    /// "read-only" would be exactly wrong.
    #[test]
    fn the_primary_row_is_never_described_as_read_only() {
        let rows = fleet(vec![facts("primary", running())]);
        let tooltip = entries(&rows, None).remove(0).tooltip();
        assert!(tooltip.contains("your own checkout"));
        assert!(!tooltip.contains("read-only"));
    }

    /// An environment that says it is done is marked, and the mark is not
    /// the light: its container is stopped, so the light is honestly red,
    /// and red is not what "this wants your judgment" looks like. The row
    /// keeps its place in the list — a fleet that reordered itself when an
    /// agent finished would move under the reader's pointer.
    #[test]
    fn a_flagged_row_is_marked_where_it_stands_and_says_why_the_light_is_red() {
        let mut ready = facts("spry-2", SupervisorState::Stopped);
        ready.review = taste_core::ReviewState::FlaggedForReview;
        let rows = fleet(vec![facts("primary", running()), ready]);
        let entries = entries(&rows, None);

        assert_eq!(
            entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            ["Yours", "the refactor"],
            "flagging moves nothing"
        );
        let flagged = &entries[1];
        assert_eq!(flagged.review, crate::fleet::ReviewMark::Flagged);
        assert_eq!(
            flagged.light,
            Light::Red,
            "nothing runs in it, and that is true"
        );
        assert!(flagged.tooltip().contains("waiting for your review"));
        assert!(
            flagged
                .tooltip()
                .contains("stopped because nothing is left to run"),
            "the red light needs explaining, or it reads as a fault"
        );

        // Working is the ordinary case and wears no mark at all.
        assert_eq!(entries[0].review, crate::fleet::ReviewMark::None);
        assert!(!entries[0].tooltip().contains("review"));
    }

    /// Settled: the user has ruled, so the row is history rather than a
    /// question. It says so, and it says the thing that follows from it.
    #[test]
    fn a_settled_row_says_it_is_safe_to_destroy() {
        for settled in [
            taste_core::ReviewState::Merged,
            taste_core::ReviewState::Rejected,
        ] {
            let mut done = facts("calm-1", SupervisorState::Stopped);
            done.review = settled;
            let entry = entries(&fleet(vec![done]), None).remove(0);
            assert_eq!(
                entry.review,
                crate::fleet::ReviewMark::Settled,
                "{settled:?}"
            );
            assert!(entry.tooltip().contains("safe to destroy"));
            assert!(!entry.unpublished, "the user already looked");
        }
    }

    /// The panel's side of the env↔issue link: a row says what its
    /// environment is working on, in the caption and — with the id, which
    /// is how a person addresses an issue — in the tooltip. A row with no
    /// claim says nothing, rather than saying "idle".
    #[test]
    fn a_row_says_what_its_environment_is_working_on() {
        let claim = |id: &str, title: &str| taste_git::Claim {
            id: id.into(),
            title: title.into(),
        };
        let mut busy = facts("calm-1", running());
        busy.working_on = vec![
            claim("i-0007", "The composer loses a half-typed follow-up"),
            claim("i-0011", "And a second one"),
        ];
        let rows = fleet(vec![facts("primary", running()), busy]);
        let entries = entries(&rows, None);

        let calm = entries.iter().find(|e| e.env == env("calm-1")).unwrap();
        let working = calm.working_on.as_ref().unwrap();
        assert_eq!(
            working.id, "i-0007",
            "the first claim; the row has one line"
        );
        assert!(
            calm.tooltip()
                .contains("Working on i-0007 — The composer loses"),
            "{}",
            calm.tooltip()
        );

        let primary = &entries[0];
        assert_eq!(primary.working_on, None);
        assert!(
            !primary.tooltip().contains("Working on"),
            "an environment with no claim says nothing, not \"idle\""
        );
    }

    /// The ceiling is a promise about ROWS, not about pixels. A fleet that
    /// is working has taller rows, and six of them still have to fit before
    /// the panel starts scrolling — otherwise the work line would quietly
    /// cost two rows of visible fleet.
    #[test]
    fn the_panel_makes_room_for_the_rows_it_actually_has() {
        let plain = |slug: &str| facts(slug, running());
        let working = |slug: &str| {
            let mut facts = facts(slug, running());
            facts.working_on = vec![taste_git::Claim {
                id: "i-0001".into(),
                title: "something".into(),
            }];
            facts
        };
        let height = |facts| list_height(&entries(&fleet(facts), None));

        assert_eq!(height(vec![plain("primary")]), ROW_HEIGHT);
        assert_eq!(
            height(vec![plain("primary"), plain("calm-1")]),
            2 * ROW_HEIGHT
        );
        assert_eq!(
            height(vec![plain("primary"), working("calm-1")]),
            2 * ROW_HEIGHT + CAPTION_HEIGHT,
            "the working row is a line taller"
        );

        // Past the ceiling only the first VISIBLE_ROWS are counted — the
        // rest are what the scrolling is for.
        let many: Vec<EnvFacts> = std::iter::once(plain("primary"))
            .chain((1..=VISIBLE_ROWS + 2).map(|n| plain(&format!("calm-{n}"))))
            .collect();
        assert_eq!(height(many), VISIBLE_ROWS * ROW_HEIGHT);
        // An empty list still claims one row: a panel that collapsed to
        // nothing would read as a panel that had gone away.
        assert_eq!(list_height(&[]), ROW_HEIGHT);
    }

    /// Two environments are read, not searched. The filter appears when
    /// the list gets long enough that reading it stops working — which is
    /// also where the panel starts scrolling.
    #[test]
    fn the_filter_appears_only_when_the_list_outgrows_reading() {
        assert!(!filter_visible(1), "the solo primary needs no search box");
        assert!(!filter_visible(2));
        assert!(!filter_visible(FILTER_THRESHOLD));
        assert!(filter_visible(FILTER_THRESHOLD + 1));
        assert_eq!(
            FILTER_THRESHOLD as i32, VISIBLE_ROWS,
            "the filter and the scrolling should start at the same length"
        );
    }

    /// Filtering matches what the row shows and the slug behind it: a
    /// renamed environment is still findable by the name the user typed
    /// into a tool call an hour ago.
    #[test]
    fn the_filter_matches_the_name_on_screen_and_the_slug_behind_it() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("spry-2", running()),
        ]);
        let entries = entries(&rows, None);
        let refactor = &entries[1];
        assert!(matches(refactor, ""), "an empty filter hides nothing");
        assert!(matches(refactor, "REFACT"), "case is not a filter");
        assert!(matches(refactor, "spry"), "the slug still finds it");
        assert!(!matches(refactor, "calm"));
        assert!(matches(&entries[0], "yours"), "the primary filters too");
    }
}
