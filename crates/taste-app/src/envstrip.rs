//! The environment strip: where the window says which world you are in.
//!
//! ENVIRONMENTS.md → "Watching an environment". Aiming the panes at
//! another environment changes the meaning of every other pane — the tree,
//! the git views, what a save is allowed to do — so it is top-level
//! context, and top-level context gets a permanent place rather than a
//! control buried in a tab. The strip is pinned to the very bottom of the
//! file-tree pane, below the intervention panel, and is the only indicator
//! of where the panes are aimed (VS Code's remote-indicator corner is the
//! acknowledged precedent).
//!
//! It renders [`FleetRow`]s and derives nothing of its own: the console
//! assembles the fleet from the six places an environment's facts live,
//! and this is one more renderer of those same rows — a strip that
//! disagreed with the fleet view about what an environment is called or
//! whether it is running would be worse than no strip.
//!
//! Everything above [`EnvStrip`] is pure and tested: what the strip says,
//! what the popover lists, and when the list is long enough to earn a
//! filter.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_devcontainer::SupervisorState;

use crate::fleet::FleetRow;

/// What the user's own checkout is called, everywhere the UI has to name
/// it. The git interface already says "Keep Yours" of the user's side of a
/// conflict; the primary environment is the same "yours".
pub const PRIMARY_TITLE: &str = "Yours";

/// Past this many environments the popover grows a filter. Two or three
/// environments are read, not searched, and a search entry over them is
/// chrome that costs a row of height and gives nothing back.
pub const FILTER_THRESHOLD: usize = 6;

/// The state dot: what an environment is doing, in one glyph.
///
/// Four states, because that is how many outcomes change what the user
/// would do next — not one per [`SupervisorState`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// Container mode: the environment is up and work runs in it.
    Running,
    /// Getting there — building or starting.
    Working,
    /// Safe mode: no container, so no exec target.
    Safe,
    /// The container tried and failed.
    Failed,
    /// The fleet has not been assembled yet.
    Unknown,
}

impl Dot {
    /// The CSS class that colours it (see the `.env-dot` rules in main.rs).
    pub fn css(self) -> &'static str {
        match self {
            Dot::Running => "running",
            Dot::Working => "working",
            Dot::Safe => "safe",
            Dot::Failed => "failed",
            Dot::Unknown => "unknown",
        }
    }

    fn of(row: &FleetRow) -> Dot {
        match row.state {
            SupervisorState::Running { .. } => Dot::Running,
            SupervisorState::Building | SupervisorState::Starting => Dot::Working,
            SupervisorState::Failed { .. } => Dot::Failed,
            SupervisorState::NoConfig
            | SupervisorState::ConfigDetected
            | SupervisorState::Stopped => Dot::Safe,
        }
    }
}

/// What the strip itself shows: where you are, and whether that is home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    /// The environment's name, or [`PRIMARY_TITLE`] for the user's own.
    pub title: String,
    pub dot: Dot,
    /// The view is read-only — true of every non-primary environment.
    pub locked: bool,
    /// Tint the strip: you are not home, and peripheral vision should say
    /// so before anything is read.
    pub away: bool,
    /// A chat in some OTHER environment is waiting on the user. The strip
    /// is permanent and the popover is not, so this is where a question
    /// asked in a world nobody is looking at becomes visible.
    pub elsewhere_waiting: bool,
    /// The tooltip: the same fact, spelled out.
    pub detail: String,
}

/// One line of the popover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub env: EnvironmentId,
    pub title: String,
    pub dot: Dot,
    /// Its chat is mid-turn.
    pub busy: bool,
    /// Its chat is waiting on the user — a permission request nobody has
    /// answered. The panel is where a conversation the user cannot see
    /// asks for them, so this is the marker that has to carry weight.
    pub attention: bool,
    /// It holds work no other checkout has a copy of.
    pub unpublished: bool,
    /// The panes are aimed here.
    pub current: bool,
    /// The state, for the row's tooltip.
    pub detail: String,
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

/// What the strip says, given the fleet and where the panes are aimed.
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
            dot: Dot::Unknown,
            locked: current.is_some(),
            away: current.is_some(),
            elsewhere_waiting: waiting_elsewhere(rows, current),
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
        dot: Dot::of(row),
        locked: away,
        away,
        detail,
        elsewhere_waiting: waiting_elsewhere(rows, current),
    }
}

/// Is a chat the user cannot see waiting for them?
///
/// Deliberately "elsewhere": the selected environment's own chat is on
/// screen with its permission banner showing, and a second marker for it
/// would be the strip nagging about something already in front of them.
fn waiting_elsewhere(rows: &[FleetRow], current: Option<&EnvironmentId>) -> bool {
    rows.iter()
        .filter(|row| !is_current(row, current))
        .any(|row| row.chat.as_ref().is_some_and(|chat| chat.attention))
}

/// The popover's rows, in the order the fleet assembled them: the primary
/// first — it is the way back — then by what the others are called. That
/// order is [`crate::fleet::assemble`]'s, and is deliberately not
/// re-derived here.
pub fn entries(rows: &[FleetRow], current: Option<&EnvironmentId>) -> Vec<Entry> {
    rows.iter()
        .map(|row| Entry {
            title: title_of(row),
            dot: Dot::of(row),
            busy: row.chat.as_ref().is_some_and(|chat| chat.busy),
            attention: row.chat.as_ref().is_some_and(|chat| chat.attention),
            unpublished: row.has_unpublished_work(),
            current: is_current(row, current),
            detail: row.state_text(),
            env: row.env.clone(),
        })
        .collect()
}

/// Whether the popover shows its filter. Only past [`FILTER_THRESHOLD`].
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

/// How the strip asks the window to aim the panes somewhere.
pub type SelectHook = Box<dyn Fn(EnvironmentId)>;
/// How the popover's last row creates an environment. Takes the row's own
/// button, which goes insensitive while the clone runs.
pub type NewEnvironmentHook = Box<dyn Fn(gtk::Button)>;
/// Called just before the popover opens, so it lists what is true now.
pub type RefreshHook = Box<dyn Fn()>;

pub struct EnvStrip {
    /// The strip itself: a permanent child at the bottom of the file-tree
    /// pane, below the intervention panel.
    pub widget: gtk::Box,
    button: gtk::MenuButton,
    dot: gtk::Box,
    label: gtk::Label,
    lock: gtk::Image,
    /// Shown when a chat in another environment is waiting on the user.
    waiting: gtk::Box,
    popover: gtk::Popover,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    rows: RefCell<Vec<FleetRow>>,
    current: RefCell<Option<EnvironmentId>>,
    /// The environments the popover is listing, in the order it listed
    /// them: what an activated row at index *n* means. Rebuilt with the
    /// list, so a filtered list cannot activate the wrong environment.
    listed: RefCell<Vec<EnvironmentId>>,
    on_select: RefCell<Option<SelectHook>>,
    on_new_environment: RefCell<Option<NewEnvironmentHook>>,
    on_refresh: RefCell<Option<RefreshHook>>,
}

impl EnvStrip {
    pub fn new() -> Rc<Self> {
        let dot = gtk::Box::builder()
            .css_classes(["env-dot", "unknown"])
            .valign(gtk::Align::Center)
            .build();
        let label = gtk::Label::builder()
            .label(PRIMARY_TITLE)
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .hexpand(true)
            // A long environment name must not widen the tree: the pane's
            // minimum decides whether GNOME will tile this window.
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(10)
            .build();
        let lock = gtk::Image::builder()
            .icon_name("system-lock-screen-symbolic")
            .css_classes(["dim-label"])
            .pixel_size(12)
            .visible(false)
            .build();
        let waiting = gtk::Box::builder()
            .css_classes(["env-attention"])
            .valign(gtk::Align::Center)
            .visible(false)
            .tooltip_text("A chat in another environment is waiting for your answer")
            .build();
        let arrow = gtk::Image::builder()
            .icon_name("pan-up-symbolic")
            .css_classes(["dim-label"])
            .pixel_size(12)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&dot);
        content.append(&label);
        content.append(&lock);
        content.append(&waiting);
        content.append(&arrow);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter environments…")
            .margin_top(6)
            .margin_start(6)
            .margin_end(6)
            .visible(false)
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .css_classes(["navigation-sidebar"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            // Past this the list scrolls rather than growing a popover
            // taller than the pane it hangs off.
            .max_content_height(320)
            .build();
        let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popover_box.set_widget_name("envswitcher-list");
        popover_box.append(&search);
        popover_box.append(&scroller);
        let popover = gtk::Popover::builder()
            .child(&popover_box)
            .width_request(240)
            .position(gtk::PositionType::Top)
            .build();
        // The switcher is a surface of its own, and a probe target of its
        // own: `filetree.envswitcher` (ui_probe.rs).
        popover.set_widget_name("envswitcher");

        let button = gtk::MenuButton::builder()
            .child(&content)
            .popover(&popover)
            .css_classes(["flat", "env-strip-button"])
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("env-strip");
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&button);

        let strip = Rc::new(Self {
            widget,
            button,
            dot,
            label,
            lock,
            waiting: waiting.clone(),
            popover: popover.clone(),
            search: search.clone(),
            list: list.clone(),
            rows: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            listed: RefCell::new(Vec::new()),
            on_select: RefCell::new(None),
            on_new_environment: RefCell::new(None),
            on_refresh: RefCell::new(None),
        });

        // The list is built at open time, from the rows as they are then:
        // a popover is a moment, not a surface that has to stay live, and
        // building it here is what keeps the strip free of per-event
        // widget churn.
        let weak = Rc::downgrade(&strip);
        popover.connect_show(move |_| {
            let Some(strip) = weak.upgrade() else { return };
            if let Some(refresh) = strip.on_refresh.borrow().as_ref() {
                refresh();
            }
            strip.rebuild_list();
        });
        let weak = Rc::downgrade(&strip);
        popover.connect_closed(move |_| {
            if let Some(strip) = weak.upgrade() {
                strip.search.set_text("");
            }
        });
        // One activation path for the pointer and the keyboard alike: a
        // ListBox activates its row on a click and on Enter, and the row's
        // index is its place in the list the strip just built.
        let weak = Rc::downgrade(&strip);
        list.connect_row_activated(move |_, row| {
            let Some(strip) = weak.upgrade() else { return };
            let index = row.index();
            if index < 0 {
                return;
            }
            let env = strip.listed.borrow().get(index as usize).cloned();
            if let Some(env) = env {
                strip.choose(&env);
            }
        });
        let weak = Rc::downgrade(&strip);
        search.connect_search_changed(move |_| {
            if let Some(strip) = weak.upgrade() {
                strip.rebuild_list();
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
        let weak = Rc::downgrade(&strip);
        search.connect_activate(move |_| {
            let Some(strip) = weak.upgrade() else { return };
            let first = strip.listed.borrow().first().cloned();
            if let Some(env) = first {
                strip.choose(&env);
            }
        });

        strip
    }

    /// Aim the strip: which environment the panes are on (`None` = the
    /// user's own). Called by the file tree, which is where that fact
    /// changes — never by an event.
    pub fn set_current(self: &Rc<Self>, current: Option<EnvironmentId>) {
        if *self.current.borrow() == current {
            return;
        }
        *self.current.borrow_mut() = current;
        self.apply_face();
    }

    /// The assembled fleet. Cheap and coarse: it lands on fleet changes,
    /// does nothing when the rows it is handed are the ones it has, and
    /// touches exactly three widgets when they are not — the popover's
    /// rows are built when the popover opens, not when the fleet moves.
    pub fn set_rows(self: &Rc<Self>, rows: &[FleetRow]) {
        if self.rows.borrow().as_slice() == rows {
            return;
        }
        *self.rows.borrow_mut() = rows.to_vec();
        self.apply_face();
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

    /// Open the switcher from the keyboard (Ctrl+Shift+E).
    pub fn open_switcher(&self) {
        self.button.popup();
    }

    fn face(&self) -> Face {
        face(&self.rows.borrow(), self.current.borrow().as_ref())
    }

    fn apply_face(&self) {
        let face = self.face();
        self.label.set_label(&face.title);
        self.button.set_tooltip_text(Some(&face.detail));
        self.lock.set_visible(face.locked);
        self.waiting.set_visible(face.elsewhere_waiting);
        for dot in [
            Dot::Running,
            Dot::Working,
            Dot::Safe,
            Dot::Failed,
            Dot::Unknown,
        ] {
            self.dot.remove_css_class(dot.css());
        }
        self.dot.add_css_class(face.dot.css());
        if face.away {
            self.widget.add_css_class("away");
        } else {
            self.widget.remove_css_class("away");
        }
    }

    fn rebuild_list(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let entries = entries(&self.rows.borrow(), self.current.borrow().as_ref());
        self.search.set_visible(filter_visible(entries.len()));
        let query = self.search.text().to_string();
        let mut current_row: Option<gtk::ListBoxRow> = None;
        let mut listed: Vec<EnvironmentId> = Vec::new();
        for entry in entries.iter().filter(|entry| matches(entry, &query)) {
            let row = self.build_row(entry);
            self.list.append(&row);
            listed.push(entry.env.clone());
            if entry.current {
                current_row = Some(row);
            }
        }
        *self.listed.borrow_mut() = listed;
        // The way to make a new one lives where the switching does: with
        // one environment there is no fleet view open to find the console's
        // button in, and this is the surface that says environments exist.
        self.list.append(&self.build_new_environment_row());
        if let Some(row) = current_row {
            self.list.select_row(Some(&row));
            row.grab_focus();
        }
        if self.search.is_visible() {
            self.search.grab_focus();
        }
    }

    fn build_row(self: &Rc<Self>, entry: &Entry) -> gtk::ListBoxRow {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);
        box_.set_margin_start(8);
        box_.set_margin_end(8);
        let dot = gtk::Box::builder()
            .css_classes(["env-dot", entry.dot.css()])
            .valign(gtk::Align::Center)
            .build();
        box_.append(&dot);
        let label = gtk::Label::builder()
            .label(&entry.title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(14)
            .build();
        box_.append(&label);
        // Waiting on the user outranks working, and is the reason this
        // list is worth glancing at: a chat in an environment nobody has
        // selected has no other way to ask.
        if entry.attention {
            let dot = gtk::Box::builder()
                .css_classes(["env-attention"])
                .valign(gtk::Align::Center)
                .tooltip_text("Its chat is waiting for your answer")
                .build();
            box_.append(&dot);
        } else if entry.busy {
            let spinner = gtk::Spinner::builder()
                .spinning(true)
                .valign(gtk::Align::Center)
                .tooltip_text("Its chat is mid-turn")
                .build();
            box_.append(&spinner);
        }
        if entry.unpublished {
            let pip = gtk::Box::builder()
                .css_classes(["env-unpublished"])
                .valign(gtk::Align::Center)
                .tooltip_text("Work here that no other checkout has")
                .build();
            box_.append(&pip);
        }
        let check = gtk::Image::builder()
            .icon_name("object-select-symbolic")
            .pixel_size(14)
            .visible(entry.current)
            .build();
        box_.append(&check);

        let row = gtk::ListBoxRow::builder()
            .child(&box_)
            .activatable(true)
            .build();
        row.set_tooltip_text(Some(&format!("{} · {}", entry.title, entry.detail)));
        row
    }

    /// The last row: make one. Mirrored from the fleet view's own button —
    /// same call, so there is still one way an environment is created.
    fn build_new_environment_row(self: &Rc<Self>) -> gtk::ListBoxRow {
        let button = gtk::Button::builder()
            .child(&{
                let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                box_.append(&gtk::Image::from_icon_name("list-add-symbolic"));
                box_.append(
                    &gtk::Label::builder()
                        .label("New environment")
                        .xalign(0.0)
                        .hexpand(true)
                        .build(),
                );
                box_
            })
            .css_classes(["flat", "env-new"])
            .tooltip_text(
                "Clone the workspace into a new environment. It gets its own \
                 checkout and devcontainer, and no chat until you give it one.",
            )
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |button| {
            let Some(strip) = weak.upgrade() else { return };
            if let Some(hook) = strip.on_new_environment.borrow().as_ref() {
                hook(button.clone());
            }
            strip.popover.popdown();
        });
        // Set off from the environments above it: the list is places to
        // go, this is a thing to do.
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_top(2);
        box_.append(&separator);
        box_.append(&button);
        gtk::ListBoxRow::builder()
            .child(&box_)
            .selectable(false)
            .activatable(false)
            .build()
    }

    fn choose(self: &Rc<Self>, env: &EnvironmentId) {
        self.popover.popdown();
        if let Some(hook) = self.on_select.borrow().as_ref() {
            hook(env.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{ChatBinding, EnvFacts, EnvGit, Spend};
    use taste_core::state::WorkspaceState;

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
            pending_rebuild: false,
            chat: None,
            git: None,
            disk: None,
            spend: Spend::default(),
            shells: 0,
        }
    }

    /// The rows the strip renders are the fleet's own, assembled the one
    /// way they are assembled anywhere.
    fn fleet(facts: Vec<EnvFacts>) -> Vec<FleetRow> {
        let mut state = WorkspaceState::default();
        state.set_environment_name(&env("spry-2"), Some("the refactor"));
        crate::fleet::assemble(facts, &state, &["agents/calm-1/topic".to_string()])
    }

    /// At home: the user's own checkout is "Yours", nothing is locked, and
    /// nothing is tinted — being home is the resting state, not a mode.
    #[test]
    fn the_strip_names_the_primary_yours_and_leaves_it_untinted() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("calm-1", SupervisorState::Building),
        ]);
        let face = face(&rows, None);
        assert_eq!(face.title, "Yours");
        assert_eq!(face.dot, Dot::Running);
        assert!(!face.locked && !face.away);
        assert!(face.detail.contains("container mode · running"));
    }

    /// Away: the name of where you are, a lock, and the tint — all three
    /// derived from the row, none of them from the tree's own state.
    #[test]
    fn watching_an_environment_names_it_locks_it_and_tints_the_strip() {
        let rows = fleet(vec![
            facts("primary", running()),
            facts("spry-2", SupervisorState::Building),
        ]);
        let face = face(&rows, Some(&env("spry-2")));
        assert_eq!(face.title, "the refactor", "the name the user gave it");
        assert_eq!(face.dot, Dot::Working, "building is not container mode");
        assert!(face.locked, "every non-primary view is read-only");
        assert!(face.away);
        assert!(face.detail.contains("read-only"));
    }

    /// Each state the dot has to tell apart, and the two that are easy to
    /// get wrong: building is not running, and a stopped container is
    /// safe mode rather than a failure.
    #[test]
    fn the_dot_says_what_the_environment_is_doing() {
        let dot = |state| {
            let rows = fleet(vec![facts("calm-1", state)]);
            face(&rows, Some(&env("calm-1"))).dot
        };
        assert_eq!(dot(running()), Dot::Running);
        assert_eq!(dot(SupervisorState::Building), Dot::Working);
        assert_eq!(dot(SupervisorState::Starting), Dot::Working);
        assert_eq!(dot(SupervisorState::Stopped), Dot::Safe);
        assert_eq!(dot(SupervisorState::NoConfig), Dot::Safe);
        assert_eq!(
            dot(SupervisorState::Failed {
                message: "boom".into()
            }),
            Dot::Failed
        );
    }

    /// The fleet has not been assembled yet — or the environment being
    /// watched has just been destroyed. The strip still says where the
    /// panes are, and does not invent a state for it.
    #[test]
    fn a_strip_with_no_rows_yet_says_where_it_is_and_nothing_more() {
        assert_eq!(
            face(&[], None),
            Face {
                title: "Yours".into(),
                dot: Dot::Unknown,
                locked: false,
                away: false,
                detail: "Yours · state not known yet".into(),
                elsewhere_waiting: false,
            }
        );
        let orphan = face(&[], Some(&env("calm-1")));
        assert!(!orphan.elsewhere_waiting, "no rows, nothing waiting");
        assert_eq!(orphan.title, "calm-1");
        assert!(orphan.locked && orphan.away, "not home is still not home");
    }

    /// The popover's order is the fleet's order — primary first as the
    /// return path, then by what the others are called — and each row
    /// carries the three things that decide whether to go there.
    #[test]
    fn the_popover_lists_the_fleet_in_its_order_with_the_primary_first() {
        let rows = fleet(vec![
            facts("spry-2", SupervisorState::Stopped),
            EnvFacts {
                chat: Some(ChatBinding {
                    label: "Claude 2".into(),
                    busy: true,
                    attention: false,
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
        assert!(entries[0].dot == Dot::Running && !entries[0].current);
        let calm = &entries[1];
        assert!(calm.current, "the row the panes are aimed at is marked");
        assert!(calm.busy, "its chat is mid-turn");
        assert!(calm.unpublished, "work only that checkout has");
        assert_eq!(calm.detail, "container mode · running");
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

    /// Two environments are read, not searched. The filter appears when
    /// the list gets long enough that reading it stops working.
    #[test]
    fn the_filter_appears_only_when_the_list_outgrows_reading() {
        assert!(!filter_visible(1), "the solo primary needs no search box");
        assert!(!filter_visible(2));
        assert!(!filter_visible(FILTER_THRESHOLD));
        assert!(filter_visible(FILTER_THRESHOLD + 1));
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
