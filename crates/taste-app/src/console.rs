//! Bottom pane: a header naming the selected environment, over one flat
//! strip of tabs.
//!
//! The header **is** the environment (ENVIRONMENTS.md → "Supervision: fleet
//! view"): its mode and container state live, the chat bound to it, its
//! branch, what it has published, what it costs on disk and what it has
//! spent, what it is working on, the review band when it is waiting on a
//! judgment — and the per-environment actions. It is the *selected*
//! environment's; the enumeration of all of them is the file tree's panel,
//! and there is no second list here.
//!
//! Under it, `[log] [shells] [resources] [services] [terminal…]`. **No
//! nested tab sets**: the first three used to be an `AdwViewStack` behind an
//! inline switcher inside a single tab, which put a row of tab-shaped
//! controls under a row of tabs. Every leaf view is a first-class tab in
//! this pane's one strip — and below `CONSOLIDATED_MAX_WIDTH_SP` these
//! pages are *transferred* into the editor's strip, because down there the
//! window has one strip and this pane is not one of its regions any more.
//! Everything that adds or raises a page asks `host()`, never `tabs`.
//!
//! Terminal tabs spawn in an execution context resolved at spawn time
//! through `ExecContext` — which is what makes container reloads invisible
//! to existing tabs and automatic for new ones — and register themselves in
//! the shell roster, so the user's own shells are as visible in the fleet
//! as the agent's are.

use adw::prelude::*;
use gtk::glib;

/// GNOME Console's ANSI palette — legible on both backgrounds.
const ANSI_PALETTE: [&str; 16] = [
    "#241f31", "#c01c28", "#2ec27e", "#f5c211", "#1e78e4", "#9841bb", "#0ab9dc", "#c0bfbc",
    "#5e5c64", "#ed333b", "#57e389", "#f8e45c", "#51a1ff", "#c061cb", "#4fd2fd", "#f6f5f4",
];

/// Match the terminal to the IDE's (= desktop's) light/dark mode.
fn apply_terminal_theme(terminal: &vte4::Terminal) {
    let dark = adw::StyleManager::default().is_dark();
    let (fg, bg) = if dark {
        ("#d0cfcc", "#1d1b20")
    } else {
        ("#171421", "#ffffff")
    };
    let fg = gtk::gdk::RGBA::parse(fg).expect("valid color");
    let bg = gtk::gdk::RGBA::parse(bg).expect("valid color");
    let palette: Vec<gtk::gdk::RGBA> = ANSI_PALETTE
        .iter()
        .map(|c| gtk::gdk::RGBA::parse(*c).expect("valid color"))
        .collect();
    let palette_refs: Vec<&gtk::gdk::RGBA> = palette.iter().collect();
    terminal.set_colors(Some(&fg), Some(&bg), &palette_refs);
}

/// Write captured output into a VTE that has no pty behind it.
///
/// The translation is not cosmetic. Pipe output carries bare `\n`, and a
/// terminal reads that as "down one line, same column" — so a build log fed
/// verbatim staircases off the right edge. A pty's line discipline would
/// have added the `\r`; there is no pty here, so this does.
fn feed(terminal: &vte4::Terminal, bytes: &[u8]) {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    let mut previous = 0u8;
    for &byte in bytes {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        previous = byte;
    }
    terminal.feed(&out);
}

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use taste_core::environment::EnvironmentId;
use taste_core::quota::QuotaSnapshot;
use taste_core::{ShellId, ShellKind, ShellSink, Workspace};
use taste_devcontainer::{
    EnvironmentRegistry, ResourceInfo, ResourceKind, Supervisor, SupervisorState,
};
use vte4::prelude::*;

use crate::fleet::{self, ChatBinding, EnvFacts, EnvGit, FleetRow, PoolFacts};

/// How the window answers "which chat works in this environment".
pub type ChatLookup = Box<dyn Fn(&EnvironmentId) -> Option<ChatBinding>>;
/// How the fleet asks the window to aim the panes at an environment.
pub type OpenEnvironmentHook = Box<dyn Fn(EnvironmentId)>;

/// How the review band aims the git views at an environment's branch.
///
/// A hook rather than a call, because the views it aims are the file
/// tree's: the changed-file list, the diff face of the editor, and the
/// bulk-op pane under them. The console knows which branch; the tree knows
/// how to show one.
/// Takes the branch of record and the branch it is read against.
pub type OpenReviewHook = Box<dyn Fn(String, String)>;
/// How the assembled fleet reaches its other renderers: the rows, the
/// `agents/*` branch names behind their published counts, and the number
/// of open issues — which is not derivable from the rows, because an
/// unclaimed issue belongs to no environment.
pub type FleetChangedHook = Box<dyn Fn(&[FleetRow], usize)>;

/// What the review band knows about one environment's branch: the single
/// mergedness fact ([`taste_git::Mergedness`]) plus the target it was asked
/// against.
///
/// Computed off the main thread with the rest of the git pass and held,
/// like every other git fact here — a render must not walk a repository.
/// `None` for an environment that has never published: absent is not
/// "not merged", and a band that said "0 commits ahead" of a branch that
/// does not exist would be the one lie this whole lifecycle exists to
/// avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFacts {
    pub branch: String,
    pub target: String,
    pub mergedness: Option<taste_git::Mergedness>,
}

impl ReviewFacts {
    /// The band's headline: what this environment is asking of the user,
    /// in the words the state means.
    pub fn headline(name: &str, state: taste_core::ReviewState) -> String {
        match state {
            taste_core::ReviewState::Working => String::new(),
            taste_core::ReviewState::FlaggedForReview => {
                format!("{name} says it is done")
            }
            taste_core::ReviewState::Merged => format!("{name} was merged"),
            taste_core::ReviewState::Rejected => format!("{name} was rejected"),
        }
    }

    /// The band's fact line: the branch, the target, and how far apart
    /// they are — asked fresh, never latched. A force-moved target
    /// un-merges the work and this says so.
    pub fn detail(&self) -> String {
        let Some(merged) = &self.mergedness else {
            return format!(
                "{} has never been published, so there is nothing to review against {}.",
                self.branch, self.target
            );
        };
        let mut text = if merged.merged {
            format!(
                "{} → {} · already in {}",
                self.branch, self.target, self.target
            )
        } else {
            format!(
                "{} → {} · {} commit{} ahead",
                self.branch,
                self.target,
                merged.ahead,
                if merged.ahead == 1 { "" } else { "s" }
            )
        };
        if let Some(note) = &merged.note {
            text.push_str(&format!(" · {note}"));
        }
        text
    }

    /// Whether Merge is a thing to offer. Work already in the target has
    /// nothing to merge, and a button that would do nothing is worse than
    /// no button.
    pub fn mergeable(&self) -> bool {
        self.mergedness
            .as_ref()
            .is_some_and(|merged| !merged.merged)
    }
}

/// The workspace's issue queue, handed to whoever draws it — the backlog
/// panel in the file-tree flank.
///
/// A hook rather than a widget the console owns, for the same reason the
/// fleet is one: the console is where the off-thread git passes live, and
/// the queue is a *workspace* fact that has no business being a tab inside
/// the pane that is about the environment you are in.
pub type IssuesChangedHook = Box<dyn Fn(&[taste_git::Issue])>;

/// Who renders the subscription pool. Separate from the fleet hook on
/// purpose: the fleet is per-environment and this is the one pool all of
/// it draws on, so they change for different reasons and at different
/// times.
pub type PoolChangedHook = Box<dyn Fn(&PoolFacts)>;

/// The environment's sections, in the order they sit in the strip.
///
/// Remembered by NAME, not by position: which section the user was reading
/// has to survive the tabs being moved into the editor's strip and back at
/// the consolidated rung, and an index into a strip that also holds files
/// and terminals means nothing on the other side of that trip.
pub(crate) const SECTIONS: [&str; 2] = ["environment", "resources"];

/// Which section a remembered name refers to. Anything unknown is the
/// environment itself, which is what this pane is about when nothing else
/// is asked for.
pub(crate) fn section_index(name: &str) -> usize {
    SECTIONS
        .iter()
        .position(|section| *section == name)
        .unwrap_or(0)
}

pub struct Console {
    pub widget: gtk::Box,
    /// The console's OWN tab view — where its pages live at full width, and
    /// what they are transferred back into when the window grows.
    tabs: adw::TabView,
    /// The view the console's pages are actually in right now. Its own at
    /// full width; the editor's, below `CONSOLIDATED_MAX_WIDTH_SP`, where
    /// the window has one strip and this pane's tabs are grafted onto its
    /// end. Everything that adds, selects or stows a page asks this, never
    /// `tabs`, or a terminal opened while consolidated would be born in a
    /// view nobody can see.
    host: RefCell<adw::TabView>,
    /// The selection handler on whichever view is the host, so it can be
    /// moved with the pages instead of firing for a strip we left.
    host_watch: RefCell<Option<(adw::TabView, glib::SignalHandlerId)>>,
    /// Set while this pane's pages are moving between strips. A page
    /// leaving a view moves that view's selection to its neighbour, so a
    /// migration walks the selection down the strip one page at a time —
    /// and without this the section "last looked at" would end up being
    /// whichever page happened to be transferred last.
    migrating: Cell<bool>,
    /// The selected environment's build/lifecycle log. One buffer per
    /// environment (see `log_buffer`), swapped in on selection — a single
    /// buffer would paint one environment's build over another's.
    supervisor_log: gtk::TextView,
    /// The environment's sections, first-class tabs in the one strip.
    ///
    /// The "environment" tab is what used to be called "Log": the state
    /// line, the review banner, what the environment is working on, the
    /// actions, and the build log itself, all in one page's content now
    /// that the pane header above the strip is gone.
    env_page: adw::TabPage,
    resources_page: adw::TabPage,
    services_page: adw::TabPage,
    /// Which section was last looked at, so landing on an environment from
    /// a notification (or coming back across a breakpoint) returns to it
    /// rather than resetting the pane.
    last_section: RefCell<String>,
    follow_log: gtk::Switch,
    /// Shell tabs running on the machine/IDE-container — retired when the
    /// devcontainer attaches (work belongs inside it).
    host_shells: RefCell<Vec<adw::TabPage>>,
    /// The environment tab's own facts row — what the environment is
    /// doing, what it costs, whether its chat is busy, and what can be
    /// done to it.
    ///
    /// It does NOT name the environment. The file tree's environment panel
    /// is the app's single namer of the selected environment
    /// (ARCHITECTURE.md → "The environment panel is the single top-level
    /// control"), and it is on screen at every rung this pane exists at —
    /// so a name here would be a second rendering of the same row, and the
    /// stale one is always whichever the user is not looking at. For the
    /// same reason the branch and the dirty count are absent: those are
    /// working-tree facts, and the file tree is where working-tree facts
    /// live.
    env_state: gtk::Label,
    env_chat: gtk::Box,
    env_disk: gtk::Label,
    env_spend: gtk::Label,
    env_actions: gtk::MenuButton,
    /// What the fleet last rendered. The unchanged-guard for row churn —
    /// state events arrive constantly and rebuilding rows under an open
    /// menu is how a popover loses its anchor.
    rows: RefCell<Vec<FleetRow>>,
    /// The environment the panel below the list is showing.
    selected: RefCell<EnvironmentId>,
    /// Per-environment facts too expensive to compute on a render: git
    /// walks and directory walks. Filled by explicit refreshes, cached
    /// until the next one.
    git_facts: RefCell<HashMap<EnvironmentId, EnvGit>>,
    /// What each environment has claimed off the issue queue — the "working
    /// on" half of the env↔issue link. Read from the issues ref in the same
    /// off-thread pass as the git facts, because it is the same ref walk.
    claim_facts: RefCell<HashMap<EnvironmentId, Vec<taste_git::Claim>>>,
    disk_facts: RefCell<HashMap<EnvironmentId, taste_devcontainer::DiskUsage>>,
    /// `agents/*` branches in the USER's checkout — where publishing lands.
    published: RefCell<Vec<String>>,
    /// The persisted workspace state, for the one thing the registry
    /// cannot say: what the user calls an environment. Held rather than
    /// re-read, because a render must not touch the filesystem.
    state: RefCell<taste_core::state::WorkspaceState>,
    /// Which chat is bound where, asked of the chat strip at render time.
    chat_lookup: RefCell<Option<ChatLookup>>,
    on_open_environment: RefCell<Option<OpenEnvironmentHook>>,
    on_open_review: RefCell<Option<OpenReviewHook>>,
    /// Leaving the review, when a judgment has settled the environment.
    on_close_review: RefCell<Option<Box<dyn Fn()>>>,
    /// Who else renders this fleet: gadget mode and the varlink service.
    /// The console assembles once and tells them; neither goes back to the
    /// six sources for a second opinion.
    on_fleet_changed: RefCell<Option<FleetChangedHook>>,
    on_issues_changed: RefCell<Option<IssuesChangedHook>>,
    /// The subscription pool the whole fleet spends out of, as the proxy
    /// last saw it described, with the breakdown of who drew on it. The
    /// console is the one place that reads the proxy — everything else
    /// downstream is handed this.
    pool: RefCell<PoolFacts>,
    on_pool_changed: RefCell<Option<PoolChangedHook>>,
    /// Per-environment log buffers, and the lifecycle roster entry that
    /// mirrors each one. The stream is a roster row like any other shell —
    /// it is what an environment is "running" when it is building itself.
    logs: RefCell<HashMap<EnvironmentId, gtk::TextBuffer>>,
    lifecycle: RefCell<HashMap<EnvironmentId, ShellSink>>,
    /// The selected environment's podman resources.
    resources_list: gtk::ListBox,
    /// The tab showing each shell, and the environment it belongs to.
    ///
    /// The environment is recorded rather than looked up, because a shell
    /// that has EXITED is kept — `taste_core::ShellRoster` still exists
    /// for fleet counts and varlink, but there is no console-side list
    /// rendering it any more, so the tab itself, marked exited, is the
    /// only record that it ran — and a tab whose environment could not be
    /// answered would be a tab that belongs to whichever one is selected.
    shell_tabs: RefCell<HashMap<ShellId, (EnvironmentId, adw::TabPage)>>,
    /// Shell tabs of the environments that are not on screen. Unparented
    /// `AdwTabView`s, exactly as the editor stows its pages: a shell tab
    /// holds a live VTE — the user's own terminal among them — so it is
    /// moved out of sight, never closed.
    stowed_shells: RefCell<HashMap<EnvironmentId, adw::TabView>>,
    /// The workspace's issue queue, read off `refs/taste/issues` in the
    /// main checkout, in the order the `order` file puts it in.
    ///
    /// **Read here, rendered elsewhere.** The console owns the read because
    /// this is where the off-thread git passes live; the backlog panel in
    /// the file-tree flank is what draws it (`backlog.rs`), and gets it
    /// through [`Console::set_on_issues_changed`]. One read of the ref per
    /// change, rather than one per surface that shows it.
    issues: RefCell<Vec<taste_git::Issue>>,
    /// Where each environment's branch stands against the merge target.
    /// Only environments that have left `Working` are in here — asking a
    /// merge-base question about every environment on every git pass would
    /// be a walk per row for a band nobody is looking at.
    review_facts: RefCell<HashMap<EnvironmentId, ReviewFacts>>,
    /// A persistent condition wants a persistent widget: `AdwBanner`,
    /// leading the environment tab's content while the environment is
    /// flagged. Its own button is "Open Review"; Merge/Reject/Destroy —
    /// more than one action, which a banner's single button cannot hold —
    /// sit in `review_actions` just beneath it.
    review_bar: adw::Banner,
    review_detail: gtk::Label,
    review_actions: gtk::Box,
    /// The row `review_detail` and `review_actions` share, hidden as one so
    /// its margins go with it.
    review_extra: gtk::Box,
    env_working_on: gtk::Label,
    /// Created lazily on the first Flatpak log line, so projects without a
    /// manifest never see the tab.
    flatpak_log: RefCell<Option<gtk::TextView>>,
    /// The pinned Services tab: systemd units + journal in the container.
    services: Rc<crate::services::ServicesPane>,
    /// The fleet's intervention panel: rename, and the destroy confirmation
    /// that lists what would be lost. Never a modal — the same convention
    /// the file tree's dirty-file flows follow.
    intervention: gtk::Box,
    /// Probe-only fabricated issues: set, the queue stops re-reading the
    /// real (empty) ref out from under the screenshot.
    probe_issues: Cell<bool>,
    /// Probe-only fabricated environments (TASTE_PROBE_CHECK).
    probe_rows: RefCell<Vec<EnvFacts>>,
    /// A fabricated limit snapshot for the probe, standing in for the
    /// account the screenshots cannot have.
    probe_quota: RefCell<Option<QuotaSnapshot>>,
    workspace: Workspace,
    environments: Arc<EnvironmentRegistry>,
}

impl Console {
    pub fn new(workspace: Workspace, environments: Arc<EnvironmentRegistry>) -> Rc<Self> {
        let tabs = adw::TabView::new();
        // Natural-width tabs (same rule as the editor): a new terminal
        // must not resize every existing tab.
        let tab_bar = adw::TabBar::builder()
            .view(&tabs)
            .autohide(false)
            .expand_tabs(false)
            .build();

        let new_tab_button = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New terminal (in the selected environment, when it has a container)")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        // The same way out of a crowded strip the editor's has: an
        // environment with two sections, Services and a couple of
        // terminals already scrolls this bar in a 700px pane.
        let overview_button = adw::TabButton::builder()
            .view(&tabs)
            .action_name("overview.open")
            .tooltip_text("All tabs")
            .build();
        tab_bar.set_start_action_widget(Some(&overview_button));

        // New Terminal, refresh and the environment's `⋮` menu live in the
        // ENVIRONMENT TAB'S OWN CONTENT, at the top of it.
        //
        // Not in a pane header — there is none any more (the environment
        // panel is the app's single namer of the selected environment, and
        // a header under it repeating the name was the thing this change
        // deleted). And not in `AdwTabBar::set_end_action_widget` either,
        // which looks like the obvious home and is a trap: at the
        // consolidated rung this pane's PAGES are transferred into the
        // editor's strip (`Editor::graft_pages`) while this tab bar stays
        // behind with the pane, so an end-action widget is a control that
        // quietly leaves the window at 960px. A page's content crosses with
        // the page, so putting them there is what makes them reachable at
        // both rungs — and they are environment actions, which is what this
        // tab is.

        // --- the environment tab's own header -------------------------------
        let refresh_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(
                "Re-read every environment: branches, published work, podman \
                 resources, and disk footprint",
            )
            .css_classes(["flat"])
            .build();
        // On by default: a running build should read like a running build.
        let follow_log = gtk::Switch::builder()
            .tooltip_text(
                "Keep the log scrolled to the newest line as output arrives; \
                 turn off to read scrollback while the build keeps streaming",
            )
            .active(true)
            .valign(gtk::Align::Center)
            .build();
        let tail_label = gtk::Label::builder()
            .label("Tail")
            .css_classes(["caption-heading"])
            .build();
        // Tail sits in a toolbar directly above the log it controls, inside
        // the same tab. It used to be in the pane header, shown and hidden
        // as the Log section came and went — a visibility rule that existed
        // only because the switch and its view were in different widgets.
        // They are not any more, so there is nothing to keep in step.
        let log_toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        log_toolbar.set_halign(gtk::Align::End);
        log_toolbar.set_margin_start(12);
        log_toolbar.set_margin_end(12);
        log_toolbar.set_margin_top(4);
        log_toolbar.append(&tail_label);
        log_toolbar.append(&follow_log);
        // This tab is ONE environment — the one the panes are aimed at —
        // and it does not name it. The enumeration AND the naming of
        // environments live in the file tree's panel and nowhere else: two
        // renderings of the same `FleetRow` are two things to keep in
        // agreement, and the one that goes stale is whichever the user is
        // not looking at. The panel is on screen whether this tab is
        // selected or not, so it wins.
        let env_state = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let env_actions = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("Actions for this environment")
            .build();
        // Which chat works here, rebuilt per render because a spinner
        // either exists or does not. This is where the busy spinner lives
        // now: the file tree's panel row has no width for one, this does.
        let env_chat = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        env_chat.set_valign(gtk::Align::Center);
        let env_disk = gtk::Label::builder()
            .css_classes(["caption", "numeric", "dim-label"])
            .tooltip_text("Disk: this environment's clone plus its volumes")
            .build();
        let env_spend = gtk::Label::builder()
            .css_classes(["caption", "numeric", "dim-label"])
            .tooltip_text("Tokens spent through the IDE's auth proxy")
            .build();

        // What the environment is working ON, as opposed to what it is
        // doing. Two different questions, and this tab is the one place
        // both are answerable at once: the state line says the container
        // is up and the agent is busy, and this says which issue that
        // busyness is about. Hidden when nothing is claimed — an empty
        // line saying nothing is worse than the absence of one.
        let env_working_on = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .visible(false)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .tooltip_text(
                "The issue this environment claimed off the backlog. A claim is the \
                 env↔issue link, readable from both ends — the backlog row says the \
                 same thing from the other side.",
            )
            .build();

        // One row: what the environment is doing, what it costs, whose
        // conversation works in it, and the three things done TO it. The
        // state label takes the slack, so the buttons sit at the right edge
        // wherever this tab happens to be drawn.
        let facts_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        facts_row.append(&env_state);
        facts_row.append(&env_disk);
        facts_row.append(&env_spend);
        facts_row.append(&env_chat);
        facts_row.append(&refresh_button);
        facts_row.append(&env_actions);
        facts_row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        facts_row.append(&new_tab_button);

        let action_bar = gtk::Box::new(gtk::Orientation::Vertical, 2);
        action_bar.set_margin_top(6);
        action_bar.set_margin_bottom(6);
        action_bar.set_margin_start(12);
        action_bar.set_margin_end(12);
        action_bar.append(&facts_row);
        action_bar.append(&env_working_on);

        // --- the review band -----------------------------------------------
        // ENVIRONMENTS.md → "The review lifecycle: environments, not an
        // inbox". When an environment has said it is done, that is the
        // first thing about it and everything else is context — so it leads
        // this tab's content. A flagged environment is a PERSISTENT
        // condition, not a transient event, and `AdwBanner` is libadwaita's
        // widget for exactly that: revealed while it holds, gone once the
        // environment is working again. Absent entirely while it works: a
        // band reading "nothing to review" would be the loudest permanent
        // feature of a tab about something else.
        //
        // Merge/Reject/Destroy are more than the one action a banner's own
        // button can hold, so they sit just beneath it; Open Review IS that
        // one button, since it is always the first thing to press.
        let review_bar = adw::Banner::builder().build();
        let review_detail = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .selectable(true)
            .build();
        let review_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        review_actions.set_halign(gtk::Align::End);
        review_actions.set_valign(gtk::Align::Start);
        let review_extra = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        review_extra.set_visible(false);
        review_extra.set_margin_start(12);
        review_extra.set_margin_end(12);
        review_extra.set_margin_top(6);
        review_extra.append(&review_detail);
        review_extra.append(&review_actions);

        // --- the selected environment's panel ------------------------------
        let supervisor_log = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .build();
        let log_scroller = gtk::ScrolledWindow::builder()
            .child(&supervisor_log)
            .vexpand(true)
            .build();

        let resources_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let resources_scroller = gtk::ScrolledWindow::builder()
            .child(&resources_list)
            .vexpand(true)
            .build();

        let intervention = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .visible(false)
            .build();

        // **No nested tab sets, and no pane header either.** The sections
        // used to be an `AdwViewStack` behind an `AdwInlineViewSwitcher`
        // INSIDE one "Environment" tab, which put a second row of
        // tab-shaped controls under the first and made "which strip am I
        // in" a question the eye had to answer twice. They are siblings of
        // Services and of the terminals now — every leaf view is a
        // first-class tab in its region's one strip.
        //
        // What described the environment briefly became a header ABOVE the
        // strip; that is gone too. A header there named the environment,
        // which the file tree's panel already does permanently and at every
        // rung, and it had to be carried into the editor's strip by hand at
        // the consolidated rung and shown above tabs that were sometimes
        // somebody's file. Its facts live in the environment tab's own
        // content instead — where a page's content crosses the breakpoint
        // with the page and needs no second mechanism.
        let env_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // Leading, because an environment waiting on a judgment is not one
        // of several equal things to look at.
        env_box.append(&review_bar);
        env_box.append(&review_extra);
        env_box.append(&action_bar);
        env_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        env_box.append(&log_toolbar);
        env_box.append(&log_scroller);
        // The intervention panel is a BOTTOM panel, the same convention the
        // file tree's dirty-file flows follow — never a modal. Everything
        // that opens one (the `⋮` menu, Reject) is in this tab's content, so
        // the user is already looking at the tab it opens in.
        env_box.append(&intervention);

        let env_page = tabs.append(&env_box);
        // Titled and iconed by `refresh_fleet_badge`, which is the one
        // place this tab's glance is composed. The title matters even
        // though the pinned rendering never draws it: it is the page's
        // accessible name, it is what `AdwTabOverview`'s search matches,
        // and it IS drawn once the page is grafted (unpinned) into the
        // editor's strip.
        env_page.set_title("Environment");
        let resources_page = tabs.append(&resources_scroller);
        resources_page.set_title("Resources");
        resources_page.set_icon(Some(&gtk::gio::ThemedIcon::new("drive-harddisk-symbolic")));
        resources_page.set_tooltip("This environment's containers, volumes and images");

        let services = crate::services::ServicesPane::new(workspace.clone());
        let services_page = tabs.append(&services.widget);
        services_page.set_title("Services");
        // Neutral until the first real answer: red is reserved for issues.
        services_page.set_icon(Some(&gtk::gio::ThemedIcon::new("taste-services-none")));

        let tabbed = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tabbed.append(&tab_bar);
        tabbed.append(&tabs);
        tabs.set_vexpand(true);
        // `overview.open` is installed on the overview itself, so the
        // button that opens it lives inside — tab bar included.
        let overview = adw::TabOverview::builder()
            .view(&tabs)
            .child(&tabbed)
            .enable_search(true)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&overview);

        let console = Rc::new(Self {
            widget,
            host: RefCell::new(tabs.clone()),
            host_watch: RefCell::new(None),
            migrating: Cell::new(false),
            tabs,
            supervisor_log,
            env_page: env_page.clone(),
            resources_page: resources_page.clone(),
            services_page: services_page.clone(),
            last_section: RefCell::new(SECTIONS[0].to_string()),
            follow_log,
            host_shells: RefCell::new(Vec::new()),
            env_state: env_state.clone(),
            env_chat: env_chat.clone(),
            env_disk: env_disk.clone(),
            env_spend: env_spend.clone(),
            env_actions: env_actions.clone(),
            rows: RefCell::new(Vec::new()),
            selected: RefCell::new(EnvironmentId::primary()),
            git_facts: RefCell::new(HashMap::new()),
            claim_facts: RefCell::new(HashMap::new()),
            disk_facts: RefCell::new(HashMap::new()),
            published: RefCell::new(Vec::new()),
            state: RefCell::new(taste_core::state::WorkspaceState::default()),
            chat_lookup: RefCell::new(None),
            on_open_environment: RefCell::new(None),
            on_open_review: RefCell::new(None),
            on_close_review: RefCell::new(None),
            on_fleet_changed: RefCell::new(None),
            on_issues_changed: RefCell::new(None),
            pool: RefCell::new(PoolFacts::default()),
            on_pool_changed: RefCell::new(None),
            logs: RefCell::new(HashMap::new()),
            lifecycle: RefCell::new(HashMap::new()),
            resources_list,
            shell_tabs: RefCell::new(HashMap::new()),
            stowed_shells: RefCell::new(HashMap::new()),
            issues: RefCell::new(Vec::new()),
            review_facts: RefCell::new(HashMap::new()),
            review_bar: review_bar.clone(),
            review_detail: review_detail.clone(),
            review_actions: review_actions.clone(),
            review_extra: review_extra.clone(),
            env_working_on: env_working_on.clone(),
            flatpak_log: RefCell::new(None),
            services,
            intervention,
            probe_rows: RefCell::new(Vec::new()),
            probe_quota: RefCell::new(None),
            probe_issues: Cell::new(false),
            workspace,
            environments,
        });

        let weak = Rc::downgrade(&console);
        new_tab_button.connect_clicked(move |_| {
            if let Some(console) = weak.upgrade() {
                console.add_terminal_tab();
            }
        });
        let weak = Rc::downgrade(&console);
        refresh_button.connect_clicked(move |_| {
            if let Some(console) = weak.upgrade() {
                console.refresh_environment_data(true);
            }
        });
        // The banner's own button is always "Open Review": `render_review`
        // clears its label (which libadwaita takes as "no button") when
        // there is nothing published to open yet.
        {
            let weak = Rc::downgrade(&console);
            review_bar.connect_button_clicked(move |_| {
                if let Some(console) = weak.upgrade() {
                    console.run_review_action("open");
                }
            });
        }
        // The sections and Services are permanent fixtures.
        {
            let weak = Rc::downgrade(&console);
            console.tabs.connect_close_page(move |tabs, page| {
                let Some(console) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                console.close_request(tabs, page)
            });
        }
        console.pin_fixtures(true);
        console.watch_host();

        console.refresh_fleet();
        console.show_selected_environment();
        console.add_terminal_tab();
        // ...and the pane opens on the environment, not on the terminal
        // that opening it created. A terminal the USER asks for takes the
        // front; this one nobody asked for.
        console.show_section(SECTIONS[0]);
        console.refresh_environment_data(false);
        console
    }

    // --- one strip, wherever it is ----------------------------------------

    /// The view this pane's tabs are in right now.
    fn host(&self) -> adw::TabView {
        self.host.borrow().clone()
    }

    /// The console's own view, for the caller that moves pages back into it.
    pub fn own_view(&self) -> adw::TabView {
        self.tabs.clone()
    }

    /// This pane's pages, in strip order — what a graft moves and what an
    /// ungraft moves back.
    pub fn strip_pages(&self) -> Vec<adw::TabPage> {
        let host = self.host();
        (0..host.n_pages())
            .map(|index| host.nth_page(index))
            .filter(|page| self.owns_page(page))
            .collect()
    }

    /// Is this one of ours? Asked by the editor's close handler, which sees
    /// this pane's pages while the window is consolidated.
    pub fn owns_page(&self, page: &adw::TabPage) -> bool {
        self.is_fixture(page)
            || self
                .shell_tabs
                .borrow()
                .values()
                .any(|(_, tab)| *tab == *page)
    }

    /// Closing one of this pane's tabs, wherever the strip is.
    ///
    /// The sections and Services are fixtures and refuse; a shell's tab
    /// closing is how the user ends it — for their own terminals that IS
    /// the kill, and for the agent's it means nothing here shows that shell
    /// any more.
    pub fn close_request(&self, view: &adw::TabView, page: &adw::TabPage) -> glib::Propagation {
        if self.is_fixture(page) {
            view.close_page_finish(page, false);
            return glib::Propagation::Stop;
        }
        let closing: Vec<ShellId> = self
            .shell_tabs
            .borrow()
            .iter()
            .filter(|(_, (_, tab))| *tab == *page)
            .map(|(id, _)| *id)
            .collect();
        for id in closing {
            self.shell_tabs.borrow_mut().remove(&id);
            if self
                .workspace
                .shells
                .get(id)
                .is_some_and(|entry| entry.kind == ShellKind::User)
            {
                self.workspace.shells.remove(id);
            }
        }
        glib::Propagation::Proceed
    }

    /// Follow the selection in whichever view holds the pages: remember the
    /// section for the next landing, and keep Tail with the log.
    fn watch_host(self: &Rc<Self>) {
        let host = self.host();
        if let Some((view, id)) = self.host_watch.borrow_mut().take() {
            // The old host is not ours to keep signalling: at the
            // consolidated rung it is the editor's strip, whose file tabs
            // have nothing to say about this pane — and a handler left on
            // it would hide the log's Tail switch every time the user
            // selected a file after the window grew back.
            glib::signal_handler_disconnect(&view, id);
        }
        let weak = Rc::downgrade(self);
        let id = host.connect_selected_page_notify(move |view| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            console.note_section(view.selected_page().as_ref());
        });
        *self.host_watch.borrow_mut() = Some((host.clone(), id));
        self.note_section(host.selected_page().as_ref());
    }

    fn note_section(&self, page: Option<&adw::TabPage>) {
        if self.migrating.get() {
            return;
        }
        let name = match page {
            Some(page) if *page == self.env_page => SECTIONS[0],
            Some(page) if *page == self.resources_page => SECTIONS[1],
            // Services, a terminal, or (consolidated) somebody's file:
            // not a section, so the remembered one stands.
            _ => return,
        };
        *self.last_section.borrow_mut() = name.to_string();
    }

    fn section_page(&self, name: &str) -> adw::TabPage {
        match section_index(name) {
            1 => self.resources_page.clone(),
            _ => self.env_page.clone(),
        }
    }

    /// Raise one section, wherever the strip currently is.
    fn show_section(&self, name: &str) {
        self.host().set_selected_page(&self.section_page(name));
    }

    /// The three pages that are this pane rather than something running in
    /// it. They never close, and they are the ones that get pinned.
    fn fixtures(&self) -> [adw::TabPage; 3] {
        [
            self.env_page.clone(),
            self.resources_page.clone(),
            self.services_page.clone(),
        ]
    }

    fn is_fixture(&self, page: &adw::TabPage) -> bool {
        self.fixtures().contains(page)
    }

    /// Icon-only, or icon-and-label — decided by WHOSE strip the pages are
    /// in, and applied by pinning.
    ///
    /// `AdwTabBar` draws a pinned page as its icon alone: no title label, no
    /// close button, fixed at the left edge. In this pane's OWN strip that
    /// is exactly right — three fixtures that never move and never close,
    /// ahead of the terminals, in a pane 700px wide where three words of
    /// title are three tabs' worth of room.
    ///
    /// It is exactly WRONG in the editor's strip. A pinned page is forced
    /// leftmost, so at the consolidated rung the panes would sit in front of
    /// the user's own files — the one thing `tabfamily` exists to prevent,
    /// and the reason the chat's grafted trio is unpinned too. So the pin
    /// comes off before the crossing and goes back on after the return, and
    /// while they are guests they render the way the chat trio does: icon
    /// plus short label (`GraftedTab`'s rule — at 900px an icon-only guest
    /// among a dozen tabs is a guess).
    ///
    /// Done explicitly rather than trusting `transfer_page` to carry or drop
    /// the flag: libadwaita's pinned state is bookkeeping in the *view*
    /// (`n_pinned_pages` and the page's position in it), not a property of
    /// the page alone, so what a transfer does with it is an implementation
    /// detail of a version. This is one call either way and no version has
    /// an opinion about it.
    fn pin_fixtures(&self, pinned: bool) {
        let host = self.host();
        // Only ever OUR strip. Pinning is what makes these three icon-only
        // here; in the editor's strip they are ordinary guests whose place
        // is `tabfamily`'s business, and touching their order or their pin
        // there would be this pane reaching into somebody else's.
        if host != self.tabs {
            return;
        }
        // Pinning REORDERS, and it does so twice over. libadwaita lifts the
        // page out of the view's list and reinserts it at the pinned
        // boundary, which means (a) a list that loses its selected row
        // hands the selection to its neighbour — pinning three pages in a
        // row walked the selection three tabs down the strip and opened the
        // pane on Services — and (b) the order that comes out depends on
        // which end you started from: unpinning left to right put the
        // boundary in front of each page in turn and delivered
        // [services] [resources] [environment], reversed, which is the
        // order they then crossed into the editor's strip in.
        //
        // So: guard the remembered section the way a migration does, and
        // afterwards say plainly where these three go. Pinned or not, they
        // lead this strip, which is a legal position in both cases (all
        // three pinned, or all three at the head of the unpinned run).
        let keep = host.selected_page();
        let was_migrating = self.migrating.replace(true);
        for page in self.fixtures() {
            host.set_page_pinned(&page, pinned);
        }
        for (at, page) in self.fixtures().iter().enumerate() {
            host.reorder_page(page, at as i32);
        }
        if let Some(keep) = keep {
            host.set_selected_page(&keep);
        }
        self.migrating.set(was_migrating);
    }

    /// About to move this pane's pages to another strip: hold the
    /// remembered section still until they land, and take the pins off so
    /// the fixtures cross as ordinary pages. Paired with
    /// [`Console::set_host`], which is what ends the migration.
    pub fn begin_migration(&self) {
        self.migrating.set(true);
        self.pin_fixtures(false);
    }

    /// Say where this pane's pages now live. The caller has already moved
    /// them (an `AdwTabPage` is transferred between views, never rebuilt —
    /// a terminal's pty has to survive the crossing).
    pub fn set_host(self: &Rc<Self>, view: &adw::TabView) {
        // The migration ends here whether or not the pages ended up
        // somewhere new: a guard that could be left on would freeze the
        // remembered section for the rest of the session.
        self.migrating.set(false);
        if self.host() != *view {
            *self.host.borrow_mut() = view.clone();
            self.watch_host();
        }
        // Home again: the fixtures go back to being icon-only. In anyone
        // else's strip they stay ordinary pages — see [`pin_fixtures`].
        self.pin_fixtures(*view == self.tabs);
        // Land on the section the user was reading, not on whatever the
        // strip happened to select while the pages were moving.
        let section = self.last_section.borrow().clone();
        self.show_section(&section);
    }

    /// Is the user looking at this environment's own tab right now?
    ///
    /// The notifier's "do not tell them what they can already see" test.
    /// It used to ask whether the pane's header was mapped, which the
    /// deletion of that header took away — and which was the weaker
    /// question anyway: the header was mapped whenever the pane was, no
    /// matter which tab was in front. An `AdwTabView` maps only the
    /// selected page's child, so this asks the exact thing, and it keeps
    /// answering at the consolidated rung where the page is in the editor's
    /// strip and this pane's own widget is not in the window at all.
    pub fn fleet_on_screen(&self) -> bool {
        self.env_page.child().is_mapped()
    }

    /// Tell the fleet how to find the chat bound to an environment, and
    /// what to do when the user opens one.
    pub fn set_chat_lookup(
        &self,
        lookup: impl Fn(&EnvironmentId) -> Option<ChatBinding> + 'static,
    ) {
        *self.chat_lookup.borrow_mut() = Some(Box::new(lookup));
    }

    pub fn set_on_open_environment(&self, hook: impl Fn(EnvironmentId) + 'static) {
        *self.on_open_environment.borrow_mut() = Some(Box::new(hook));
    }

    /// Where Open Review sends the git views: the file tree, aimed at one
    /// environment's branch of record.
    pub fn set_on_open_review(&self, hook: impl Fn(String, String) + 'static) {
        *self.on_open_review.borrow_mut() = Some(Box::new(hook));
    }

    /// ...and where a settled judgment takes them back from.
    pub fn set_on_close_review(&self, hook: impl Fn() + 'static) {
        *self.on_close_review.borrow_mut() = Some(Box::new(hook));
    }

    /// The workspace state the window restored, for the environment names
    /// in it. Read once by the window, never by a render.
    pub fn set_workspace_state(self: &Rc<Self>, state: taste_core::state::WorkspaceState) {
        *self.state.borrow_mut() = state;
        self.refresh_fleet();
    }

    // --- the fleet -------------------------------------------------------

    /// Re-render the fleet from what is already known: supervisor states
    /// (in memory), cached git and disk facts, and the chat strip.
    ///
    /// Cheap by construction — nothing in here touches the filesystem, git,
    /// or podman, which is what lets it run on every state event.
    pub fn refresh_fleet(self: &Rc<Self>) {
        let mut facts: Vec<EnvFacts> = self
            .environments
            .list()
            .iter()
            .map(|supervisor| self.facts_for(supervisor))
            .collect();
        facts.extend(self.probe_rows.borrow().iter().cloned());
        let published = self.published.borrow();
        let rows = fleet::assemble(facts, &self.state.borrow(), &published);
        drop(published);
        if *self.rows.borrow() != rows {
            *self.rows.borrow_mut() = rows;
            self.render_fleet();
            self.refresh_fleet_badge();
            self.announce_fleet();
        }
        // After the rows, always: the pool's breakdown is read off them,
        // and the account's own limit state can move on a tick where no
        // row did — a turn that spent nothing this IDE can see still
        // comes back through a response carrying fresh headers.
        self.refresh_pool();
    }

    /// Take the account's limit state off the proxy, and tell whoever
    /// draws it when it moves.
    ///
    /// Rides the fleet's own 1 Hz tick rather than a timer of its own: a
    /// snapshot only ever changes when a turn finished, which is the same
    /// moment spend changes, and a second wakeup to notice the same event
    /// would be a second wakeup for nothing. Cheap enough to belong here —
    /// a mutex and a clone of a struct with two small strings in it.
    ///
    /// The equality guard matters more than usual: an idle fleet re-reads
    /// the same snapshot every second, and redrawing a gauge that says
    /// what it said a second ago is a frame nobody asked for. The *age*
    /// shown beside it moves on the panel's own tick instead.
    fn refresh_pool(self: &Rc<Self>) {
        let quota = match self.probe_quota.borrow().as_ref() {
            Some(probe) => probe.clone(),
            None => match taste_acp::authproxy::handle() {
                Some(handle) => handle.quota(),
                // No proxy: nothing was observed, and nothing here will
                // go looking. An empty snapshot is the honest answer.
                None => QuotaSnapshot::default(),
            },
        };
        // Who drew on it, off the rows the fleet was just assembled from
        // rather than a second read of the proxy. Biggest first, because
        // the question this answers is "what is eating it".
        let mut spenders: Vec<(String, u64)> = self
            .rows
            .borrow()
            .iter()
            .filter(|row| !row.spend.is_zero())
            .map(|row| (row.name.clone(), row.spend.tokens()))
            .filter(|(_, tokens)| *tokens > 0)
            .collect();
        spenders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let fresh = PoolFacts { quota, spenders };
        if *self.pool.borrow() == fresh {
            return;
        }
        *self.pool.borrow_mut() = fresh;
        self.announce_pool();
    }

    /// Who draws the pool: the environments panel's gauge and every chat
    /// pane's utilization tab.
    pub fn set_on_pool_changed(&self, hook: impl Fn(&PoolFacts) + 'static) {
        *self.on_pool_changed.borrow_mut() = Some(Box::new(hook));
    }

    fn announce_pool(&self) {
        let hook = self.on_pool_changed.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(&self.pool.borrow());
        }
    }

    /// Hand the assembled fleet to whoever else renders it — gadget mode
    /// and the varlink service, both of which take the SAME rows rather
    /// than deriving their own.
    ///
    /// Fires only when something actually moved: the guard in
    /// [`Console::refresh_fleet`] has already returned for an unchanged
    /// fleet, so a subscriber here is woken by change and not by events.
    /// `published` rides along because the notification digest needs the
    /// branch names, not just the counts the rows carry.
    /// Who draws the queue. Called on every read of `refs/taste/issues`,
    /// including the first.
    pub fn set_on_issues_changed(&self, hook: impl Fn(&[taste_git::Issue]) + 'static) {
        *self.on_issues_changed.borrow_mut() = Some(Box::new(hook));
    }

    /// Hand the queue to whoever draws it.
    fn announce_issues(&self) {
        let hook = self.on_issues_changed.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(&self.issues.borrow());
        }
    }

    pub fn set_on_fleet_changed(&self, hook: impl Fn(&[FleetRow], usize) + 'static) {
        *self.on_fleet_changed.borrow_mut() = Some(Box::new(hook));
    }

    /// Hand the current fleet to the other renderers, whether or not it
    /// moved. For the moment a subscriber attaches: the rows already exist
    /// and the unchanged-guard would otherwise keep them to itself, so a
    /// card and a socket would both start out empty.
    pub fn republish_fleet(&self) {
        self.announce_fleet();
        self.announce_pool();
    }

    fn announce_fleet(&self) {
        let hook = self.on_fleet_changed.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(&self.rows.borrow(), self.open_issues());
        }
    }

    // --- the issue queue ---------------------------------------------------

    /// Issues nobody has finished. The number the gadget card and the
    /// varlink service publish, taken from the one list the queue renders.
    pub fn open_issues(&self) -> usize {
        self.issues
            .borrow()
            .iter()
            .filter(|issue| !issue.resolution.is_resolved())
            .count()
    }

    /// Re-read `refs/taste/issues` from the user's main checkout, in the
    /// order the `order` file puts it in.
    ///
    /// Off the main thread, like every other git pass here: this is a tree
    /// walk plus a blob read per issue, and the queue moves whenever an
    /// agent files or claims something — or whenever the user reorders it
    /// from the backlog panel, which is the caller that makes the round
    /// trip visible.
    ///
    /// `ordered_issues` rather than `issues`: the order file is the user's
    /// authored sequence, and a surface that re-sorted by id would silently
    /// undo every move they made.
    pub fn refresh_issues(self: &Rc<Self>) {
        if self.probe_issues.get() {
            return; // a fabricated queue is the point of a probe instance
        }
        let main_checkout = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                taste_git::GitWorkspace::discover(&main_checkout)
                    .and_then(|git| git.ordered_issues().ok())
                    .unwrap_or_default()
            });
            let Ok(issues) = handle.await else { return };
            let Some(console) = weak.upgrade() else {
                return;
            };
            if *console.issues.borrow() == issues {
                return; // nothing moved
            }
            console.adopt_claims(&issues);
            *console.issues.borrow_mut() = issues;
            console.announce_issues();
            console.announce_fleet();
        });
    }

    /// Who is working on what, from the queue that was just read.
    ///
    /// The env↔issue link's environment end, derived on the main thread
    /// from issues already in hand — one walk of the ref answers both
    /// questions, where it used to be read once for the backlog and again
    /// on the environment pass for this. That was not only a duplicate
    /// read: the two refreshed on different triggers, so an agent claiming
    /// an issue moved the backlog row immediately and left the panel's work
    /// line saying nothing until an unrelated environment event came along.
    fn adopt_claims(&self, issues: &[taste_git::Issue]) {
        let mut claims: HashMap<EnvironmentId, Vec<taste_git::Claim>> = HashMap::new();
        for issue in issues {
            // Completed and declined alike: an environment that still
            // happens to be the assignee of a settled issue is history, not
            // work in flight, and the panel would go on saying it was
            // working on it.
            if issue.resolution.is_resolved() {
                continue;
            }
            let Some(env) = issue
                .assignee
                .as_deref()
                .and_then(|slug| EnvironmentId::parse(slug).ok())
            else {
                continue;
            };
            claims.entry(env).or_default().push(taste_git::Claim {
                id: issue.id.clone(),
                title: issue.title.clone(),
            });
        }
        // Replaced wholesale, so a released claim disappears rather than
        // lingering as the last thing an environment was seen holding.
        *self.claim_facts.borrow_mut() = claims;
    }

    /// Land on one environment: raise its section and select it. Where a
    /// notification click about an environment, and gadget mode's
    /// click-through on a row with no chat, both end up.
    pub fn reveal_environment(self: &Rc<Self>, env: &EnvironmentId) {
        let section = self.last_section.borrow().clone();
        self.show_section(&section);
        // Showing an environment means going to it — there is one
        // selection, and this asks the window to move it. `note_watching`
        // brings this panel along when it does.
        self.open_environment(env.clone());
    }

    fn facts_for(&self, supervisor: &Arc<Supervisor>) -> EnvFacts {
        let env = supervisor.id().clone();
        let chat = self
            .chat_lookup
            .borrow()
            .as_ref()
            .and_then(|lookup| lookup(&env));
        EnvFacts {
            state: supervisor.state(),
            authority: supervisor.config_authority(),
            pending_rebuild: supervisor.pending_changes(),
            chat,
            git: self.git_facts.borrow().get(&env).cloned(),
            disk: self.disk_facts.borrow().get(&env).copied(),
            review: self.workspace.review.state(&env),
            working_on: self
                .claim_facts
                .borrow()
                .get(&env)
                .cloned()
                .unwrap_or_default(),
            spend: taste_acp::authproxy::handle()
                .map(|handle| {
                    let spend = handle.spend(env.as_str());
                    fleet::Spend {
                        requests: spend.requests,
                        input_tokens: spend.input_tokens,
                        output_tokens: spend.output_tokens,
                    }
                })
                .unwrap_or_default(),
            // An in-memory Vec, filtered by environment — cheap enough to
            // be part of a render, which is the bar everything in here has
            // to clear.
            shells: self.workspace.shells.list(Some(&env)).len(),
            env,
        }
    }

    /// The top of the one environment this tab is about.
    ///
    /// This was a list of every environment, then a header naming one.
    /// Both are gone: the file tree's panel enumerates them permanently,
    /// with a traffic light and a sparkline each, and NAMES the selected
    /// one — so a list here was a second rendering of the same `FleetRow`s
    /// for the same glance, and a name here was a second rendering of one
    /// row. What a one-line panel row cannot carry is what stayed: the
    /// state in words, the lifecycle actions, the build log, podman's
    /// resources.
    fn render_fleet(self: &Rc<Self>) {
        let env = self.selected.borrow().clone();
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == env)
            .cloned();
        while let Some(child) = self.env_chat.first_child() {
            self.env_chat.remove(&child);
        }
        let Some(row) = row else {
            // The fleet has not been assembled yet, or this environment was
            // destroyed under the tab. Admit it and stop — the panel still
            // names the environment even when this tab has nothing on it.
            self.env_state.set_label("state not known yet");
            self.env_state.set_tooltip_text(None);
            self.env_disk.set_label("");
            self.env_spend.set_label("");
            self.env_working_on.set_visible(false);
            self.review_bar.set_revealed(false);
            self.review_extra.set_visible(false);
            self.env_actions.set_sensitive(false);
            return;
        };
        self.env_state.set_label(&Self::env_state_line(&row));
        // The short form is on the line; what it MEANS for what can run and
        // what can be written is a sentence, and a sentence belongs in a
        // tooltip rather than on a row the eye scans.
        self.env_state.set_tooltip_text(Some(row.mode_explainer()));
        // A dash for "not measured yet" belongs in a table with fixed
        // columns. Here it is one more thing crowding the row's own words:
        // nothing measured, nothing shown.
        for (label, text) in [
            (&self.env_disk, row.disk_text()),
            (&self.env_spend, row.spend_text()),
        ] {
            label.set_visible(text != "—");
            label.set_label(&text);
        }
        // What it is working ON. `working_on_text` is the fleet row's own
        // phrasing, so the console and any other surface that shows a
        // claim say the same sentence.
        match row.working_on_text() {
            Some(text) => {
                self.env_working_on.set_label(&format!("working on {text}"));
                self.env_working_on.set_visible(true);
            }
            None => self.env_working_on.set_visible(false),
        }
        self.env_actions.set_sensitive(true);
        self.env_actions.set_popover(Some(&self.env_menu(&row)));
        self.render_review(&row);

        if let Some(chat) = &row.chat {
            if chat.busy {
                let spinner = gtk::Spinner::new();
                spinner.start();
                self.env_chat.append(&spinner);
            }
            // The role, as the same quiet glyph the tab wears.
            if chat.orchestrator {
                let mark = gtk::Image::from_icon_name("system-users-symbolic");
                mark.add_css_class("dim-label");
                self.env_chat.append(&mark);
            }
            self.env_chat.append(
                &gtk::Label::builder()
                    .label(glib::markup_escape_text(&chat.label))
                    .css_classes(["caption", "dim-label"])
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let role = if chat.orchestrator {
                "\nIt is this workspace's orchestrator: it can create and drive other chats."
            } else {
                ""
            };
            self.env_chat.set_tooltip_text(Some(&if chat.awaits_user {
                format!("{} works here, and is waiting for you{role}", chat.label)
            } else if chat.busy {
                format!("{} works here, and is working now{role}", chat.label)
            } else {
                format!("{} works here{role}", chat.label)
            }));
        }
    }

    /// The environment tab's own state line: the state word, and the
    /// container facts that stay with it (unpublished and published work).
    ///
    /// NOT the branch and NOT the dirty count. Those are working-tree
    /// facts, and the file tree is where working-tree facts live — exactly
    /// as the environment's *name* lives in the panel above it and not
    /// here. This line is always on screen while the tab is; the fuller
    /// sentence, branch and all, is [`Console::env_facts_line`], which
    /// hangs off the tab's tooltip where a hover asks for it.
    ///
    /// It opens with [`FleetRow::state_text`], which no longer spends its
    /// first two words saying "container mode": every environment that is
    /// up is a container, so the normal case is unmarked and the line
    /// starts with what is actually happening. See
    /// [`FleetRow::mode_text`] for the ladder it does name.
    fn env_state_line(row: &FleetRow) -> String {
        let mut text = row.state_text();
        if let Some(git) = &row.git {
            if git.unpublished > 0 {
                text.push_str(&format!(" · {} unpublished", git.unpublished));
            }
        }
        if row.published > 0 {
            text.push_str(&format!(" · ↑{} published", row.published));
        }
        text
    }

    /// The fuller sentence, for the tab's own tooltip: the state line plus
    /// the branch and the dirty count. A hover detail, not a permanently
    /// visible repeat of what the file tree already shows.
    fn env_facts_line(row: &FleetRow) -> String {
        let mut text = row.state_text();
        if let Some(git) = &row.git {
            if let Some(branch) = &git.branch {
                text.push_str(&format!(" · {branch}"));
            }
            // Two different facts, never added together: commits the
            // checkout has never seen, and files not committed at all.
            if git.unpublished > 0 {
                text.push_str(&format!(" · {} unpublished", git.unpublished));
            }
            if git.dirty > 0 {
                text.push_str(&format!(" · {} dirty", git.dirty));
            }
        }
        if row.published > 0 {
            text.push_str(&format!(" · ↑{} published", row.published));
        }
        text
    }

    /// The review banner: what a finished environment is asking of the
    /// user, and the things they can do about it.
    ///
    /// Absent while the environment is working, which is nearly always.
    /// The alternative — a permanent band reading "nothing to review" —
    /// would make this tab's most prominent element a statement about the
    /// absence of news. `AdwBanner`'s own reveal animation is what says it
    /// arrived, and its revealed state is what says it still holds.
    fn render_review(self: &Rc<Self>, row: &FleetRow) {
        let mark = row.review_mark();
        if mark == crate::fleet::ReviewMark::None {
            self.review_bar.set_revealed(false);
            self.review_extra.set_visible(false);
            return;
        }
        let name = crate::envstrip::title_of(row);
        self.review_bar
            .set_title(&ReviewFacts::headline(&name, row.review));

        let facts = self.review_facts.borrow().get(&row.env).cloned();
        self.review_detail.set_label(&match &facts {
            Some(facts) => facts.detail(),
            // The git pass has not run for this environment yet. Say that,
            // rather than showing a branch line assembled out of nothing.
            None => format!("{} — checking the branch…", row.review.detail()),
        });

        while let Some(child) = self.review_actions.first_child() {
            self.review_actions.remove(&child);
        }
        // Open Review IS the banner's own button: judging before looking is
        // the thing this band exists to prevent, so it is the one action
        // that gets the banner's single slot. An empty label is how
        // `AdwBanner` hides that button, and it is offered only when there
        // is a published branch for it to go to.
        let published = facts.as_ref().is_some_and(|f| f.mergedness.is_some());
        self.review_bar
            .set_button_label(if published { Some("Open Review") } else { None });
        if row.review.flagged() {
            if facts.as_ref().is_some_and(ReviewFacts::mergeable) {
                self.review_actions.append(&self.review_button(
                    "Merge",
                    &["suggested-action"],
                    "merge",
                ));
            }
            self.review_actions
                .append(&self.review_button("Reject", &["flat"], "reject"));
        } else if row.destroyable() {
            // Settled. The one thing left is to let it go — and the
            // destroy is warning-free now, because the user has already
            // looked at the branch and ruled on it.
            self.review_actions.append(&self.review_button(
                "Destroy Environment",
                &["destructive-action"],
                "destroy",
            ));
        }
        self.review_detail.set_visible(true);
        self.review_extra.set_visible(true);
        self.review_bar.set_revealed(true);
    }

    fn review_button(
        self: &Rc<Self>,
        label: &str,
        classes: &[&str],
        action: &'static str,
    ) -> gtk::Button {
        let button = gtk::Button::builder()
            .label(label)
            .css_classes(classes.to_vec())
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(console) = weak.upgrade() {
                console.run_review_action(action);
            }
        });
        button
    }

    /// The four review actions. Every one of them is USER-initiated — this
    /// band is the only thing that presses them — which is what makes
    /// Open Review's container-free git work and Merge's host-side libgit2
    /// both fine here.
    fn run_review_action(self: &Rc<Self>, action: &str) {
        let env = self.selected.borrow().clone();
        let facts = self.review_facts.borrow().get(&env).cloned();
        match action {
            "open" => {
                if let Some(facts) = facts {
                    self.open_review(&facts.branch, &facts.target);
                }
            }
            "merge" => {
                let Some(facts) = facts else { return };
                self.clone().merge_review(env, facts);
            }
            "reject" => self.clone().reject_intervention(&env),
            "destroy" => self.destroy_intervention(&env),
            _ => {}
        }
    }

    /// Aim the git views at an environment's branch — the file tree's
    /// changed-file list over `changed_since_base`, and the diffs its rows
    /// open.
    ///
    /// The target travels with the branch. The band computed it to say how
    /// far ahead the work is; the list diffs against it and the tabs name
    /// it, so all three are answering with the same "in".
    fn open_review(self: &Rc<Self>, branch: &str, target: &str) {
        let hook = self.on_open_review.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(branch.to_string(), target.to_string());
        }
    }

    /// The environment has been ruled on: take its review off the panes.
    ///
    /// Merging or rejecting is the end of the question the review was
    /// asking, and a changed-files list left standing over a settled branch
    /// invites a second judgment on work already judged.
    fn close_review(&self) {
        if let Some(hook) = self.on_close_review.borrow().as_ref() {
            hook();
        }
    }

    /// Merge an environment's branch into the user's checkout, then record
    /// the decision.
    ///
    /// Host-side libgit2 (`merge_branch`), which runs no hooks and touches
    /// no container — the same mediation publish uses in the other
    /// direction. USER-initiated, and the only thing that ever presses it
    /// is this button.
    ///
    /// The state moves only if the merge actually advanced or was already
    /// in: recording "merged" over a merge that refused would be exactly
    /// the latch this lifecycle is built to avoid.
    fn merge_review(self: &Rc<Self>, env: EnvironmentId, facts: ReviewFacts) {
        let root = self.workspace.root().to_path_buf();
        let branch = facts.branch.clone();
        let events = self.workspace.events.clone();
        let review = self.workspace.review.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let merging = branch.clone();
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = taste_git::GitWorkspace::discover(&root)
                    .ok_or_else(|| "this workspace is not a git repository".to_string())?;
                git.merge_branch(&merging).map_err(|e| format!("{e:#}"))
            });
            let Ok(outcome) = handle.await else { return };
            match outcome {
                // `clean()` covers both landings that count: the merge
                // moved the branch, or the target already had the work.
                // Either way the user has ruled and the environment is
                // settled; only a conflict leaves it still asking.
                Ok(outcome) if outcome.clean() => {
                    // The record, not the fact: whether the work is IN the
                    // target stays a fresh query every time it is asked.
                    let recorded = crate::runtime::runtime()
                        .spawn_blocking(move || review.set(&env, taste_core::ReviewState::Merged));
                    let _ = recorded.await;
                    events.publish(taste_core::Event::Toast(format!("Merged {branch}")));
                    events.publish(taste_core::Event::FileTreeChanged);
                    // The question is answered: the review list and its
                    // diffs go, rather than standing over a branch that is
                    // now in.
                    if let Some(console) = weak.upgrade() {
                        console.close_review();
                    }
                }
                Ok(outcome) => {
                    // A conflict or a refusal. Nothing was written, and
                    // nothing is recorded — the environment is still
                    // waiting for a judgment it has not received.
                    let files = outcome.conflicts.len();
                    events.publish(taste_core::Event::Toast(format!(
                        "{branch} does not merge cleanly — {files} conflicting file{}. \
                         Nothing was changed, and it is still waiting for review.",
                        if files == 1 { "" } else { "s" }
                    )));
                }
                Err(e) => events.publish(taste_core::Event::Toast(format!("Merge failed: {e}"))),
            }
            if let Some(console) = weak.upgrade() {
                console.refresh_environment_data(false);
            }
        });
    }

    /// Reject an environment: record the decision, and optionally say why
    /// on the issue it claimed.
    ///
    /// The comment is the point of the panel. A rejection with no reason
    /// leaves the next environment to pick that issue up with no idea what
    /// was already tried, and the claim is exactly the link that knows
    /// where to put the note.
    fn reject_intervention(self: &Rc<Self>, env: &EnvironmentId) {
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == *env)
            .cloned();
        let name = row
            .as_ref()
            .map(crate::envstrip::title_of)
            .unwrap_or_else(|| env.to_string());
        let claim = row.as_ref().and_then(|row| row.working_on.first().cloned());

        let content = self.open_intervention(&format!("Reject {name}?"));
        content.append(
            &gtk::Label::builder()
                .label(match &claim {
                    Some(claim) => format!(
                        "Its branch stays where it is — rejecting is a decision, not a \
                         delete. The note below is posted to {}, so whoever picks it up \
                         next knows what was already tried.",
                        claim.id
                    ),
                    None => "Its branch stays where it is — rejecting is a decision, not a \
                             delete. It claimed no issue, so there is nowhere to leave a \
                             note; the environment becomes safe to destroy."
                        .to_string(),
                })
                .css_classes(["caption", "dim-label"])
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        let comment = gtk::Entry::builder()
            .placeholder_text("Why (optional)")
            .hexpand(true)
            .visible(claim.is_some())
            .build();
        let reject = gtk::Button::builder()
            .label("Reject")
            .css_classes(["destructive-action"])
            .build();
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row_box.append(&comment);
        row_box.append(&reject);
        content.append(&row_box);

        let env = env.clone();
        let weak = Rc::downgrade(self);
        reject.connect_clicked(move |button| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            button.set_sensitive(false);
            let text = comment.text().trim().to_string();
            let issue = claim.as_ref().map(|claim| claim.id.clone());
            let root = console.workspace.root().to_path_buf();
            let review = console.workspace.review.clone();
            let events = console.workspace.events.clone();
            let env = env.clone();
            let weak = Rc::downgrade(&console);
            glib::spawn_future_local(async move {
                let recorded = env.clone();
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    review.set(&recorded, taste_core::ReviewState::Rejected)?;
                    // The note, when there is one and somewhere to put it.
                    if let (Some(issue), false) = (issue, text.is_empty()) {
                        let git = taste_git::GitWorkspace::discover(&root)
                            .ok_or_else(|| anyhow::anyhow!("no git repository"))?;
                        let target = git.issue_target_branch();
                        let change = taste_git::IssueChange {
                            comment: Some(text),
                            ..Default::default()
                        };
                        git.issue_update(&issue, &change, &target, "primary")?;
                    }
                    Ok::<(), anyhow::Error>(())
                });
                let settled = match handle.await {
                    Ok(Ok(())) => {
                        events.publish(taste_core::Event::Toast(format!("Rejected {env}")));
                        true
                    }
                    Ok(Err(e)) => {
                        events.publish(taste_core::Event::Toast(format!("Reject failed: {e:#}")));
                        false
                    }
                    Err(_) => return,
                };
                if let Some(console) = weak.upgrade() {
                    // Ruled on: the review leaves the panes with it. A
                    // failed reject settles nothing and leaves it up.
                    if settled {
                        console.close_review();
                    }
                    console.close_intervention();
                    console.refresh_environment_data(false);
                    console.refresh_issues();
                }
            });
        });
    }

    /// The header's action menu: lifecycle and destruction, for the one
    /// environment this tab is about.
    ///
    /// "Open Environment" went with the list and is not missed — this tab
    /// shows wherever the panes are aimed, so opening the environment it is
    /// already showing is a no-op. Aiming them somewhere else is the file
    /// tree panel's job, one click, no menu.
    fn env_menu(self: &Rc<Self>, row: &FleetRow) -> gtk::Popover {
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::builder().child(&menu_box).build();

        // Whether a container is UP, not whether it is the project's: a
        // baseline container is just as stoppable, and asking the mode here
        // offered Start for something already running.
        let running = row.container_running();
        let entries: Vec<(&str, &str, bool, &'static str, String)> = vec![
            (
                "Start",
                "media-playback-start-symbolic",
                !running,
                "start",
                "Build if needed, then start this environment's container".into(),
            ),
            (
                "Stop",
                "media-playback-stop-symbolic",
                running,
                "stop",
                "Stop and remove the container (the clone stays)".into(),
            ),
            (
                "Rebuild",
                "view-refresh-symbolic",
                true,
                "rebuild",
                "Rebuild and restart from the current configuration".into(),
            ),
            (
                "Nuke",
                "user-trash-symbolic",
                true,
                "nuke",
                "Remove the container AND its image; the next start rebuilds from scratch".into(),
            ),
            (
                "Rename…",
                "document-edit-symbolic",
                !row.primary,
                "rename",
                "Give this environment a name you will recognise".into(),
            ),
            (
                "Destroy…",
                "edit-delete-symbolic",
                row.destroyable(),
                "destroy",
                if row.primary {
                    "Your checkout is not the IDE's to destroy".into()
                } else {
                    "Remove the clone, its container and its volumes — after saying what is lost"
                        .into()
                },
            ),
        ];
        for (label, icon, enabled, action, tip) in entries {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.set_halign(gtk::Align::Start);
            content.append(&gtk::Image::from_icon_name(icon));
            content.append(&gtk::Label::new(Some(label)));
            let item = gtk::Button::builder()
                .child(&content)
                .css_classes(["flat"])
                .width_request(220)
                // Disabled, never hidden: an action that does not apply to
                // this environment still says it exists.
                .sensitive(enabled)
                .tooltip_text(&tip)
                .build();
            if matches!(action, "nuke" | "destroy") {
                item.add_css_class("destructive-action");
            }
            let weak = Rc::downgrade(self);
            let env = row.env.clone();
            let popover = popover.clone();
            let action: &'static str = action;
            item.connect_clicked(move |_| {
                popover.popdown();
                // Deferred to an idle: several of these re-render the fleet,
                // and disposing this button's own row while its click
                // handler is still on the stack is how a popover loses the
                // anchor it is popping down against.
                let weak = weak.clone();
                let env = env.clone();
                glib::idle_add_local_once(move || {
                    if let Some(console) = weak.upgrade() {
                        console.run_row_action(action, env.clone());
                    }
                });
            });
            menu_box.append(&item);
        }
        popover
    }

    fn run_row_action(self: &Rc<Self>, action: &str, env: EnvironmentId) {
        let Some(supervisor) = self.environments.get(&env) else {
            // A probe row, or one destroyed under the open menu.
            return;
        };
        match action {
            "rename" => self.rename_intervention(&env),
            "destroy" => self.destroy_intervention(&env),
            "stop" => {
                let events = self.workspace.events.clone();
                crate::runtime::runtime().spawn(async move {
                    if let Err(e) = supervisor.stop().await {
                        events.publish(taste_core::Event::Toast(format!("Stop failed: {e}")));
                    }
                });
            }
            "start" | "rebuild" => {
                let events = self.workspace.events.clone();
                crate::runtime::runtime().spawn(async move {
                    if let Err(e) = supervisor.reload().await {
                        events.publish(taste_core::Event::Toast(format!("{env}: {e}")));
                    }
                });
            }
            "nuke" => {
                let weak = Rc::downgrade(self);
                self.clone().confirm_destructive(
                    &format!("Nuke {env}?"),
                    "Removes the container and its image. The next start rebuilds \
                     from scratch. The clone and named volumes are kept.",
                    "Remove",
                    move || {
                        let supervisor = supervisor.clone();
                        let weak = weak.clone();
                        let handle =
                            crate::runtime::runtime().spawn(async move { supervisor.nuke().await });
                        glib::spawn_future_local(async move {
                            let _ = handle.await;
                            if let Some(console) = weak.upgrade() {
                                console.refresh_environment_data(false);
                            }
                        });
                    },
                );
            }
            _ => {}
        }
    }

    /// Ask the window to aim its panes at an environment. The one way this
    /// pane moves the selection, and it does it by asking rather than by
    /// changing anything of its own.
    fn open_environment(self: &Rc<Self>, env: EnvironmentId) {
        let hook = self.on_open_environment.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(env);
        }
    }

    /// Follow the panes. Nothing in this tab picks an environment any more
    /// — the panel does, and this is how it brings the tab along: the
    /// detail header, the tab badge, the detail pages and the shell tabs
    /// all re-aim together, because a console showing one environment's
    /// header over another's shells is the disagreement deleting the
    /// second listing was meant to make impossible.
    pub fn note_watching(self: &Rc<Self>, env: &EnvironmentId) {
        if *self.selected.borrow() == *env {
            return;
        }
        *self.selected.borrow_mut() = env.clone();
        self.render_fleet();
        self.refresh_fleet_badge();
        self.show_selected_environment();
        self.sync_shell_tabs();
    }

    /// Re-read what a render cannot: each environment's branch and
    /// unpublished work, the user's published branches, podman resources,
    /// and — when asked — the disk footprint.
    ///
    /// All of it off the main thread. `deep` is the explicit refresh: it
    /// adds the directory walks, which are the expensive half and are never
    /// a side effect of anything else.
    pub fn refresh_environment_data(self: &Rc<Self>, deep: bool) {
        self.services.refresh();
        self.refresh_resources();

        let main_checkout = self.workspace.root().to_path_buf();
        let clones: Vec<(EnvironmentId, PathBuf)> = self
            .environments
            .list()
            .iter()
            .map(|supervisor| (supervisor.id().clone(), supervisor.root().to_path_buf()))
            .collect();
        // Which environments have left `Working`, so the merge-base
        // question is asked about those and no others. Asking it for every
        // environment on every pass would be a revwalk per row for a band
        // that is not on screen.
        let review = self.workspace.review.clone();
        let under_review: Vec<EnvironmentId> = clones
            .iter()
            .map(|(env, _)| env.clone())
            .filter(|env| review.state(env) != taste_core::ReviewState::Working)
            .collect();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let hub = taste_git::GitWorkspace::discover(&main_checkout);
                // The branch of record against the branch the user is on.
                // `issue_target_branch` is the same target the issue close
                // gate verifies against — two answers to "merged into
                // what" is one too many.
                let target = hub
                    .as_ref()
                    .map(|git| git.issue_target_branch())
                    .unwrap_or_else(|| "HEAD".to_string());
                let mut review_facts: Vec<(EnvironmentId, ReviewFacts)> = Vec::new();
                for env in under_review {
                    let mergedness = hub
                        .as_ref()
                        .and_then(|git| git.env_mergedness(env.as_str(), &target).ok())
                        .flatten();
                    review_facts.push((
                        env.clone(),
                        ReviewFacts {
                            branch: taste_git::env_branch(env.as_str()),
                            target: target.clone(),
                            mergedness,
                        },
                    ));
                }
                let published = hub
                    .as_ref()
                    .and_then(|git| git.branches_matching(taste_git::ENV_BRANCH_PREFIX).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|branch| branch.name)
                    .collect::<Vec<String>>();
                // No walk of the issues ref here. It used to be read a
                // second time on this pass, for the claims — which meant
                // "what is this environment working on" was only as fresh
                // as the last environment event, while the queue itself
                // refreshed on every `GitStatusChanged`. An agent claiming
                // an issue moved the backlog row and left the panel's work
                // line stale until something unrelated happened.
                //
                // One read now, in `refresh_issues`, which derives both —
                // and this pass ends by calling it.
                let mut facts: Vec<(EnvironmentId, EnvGit)> = Vec::new();
                for (env, root) in clones {
                    let Some(git) = taste_git::GitWorkspace::discover(&root) else {
                        continue;
                    };
                    let unpublished = if env.is_primary() {
                        // The primary IS the hub: it publishes to nobody,
                        // so "unpublished" is not a thing it can have.
                        0
                    } else {
                        taste_git::unpublished_work(&root, &main_checkout)
                            .map(|work| work.len())
                            .unwrap_or(0)
                    };
                    facts.push((
                        env,
                        EnvGit {
                            branch: git.branch_name(),
                            unpublished,
                            dirty: git.status().map(|status| status.len()).unwrap_or(0),
                        },
                    ));
                }
                (published, facts, review_facts)
            });
            let Ok((published, facts, review_facts)) = handle.await else {
                return;
            };
            let Some(console) = weak.upgrade() else {
                return;
            };
            // A probe instance's fabricated fleet is the point of that
            // instance; the real checkout's (empty) branch list must not
            // land on top of it a beat later.
            if console.probe_rows.borrow().is_empty() {
                *console.published.borrow_mut() = published;
            }
            let mut cache = console.git_facts.borrow_mut();
            for (env, git) in facts {
                cache.insert(env, git);
            }
            drop(cache);
            // Replaced wholesale: an environment that went back to
            // Working, or was destroyed, must not keep a stale branch
            // comparison the band would go on drawing.
            //
            // ...except under a probe, whose fabricated fleet has no
            // environments the real checkout knows about — clearing here
            // would wipe the seeded mergedness a beat after it was
            // planted, exactly as it would wipe the seeded branch list.
            if console.probe_rows.borrow().is_empty() {
                let mut review_cache = console.review_facts.borrow_mut();
                review_cache.clear();
                for (env, facts) in review_facts {
                    review_cache.insert(env, facts);
                }
            }
            console.refresh_fleet();
            console.refresh_issues();
            if deep {
                console.refresh_disk();
            }
        });
    }

    /// Walk every environment's footprint. Explicit, cached, and never on
    /// the main thread: this is a directory walk over checkouts and volume
    /// mountpoints, and doing it on a render would make every state event
    /// cost a `du`.
    fn refresh_disk(self: &Rc<Self>) {
        let supervisors = self.environments.list();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn(async move {
                let mut out = Vec::new();
                for supervisor in supervisors {
                    out.push((supervisor.id().clone(), supervisor.disk_usage().await));
                }
                out
            });
            let Ok(usage) = handle.await else { return };
            let Some(console) = weak.upgrade() else {
                return;
            };
            let mut cache = console.disk_facts.borrow_mut();
            for (env, disk) in usage {
                cache.insert(env, disk);
            }
            drop(cache);
            console.refresh_fleet();
        });
    }

    /// An environment's container moved. Live in the row, immediately.
    pub fn on_environment_state(self: &Rc<Self>, env: &EnvironmentId, running: bool) {
        if env.is_primary() && running {
            // Attached: host consoles retire; work happens inside. Open a
            // devcontainer shell in their place if any were up.
            let stale: Vec<adw::TabPage> = self.host_shells.borrow_mut().drain(..).collect();
            let had_hosts = !stale.is_empty();
            for page in stale {
                self.host().close_page(&page);
            }
            if had_hosts {
                self.add_terminal_tab();
            }
        }
        self.refresh_fleet();
        // A container coming or going changes what podman has to say about
        // this environment, and what its clone's git looks like.
        self.refresh_environment_data(false);
    }

    /// Compose the environment tab's whole glance: its icon, its badges and
    /// its tooltip.
    ///
    /// This is the ONE place that happens, and it is the tab rather than a
    /// header because the tab is what is on screen when the tab is not
    /// selected. Pinned in this pane's own strip, it draws as the icon
    /// alone, so the icon has to carry the container's state; the title is
    /// the constant "Environment", which is what gets drawn once the page
    /// is grafted into the editor's strip (unpinned) beside the chat's
    /// [Chat] [Usage] [Agent].
    ///
    /// It deliberately does NOT title itself with the environment's name.
    /// The file tree's panel names the selected environment permanently and
    /// at every rung; a tab saying it again would be a second rendering of
    /// one row, and the stale one is always whichever the user is not
    /// looking at. The name is in the tooltip, where a hover asks for it.
    fn refresh_fleet_badge(&self) {
        let env = self.selected.borrow().clone();
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == env)
            .cloned();
        let Some(row) = row else {
            self.env_page.set_tooltip("");
            self.env_page.set_needs_attention(false);
            self.env_page
                .set_icon(Some(&gtk::gio::ThemedIcon::new("taste-container-off")));
            self.env_page.set_indicator_icon(gtk::gio::Icon::NONE);
            self.env_page.set_indicator_tooltip("");
            return;
        };
        // The name AND the full facts sentence — branch and dirty count
        // included — because a tooltip is asked for, unlike the state line
        // in the content, which is always on screen and therefore says only
        // what the file tree is not already saying.
        self.env_page
            .set_tooltip(&format!("{}\n{}", row.env, Self::env_facts_line(&row)));
        // Needs-attention is the strip's way of saying "come here": the
        // environment failed, or it is flagged and waiting on a judgment,
        // or its conversation has stopped on a question. All three are
        // things the user has to answer, and none of them is visible while
        // another tab is in front.
        let awaiting = row.chat.as_ref().is_some_and(|chat| chat.awaits_user);
        self.env_page.set_needs_attention(
            matches!(row.state, SupervisorState::Failed { .. }) || row.review.flagged() || awaiting,
        );
        // A pinned tab draws its icon and nothing else, so the icon is the
        // container's state: up, standing in, or not there.
        //
        // Drift rides the same icon rather than an indicator badge of its
        // own, and that is a correction the screenshot made. `AdwTabPage`'s
        // indicator icon *replaces* the tab icon on a PINNED page — so a
        // drift badge here cost the container-state glyph entirely, and the
        // frame showed an update arrow where the running container used to
        // be. It would also have been a third rendering of one fact: the
        // state line already reads "running · needs rebuild", in words,
        // right under this tab. One channel, and it is the icon.
        self.env_page.set_icon(Some(&gtk::gio::ThemedIcon::new(
            if row.pending_rebuild || row.baseline() {
                // Up, but not as asked: a baseline standing in for the
                // project's config, or a container whose config has moved
                // on without it. Both are the warn icon's meaning, and both
                // match the amber light the same row reports in the panel.
                "taste-container-warn"
            } else if row.container_mode() {
                "taste-container-on"
            } else {
                "taste-container-off"
            },
        )));
        self.env_page.set_indicator_icon(gtk::gio::Icon::NONE);
        self.env_page.set_indicator_tooltip("");
    }

    // --- the selected environment's detail -------------------------------

    fn selected_supervisor(&self) -> Option<Arc<Supervisor>> {
        self.environments.get(&self.selected.borrow())
    }

    fn show_selected_environment(self: &Rc<Self>) {
        let env = self.selected.borrow().clone();
        self.supervisor_log.set_buffer(Some(&self.log_buffer(&env)));
        self.scroll_log_to_end();
        self.refresh_resources();
    }

    /// One log buffer per environment, seeded from that environment's own
    /// ring the first time it is shown — an environment that built before
    /// the user ever looked at it still has its build to show.
    fn log_buffer(self: &Rc<Self>, env: &EnvironmentId) -> gtk::TextBuffer {
        if let Some(buffer) = self.logs.borrow().get(env) {
            return buffer.clone();
        }
        let buffer = gtk::TextBuffer::new(None);
        if let Some(supervisor) = self.environments.get(env) {
            let backlog = supervisor.logs_tail(2000).join("\n");
            if !backlog.is_empty() {
                buffer.set_text(&format!("{backlog}\n"));
            }
        }
        self.logs.borrow_mut().insert(env.clone(), buffer.clone());
        buffer
    }

    /// The lifecycle stream as a roster row: an environment building itself
    /// is something it is running, and the roster is where the fleet says
    /// what is running. Read-only and unkillable by construction — there is
    /// no process of ours to signal, and stopping a build is Stop.
    fn lifecycle_sink(&self, env: &EnvironmentId) -> ShellSink {
        if let Some(sink) = self.lifecycle.borrow().get(env) {
            return sink.clone();
        }
        let sink = self.workspace.shells.register(
            env.clone(),
            ShellKind::Lifecycle,
            "devcontainer build and lifecycle",
            None,
        );
        self.lifecycle
            .borrow_mut()
            .insert(env.clone(), sink.clone());
        sink
    }

    /// Re-query podman for the selected environment's resources.
    pub fn refresh_resources(self: &Rc<Self>) {
        let Some(supervisor) = self.selected_supervisor() else {
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle =
                crate::runtime::runtime().spawn(async move { supervisor.list_resources().await });
            let Ok(resources) = handle.await else { return };
            let Some(console) = weak.upgrade() else {
                return;
            };
            console.render_resources(&resources);
        });
    }

    fn render_resources(self: &Rc<Self>, resources: &[ResourceInfo]) {
        while let Some(child) = self.resources_list.first_child() {
            self.resources_list.remove(&child);
        }
        if resources.is_empty() {
            let empty = gtk::Label::builder()
                .label("No containers or images yet — start this environment to create them.")
                .css_classes(["dim-label"])
                .margin_top(8)
                .margin_bottom(8)
                .build();
            self.resources_list.append(&empty);
            return;
        }
        // These resources ARE a hierarchy: the container on top, the
        // image it committed and the volumes mounted into it beneath;
        // base images stand alone.
        let container_names: Vec<&str> = resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Container)
            .map(|r| r.name.as_str())
            .collect();
        let depth_of = |resource: &ResourceInfo| -> i32 {
            match resource.kind {
                // The substrate is what everything else sits on, so it
                // sits at the top of the tree rather than under a
                // container it does not belong to.
                ResourceKind::Substrate => 0,
                ResourceKind::Container => 0,
                ResourceKind::Image => {
                    if container_names.iter().any(|c| resource.name.contains(c)) {
                        1
                    } else {
                        0
                    }
                }
                ResourceKind::Volume => i32::from(!container_names.is_empty()),
            }
        };
        let mut ordered: Vec<&ResourceInfo> = resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Substrate)
            .collect();
        for container in resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Container)
        {
            ordered.push(container);
            ordered.extend(resources.iter().filter(|r| {
                r.kind == ResourceKind::Image && r.name.contains(container.name.as_str())
            }));
            ordered.extend(resources.iter().filter(|r| r.kind == ResourceKind::Volume));
        }
        // Anything not claimed above (base images; everything, when no
        // container runs).
        let claimed: Vec<(ResourceKind, String)> =
            ordered.iter().map(|o| (o.kind, o.name.clone())).collect();
        ordered.extend(
            resources
                .iter()
                .filter(|r| !claimed.contains(&(r.kind, r.name.clone()))),
        );
        for resource in ordered {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            // Uniform row height: the tallest row (the one carrying a
            // button) sets the standard for all of them.
            row.set_height_request(34);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            row.set_margin_start(8 + depth_of(resource) * 22);
            row.set_margin_end(8);
            let icon = gtk::Image::from_icon_name(match resource.kind {
                ResourceKind::Container => "utilities-terminal-symbolic",
                ResourceKind::Image => "drive-harddisk-symbolic",
                ResourceKind::Volume => "folder-symbolic",
                ResourceKind::Substrate => "computer-symbolic",
            });
            let name = gtk::Label::builder()
                .label(&resource.name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build();
            // podman capitalizes mid-sentence ("Up About an hour"):
            // sentence-case it for display.
            let mut status_text = resource.status.to_lowercase();
            if let Some(first) = status_text.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            let status = gtk::Label::builder()
                .label(&status_text)
                .css_classes(["dim-label", "caption"])
                .build();
            row.append(&icon);
            row.append(&name);
            row.append(&status);

            // Volumes are caches with their own (guarded) removal.
            if resource.kind == ResourceKind::Volume && resource.status == "present" {
                let delete = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Remove this volume (cache contents are lost)")
                    .css_classes(["flat"])
                    .build();
                let weak = Rc::downgrade(self);
                let volume = resource.name.clone();
                delete.connect_clicked(move |_| {
                    let Some(console) = weak.upgrade() else {
                        return;
                    };
                    let Some(supervisor) = console.selected_supervisor() else {
                        return;
                    };
                    let volume = volume.clone();
                    let weak_refresh = Rc::downgrade(&console);
                    console.clone().confirm_destructive(
                        "Remove volume?",
                        &format!("Volume “{volume}” and its cached contents will be deleted."),
                        "Delete",
                        move || {
                            let supervisor = supervisor.clone();
                            let volume = volume.clone();
                            let weak_refresh = weak_refresh.clone();
                            let events = console.workspace.events.clone();
                            let handle = crate::runtime::runtime().spawn(async move {
                                if let Err(e) = supervisor.remove_volume(&volume).await {
                                    events.publish(taste_core::Event::Toast(format!(
                                        "Volume removal failed: {e}"
                                    )));
                                }
                            });
                            glib::spawn_future_local(async move {
                                let _ = handle.await;
                                if let Some(console) = weak_refresh.upgrade() {
                                    console.refresh_resources();
                                }
                            });
                        },
                    );
                });
                row.append(&delete);
            }
            self.resources_list.append(&row);
        }
    }

    // --- environment lifecycle -------------------------------------------

    /// A human's environment: a clone with no chat bound to it.
    ///
    /// The container is deliberately not started — environments are lazy by
    /// policy (clone on creation, build on first need), and starting one
    /// runs its configuration's lifecycle commands, which is a decision the
    /// user makes with Start.
    /// Clone the workspace into a new environment. Public because the
    /// environment strip's popover mirrors this button: two entry points,
    /// one creation path.
    pub fn create_environment(self: &Rc<Self>, button: gtk::Button) {
        let id = match crate::environments::next_id(&self.environments) {
            Ok(id) => id,
            Err(e) => {
                self.workspace
                    .events
                    .publish(taste_core::Event::Toast(format!("{e:#}")));
                return;
            }
        };
        button.set_sensitive(false);
        let events = self.workspace.events.clone();
        let root = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        crate::environments::create(
            self.environments.clone(),
            id,
            Box::new(move |outcome| {
                button.set_sensitive(true);
                match outcome {
                    Ok(id) => {
                        events.publish(taste_core::Event::Toast(format!("Created {id}")));
                        note_created(&root, &id);
                        if let Some(console) = weak.upgrade() {
                            console.refresh_environment_data(false);
                        }
                    }
                    Err(e) => events.publish(taste_core::Event::Toast(e)),
                }
            }),
        );
    }

    /// Rename an environment: the one thing the clone directory cannot say.
    fn rename_intervention(self: &Rc<Self>, env: &EnvironmentId) {
        let current = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == *env)
            .filter(|row| row.named)
            .map(|row| row.name.clone())
            .unwrap_or_default();
        let content = self.open_intervention(&format!("Name for {env}"));
        content.append(
            &gtk::Label::builder()
                .label(
                    "The name is yours; the slug stays the identity — container \
                     names, volumes and its socket keep using it.",
                )
                .css_classes(["caption", "dim-label"])
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        let entry = gtk::Entry::builder()
            .text(&current)
            .placeholder_text(env.as_str())
            .hexpand(true)
            .build();
        let save = gtk::Button::builder()
            .label("Save")
            .css_classes(["suggested-action"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&save);
        content.append(&row);

        let weak = Rc::downgrade(self);
        let env = env.clone();
        let apply = move |entry: &gtk::Entry| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            let name = entry.text().to_string();
            let root = console.workspace.root().to_path_buf();
            let env = env.clone();
            let weak = Rc::downgrade(&console);
            // A state file is read and written: not on this thread.
            glib::spawn_future_local(async move {
                let named = env.clone();
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    let mut state = taste_core::state::load(&root);
                    state.root = root.clone();
                    state.set_environment_name(&named, Some(&name));
                    taste_core::state::save(&root, &state).map(|()| state)
                });
                let Ok(Ok(state)) = handle.await else { return };
                let Some(console) = weak.upgrade() else {
                    return;
                };
                // The console's copy of workspace state is what the fleet
                // renders from; updating it is what makes the row move.
                *console.state.borrow_mut() = state;
                console.close_intervention();
                console.refresh_fleet();
            });
        };
        {
            let apply = apply.clone();
            let entry = entry.clone();
            save.connect_clicked(move |_| apply(&entry));
        }
        entry.connect_activate(move |entry| apply(entry));
    }

    /// Destroying an environment says what it holds first.
    ///
    /// The clone can be the only copy of an agent's unreviewed work, so the
    /// enumeration happens BEFORE the confirmation is even offered — a
    /// dialog that appears instantly and a warning that arrives afterwards
    /// is how work gets thrown away.
    fn destroy_intervention(self: &Rc<Self>, env: &EnvironmentId) {
        let Some(supervisor) = self.environments.get(env) else {
            return;
        };
        let content = self.open_intervention(&format!("Destroy {env}?"));
        let summary = gtk::Label::builder()
            .label("Checking what this environment holds…")
            .css_classes(["caption"])
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        content.append(&summary);
        let button = gtk::Button::builder()
            .label("Destroy")
            .css_classes(["destructive-action"])
            .halign(gtk::Align::End)
            .sensitive(false)
            .build();
        content.append(&button);

        let repo = supervisor.root().to_path_buf();
        let main_checkout = self.workspace.root().to_path_buf();
        let chat = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == *env)
            .and_then(|row| row.chat.clone());
        // Whether the user has already ruled on this environment. It does
        // not change what is enumerated — the facts are the facts — only
        // whether they are framed as a warning or as a record.
        let settled = self.workspace.review.state(env).settled();
        let env = env.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let unpublished =
                    taste_git::unpublished_work(&repo, &main_checkout).unwrap_or_default();
                let dirty = taste_git::GitWorkspace::discover(&repo)
                    .and_then(|git| git.status().ok())
                    .map(|status| status.len())
                    .unwrap_or(0);
                (unpublished, dirty)
            });
            let Ok((unpublished, dirty)) = handle.await else {
                return;
            };
            let mut text = String::new();
            if settled {
                // The warning exists for work nobody has looked at. Once
                // the user has ruled on this environment, repeating it
                // would make the warning that DOES matter look like noise
                // — so it is stated as a fact and not as a caution.
                text.push_str(
                    "You have already ruled on this environment, so nothing here is \
                     waiting on you.\n\n",
                );
            }
            if unpublished.is_empty() && dirty == 0 {
                text.push_str(
                    "Nothing here is unpublished: everything this environment \
                     committed is already in your checkout.\n\n",
                );
            } else {
                text.push_str(if settled {
                    // The warning exists for work nobody has looked at.
                    // Once the user has ruled, the leftovers are what they
                    // already decided against — a fact, not a caution.
                    "Its clone still holds what you decided against:\n"
                } else {
                    "This environment holds work nobody else has:\n"
                });
                for branch in unpublished.iter().take(8) {
                    text.push_str(&format!(
                        "  {} — {} commit{}{} — {}\n",
                        branch.branch,
                        branch.commits,
                        if branch.commits == 1 { "" } else { "s" },
                        if branch.truncated { "+" } else { "" },
                        if branch.summary.is_empty() {
                            "(no commit message)"
                        } else {
                            &branch.summary
                        }
                    ));
                }
                if unpublished.len() > 8 {
                    text.push_str(&format!("  … and {} more\n", unpublished.len() - 8));
                }
                if dirty > 0 {
                    text.push_str(&format!(
                        "  {dirty} uncommitted file{}\n",
                        if dirty == 1 { "" } else { "s" }
                    ));
                }
                text.push('\n');
            }
            if let Some(chat) = &chat {
                text.push_str(&format!(
                    "“{}” works here; it keeps its conversation but loses the \
                     files it was working on.\n\n",
                    chat.label
                ));
            }
            text.push_str(
                "Destroying removes the clone, the container and this \
                 environment's volumes. It cannot be undone.",
            );
            summary.set_label(&text);
            button.set_sensitive(true);

            let weak_button = weak.clone();
            button.connect_clicked(move |button| {
                let Some(console) = weak_button.upgrade() else {
                    return;
                };
                button.set_sensitive(false);
                console.run_destroy(env.clone());
            });
        });
    }

    fn run_destroy(self: &Rc<Self>, env: EnvironmentId) {
        let registry = self.environments.clone();
        let events = self.workspace.events.clone();
        let root = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let target = env.clone();
            let handle = crate::runtime::runtime().spawn(async move {
                registry
                    .destroy(&target)
                    .await
                    .map_err(|e| format!("{e:#}"))
            });
            let Ok(result) = handle.await else { return };
            match result {
                Ok(report) => {
                    let mut message = format!("Destroyed {env}");
                    if !report.removed_volumes.is_empty() {
                        message.push_str(&format!(
                            " · {} volume{} freed",
                            report.removed_volumes.len(),
                            if report.removed_volumes.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ));
                    }
                    if report.had_unsaved_work() {
                        message.push_str(&format!(
                            " · {} unpublished branch(es) and {} uncommitted file(s) went with it",
                            report.unpublished.len(),
                            report.dirty_files
                        ));
                    }
                    events.publish(taste_core::Event::Toast(message));
                    forget_environment(&root, &env);
                }
                Err(e) => events.publish(taste_core::Event::Toast(format!("Destroy failed: {e}"))),
            }
            if let Some(console) = weak.upgrade() {
                console.close_intervention();
                console.git_facts.borrow_mut().remove(&env);
                console.claim_facts.borrow_mut().remove(&env);
                console.review_facts.borrow_mut().remove(&env);
                // ...and the board's own cache, so a slug that comes round
                // again does not inherit the last tenant's verdict.
                console.workspace.review.forget(&env);
                console.disk_facts.borrow_mut().remove(&env);
                console.logs.borrow_mut().remove(&env);
                if let Some(sink) = console.lifecycle.borrow_mut().remove(&env) {
                    sink.remove();
                }
                if *console.selected.borrow() == env {
                    *console.selected.borrow_mut() = EnvironmentId::primary();
                    console.show_selected_environment();
                }
                console.refresh_environment_data(false);
            }
        });
    }

    // --- the fleet's intervention panel ------------------------------------

    fn open_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
        while let Some(child) = self.intervention.first_child() {
            self.intervention.remove(&child);
        }
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_start(10);
        header.set_margin_end(6);
        header.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["caption-heading"])
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build(),
        );
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .css_classes(["flat", "circular"])
            .build();
        {
            let weak = Rc::downgrade(self);
            close.connect_clicked(move |_| {
                if let Some(console) = weak.upgrade() {
                    console.close_intervention();
                }
            });
        }
        header.append(&close);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(4);
        content.set_margin_bottom(10);
        content.set_margin_start(10);
        content.set_margin_end(10);
        self.intervention.append(&header);
        self.intervention.append(&content);
        self.intervention.set_visible(true);
        content
    }

    fn close_intervention(&self) {
        self.intervention.set_visible(false);
        while let Some(child) = self.intervention.first_child() {
            self.intervention.remove(&child);
        }
    }

    fn confirm_destructive(
        self: Rc<Self>,
        heading: &str,
        body: &str,
        affirm: &str,
        on_confirm: impl Fn() + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("confirm", affirm)]);
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(Some("confirm"), move |_, _| on_confirm());
        dialog.present(Some(&self.widget));
    }

    /// The exited-process countdown: five seconds to object, then the tab
    /// closes itself. `what` names what ended ("Shell exited", "Sign In
    /// finished") — the countdown is appended.
    fn countdown_close(self: &Rc<Self>, page: adw::TabPage, what: &str) {
        let overlay = self
            .widget
            .root()
            .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
            .and_then(|window| window.content())
            .and_downcast::<adw::ToastOverlay>();
        let Some(overlay) = overlay else {
            self.host().close_page(&page);
            return;
        };
        let toast = adw::Toast::builder()
            .title(format!("{what} — closing this terminal in 5 s"))
            .button_label("Keep Open")
            .timeout(0)
            .build();
        let keep = Rc::new(Cell::new(false));
        {
            let keep = keep.clone();
            toast.connect_button_clicked(move |toast| {
                keep.set(true);
                toast.dismiss();
            });
        }
        overlay.add_toast(toast.clone());
        let what = what.to_string();
        let remaining = Cell::new(5i32);
        let tabs = self.host();
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            if keep.get() {
                return glib::ControlFlow::Break;
            }
            let left = remaining.get() - 1;
            remaining.set(left);
            if left <= 0 {
                toast.dismiss();
                tabs.close_page(&page);
                return glib::ControlFlow::Break;
            }
            toast.set_title(&format!("{what} — closing this terminal in {left} s"));
            glib::ControlFlow::Continue
        });
    }

    /// Mark a terminal tab EXITED, in place, until the user closes it by
    /// hand.
    ///
    /// This is what replaced the five-second auto-close for terminals.
    /// `countdown_close` above is still what a *command* tab uses — sign-in
    /// has a natural end and nothing further to show once it succeeds — but
    /// a terminal's output is not that: it is the record of what happened,
    /// and closing on exit throws it away. So the tab sits there, its title
    /// and its indicator saying it is done, exactly as long as the user
    /// wants to keep reading it.
    fn mark_tab_exited(page: &adw::TabPage, what: &str) {
        let title = page.title();
        if !title.ends_with(" (exited)") {
            page.set_title(&format!("{title} (exited)"));
        }
        // Overwrites the ownership badge an agent's tab wore: a dead
        // command has no owner left to mark, and "it stopped" is the more
        // useful of the two facts once both are true.
        page.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(
            "media-playback-stop-symbolic",
        )));
        page.set_indicator_tooltip(&format!(
            "{what} — the output stays until you close this tab"
        ));
    }

    /// Live badge for the Services tab: count, failures called out.
    pub fn update_service_summary(&self, total: usize, failed: usize) {
        self.services_page.set_title(&if failed > 0 {
            format!("Services · {total} · {failed} failed")
        } else {
            format!("Services · {total}")
        });
        self.services_page.set_needs_attention(failed > 0);
        self.services_page
            .set_icon(Some(&gtk::gio::ThemedIcon::new(if failed > 0 {
                "taste-services-off"
            } else {
                "taste-services-on"
            })));
    }

    /// Services can't be listed: yellow when the container runs without
    /// systemd, neutral gray when there is no container to ask. Red stays
    /// reserved for actual failures.
    pub fn set_services_unavailable(&self, systemd_missing: bool) {
        self.services_page.set_title(if systemd_missing {
            "Services · no systemd"
        } else {
            "Services"
        });
        self.services_page
            .set_icon(Some(&gtk::gio::ThemedIcon::new(if systemd_missing {
                "taste-services-warn"
            } else {
                "taste-services-none"
            })));
        self.services_page.set_needs_attention(false);
    }

    /// Bring the environment tab — which is where the log lives — to the
    /// front for one environment (the safe-mode banner's "View Log" lands
    /// here).
    pub fn show_devcontainer_log(self: &Rc<Self>, env: &EnvironmentId) {
        self.note_watching(env);
        self.show_section(SECTIONS[0]);
    }

    /// Append one environment's build/startup output — to its own log
    /// buffer, and to its lifecycle roster row.
    pub fn append_env_log(self: &Rc<Self>, env: &EnvironmentId, line: &str) {
        self.lifecycle_sink(env)
            .push(format!("{line}\n").as_bytes());
        let buffer = self.log_buffer(env);
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, line);
        buffer.insert(&mut end, "\n");
        if *self.selected.borrow() == *env {
            self.scroll_log_to_end();
        }
    }

    fn scroll_log_to_end(&self) {
        if !self.follow_log.is_active() {
            return;
        }
        if let Some(scroller) = self
            .supervisor_log
            .parent()
            .and_downcast::<gtk::ScrolledWindow>()
        {
            let adjustment = scroller.vadjustment();
            glib::idle_add_local_once(move || adjustment.set_value(adjustment.upper()));
        }
    }

    /// Append a Flatpak build/install log line, creating the pinned
    /// "Flatpak" tab on first use.
    pub fn append_flatpak_log(&self, line: &str) {
        if self.flatpak_log.borrow().is_none() {
            let view = gtk::TextView::builder()
                .editable(false)
                .monospace(true)
                .wrap_mode(gtk::WrapMode::WordChar)
                .build();
            let scroller = gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .build();
            // NOT pinned: pinned pages are forced to the left edge of
            // whatever view holds them, and at the consolidated rung that
            // view is the editor's — a Flatpak log jumping in front of the
            // user's open files is the nested-strip problem in another
            // costume. It is a tab like any other, and closable.
            let page = self.host().append(&scroller);
            page.set_title("Flatpak");
            page.set_icon(Some(&gtk::gio::ThemedIcon::new("folder-download-symbolic")));
            self.host().set_selected_page(&page);
            *self.flatpak_log.borrow_mut() = Some(view);
        }
        if let Some(view) = self.flatpak_log.borrow().as_ref() {
            let buffer = view.buffer();
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, line);
            buffer.insert(&mut end, "\n");
        }
    }

    /// Open a tab running one specific command (login TUIs and the like)
    /// in the current execution context. A command that SUCCEEDS has
    /// nothing left to read, so its tab retires itself (same five-second
    /// grace as an exited shell); a failure leaves its output up.
    pub fn add_command_tab(
        self: &Rc<Self>,
        title: &str,
        program: &str,
        args: &[String],
        env: &[(String, String)],
        wrapped: bool,
    ) {
        // Pre-wrapped commands (the agent sign-in) already carry their own
        // execution context; resolving them into the devcontainer would
        // run them in the wrong universe.
        let spec = if wrapped {
            taste_core::exec::CommandSpec {
                program: program.to_string(),
                args: args.to_vec(),
            }
        } else {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            self.workspace.exec.resolve(program, &arg_refs, true)
        };
        let root = self.workspace.root().to_path_buf();
        let (terminal, page) = self.spawn_tab(title, "system-run-symbolic", spec, env, &root);
        // Command tabs have a natural end: announce it and let interested
        // panes react (the sign-in flow keys off this).
        let events = self.workspace.events.clone();
        let title = title.to_string();
        let weak = Rc::downgrade(self);
        terminal.connect_child_exited(move |_, status| {
            events.publish(taste_core::Event::CommandTabExited {
                title: title.clone(),
                status,
            });
            if status != 0 {
                // Left open on purpose: the failure IS the output.
                events.publish(taste_core::Event::Toast(format!(
                    "{title} exited with status {status}"
                )));
                return;
            }
            // Signed in (or whatever else finished): the console has served
            // its purpose. The countdown toast doubles as the "finished"
            // notice, so it replaces the plain one.
            if let Some(console) = weak.upgrade() {
                console.countdown_close(page.clone(), &format!("{title} finished"));
            }
        });
    }

    /// Where a new terminal runs: the selected environment when it has a
    /// container of its own, else the workspace's own context.
    ///
    /// The fallback is not a nicety. A non-primary environment with no
    /// container resolves to the HOST, and a shell there would open on the
    /// user's checkout while claiming to be that environment's — an
    /// attribution lie in the roster, and the wrong files under the cursor.
    fn terminal_target(&self) -> (EnvironmentId, taste_core::ExecContext, PathBuf) {
        let selected = self.selected.borrow().clone();
        if !selected.is_primary() {
            if let Some(supervisor) = self.environments.get(&selected) {
                // "Has a container", not "is in container mode": a baseline
                // container is a real place with that environment's own
                // files in it, and a shell there is honestly labelled. The
                // fallback below exists for having *nowhere*, which is the
                // case that would resolve to the host.
                if supervisor.exec().has_exec_target() {
                    return (
                        selected,
                        supervisor.exec().clone(),
                        supervisor.root().to_path_buf(),
                    );
                }
            }
        }
        (
            EnvironmentId::primary(),
            self.workspace.exec.clone(),
            self.workspace.root().to_path_buf(),
        )
    }

    /// Open a shell tab in the selected environment's execution context.
    ///
    /// It registers in that environment's shell roster: the user's own
    /// terminals are part of what an environment is running, and the fleet
    /// says so. Interactive, and deliberately **not** killable from the
    /// roster — it is the user's, and closing its tab is how it ends.
    pub fn add_terminal_tab(self: &Rc<Self>) {
        let (env, exec, cwd) = self.terminal_target();
        let spec = exec.resolve("/bin/bash", &[], true);
        // Name the shell by where it REALLY runs — "host" was ambiguous
        // when the IDE itself lives in a container.
        let in_devcontainer = exec.container_id().is_some();
        // Non-devcontainer shells carry a red warning badge: they run on
        // the host (or the IDE's own barely-confined container), outside
        // the environment work is supposed to happen in.
        let (title, icon) = if in_devcontainer {
            (
                if env.is_primary() {
                    "devcontainer".to_string()
                } else {
                    env.to_string()
                },
                "package-x-generic-symbolic",
            )
        } else if exec.is_inside_container() {
            // Self-hosting bootstrap: the IDE's own container IS the
            // project's devcontainer (container mode by construction), so
            // its shells are confined — no warning. Warn only when the
            // surrounding container is not the devcontainer (safe mode).
            if exec.is_container() {
                ("IDE container".to_string(), "package-x-generic-symbolic")
            } else {
                ("IDE container".to_string(), "taste-container-warn")
            }
        } else {
            ("this machine".to_string(), "taste-host-warn")
        };
        let (terminal, page) = self.spawn_tab(&title, icon, spec, &[], &cwd);
        let sink = self
            .workspace
            .shells
            .register(env.clone(), ShellKind::User, "bash", None);
        self.shell_tabs
            .borrow_mut()
            .insert(sink.id(), (env.clone(), page.clone()));
        // Retitle as user@host, asked of the shell's own execution context
        // (the placeholder above stands until the probe answers).
        {
            let probe = exec.resolve(
                "sh",
                // uname -n, not hostname: minimal images lack the latter
                // (it probed as "dev@").
                &["-c", "printf '%s@%s' \"$(id -un)\" \"$(uname -n)\""],
                false,
            );
            let page_for_title = page.clone();
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    std::process::Command::new(&probe.program)
                        .args(&probe.args)
                        .output()
                });
                let Ok(Ok(output)) = handle.await else { return };
                if !output.status.success() {
                    return;
                }
                let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
                // Both halves or nothing: "dev@" helps nobody.
                let complete = title
                    .split_once('@')
                    .is_some_and(|(u, h)| !u.is_empty() && !h.is_empty());
                if complete {
                    page_for_title.set_title(&title);
                }
            });
        }
        // A shell that exits KEEPS its tab: the output is the record of
        // what happened, and the user closes it by hand when they are done
        // reading it (`Self::mark_tab_exited`). It used to close itself
        // after a five-second countdown toast, which threw that record away
        // by default and made cancelling the normal case.
        //
        // No ownership indicator on this one, deliberately: it is the
        // user's own terminal, which is the default assumption for any tab
        // in this strip. The exception worth badging is a tab that is NOT
        // theirs (`add_shell_tab`), the same asymmetry the retired roster's
        // "yours" tag drew from the other side.
        //
        // The page handle comes straight from spawn_tab: walking widget
        // parents into TabView internals made tabs.page() panic inside a
        // GTK callback — a non-unwinding abort on host runs.
        {
            let page = page.clone();
            let sink = sink.clone();
            terminal.connect_child_exited(move |_, status| {
                sink.finish(taste_core::ShellState::Exited {
                    code: Some(status),
                    signal: None,
                });
                Self::mark_tab_exited(&page, "Shell exited");
            });
        }
        // Closing the tab is what ends it; the close handler above takes
        // the roster entry with it.
        if !in_devcontainer && !exec.is_inside_container() {
            self.host_shells.borrow_mut().push(page);
        }
    }

    /// Open tabs for shells this console has not seen yet.
    ///
    /// Driven by `Event::ShellRosterChanged`, which is deliberately coarse
    /// — it says "look again", not what changed. Output never travels on
    /// the bus; each tab subscribes to its own shell and pumps from there.
    ///
    /// Only the agent's shells get tabs. The user's own terminals are
    /// already tabs (this console spawned them), and the lifecycle stream
    /// is the environment tab's log.
    ///
    /// There is no roster list to refresh any more — the tabs themselves
    /// are the listing, and each keeps its own status current as its
    /// updates arrive (see `add_shell_tab`) — so the environment that
    /// changed only matters to `sync_shell_tabs`, which reads the selected
    /// one off `self.selected` itself.
    pub fn sync_shell_roster(self: &Rc<Self>, _env: &EnvironmentId) {
        self.sync_shell_tabs();
    }

    /// Make the shell tabs on screen be the selected environment's, and
    /// only those.
    ///
    /// A tab showing a command running in another environment is that
    /// environment's resource sitting in this environment's pane — the
    /// thing this whole pass is about. But it is **stowed, never closed**:
    /// a shell tab holds a live VTE, and one of them is the user's own
    /// interactive terminal. Closing that because they looked at another
    /// environment would kill a running command and throw away its
    /// scrollback — the pane's tidiness is not worth the user's work. The
    /// pages move to an unparented `AdwTabView` per environment, the same
    /// way the editor stows its tabs, and come back untouched.
    fn sync_shell_tabs(self: &Rc<Self>) {
        let env = self.selected.borrow().clone();
        // What is on screen right now. `AdwTabPage` cannot be asked which
        // view holds it, so the view is asked instead — and it is the only
        // authority worth trusting here anyway.
        let host = self.host();
        let on_screen: Vec<adw::TabPage> = (0..host.n_pages())
            .map(|index| host.nth_page(index))
            .collect();
        // Out: everything on screen that is not this environment's.
        let leaving: Vec<(EnvironmentId, adw::TabPage)> = self
            .shell_tabs
            .borrow()
            .values()
            .filter(|(owner, page)| *owner != env && on_screen.contains(page))
            .cloned()
            .collect();
        for (owner, page) in leaving {
            let holding = self.holding_shell_view(&owner);
            host.transfer_page(&page, &holding, holding.n_pages());
            self.stowed_shells.borrow_mut().insert(owner, holding);
        }
        // Back in: everything this environment had stowed, in the order it
        // was stowed in.
        if let Some(holding) = self.stowed_shells.borrow_mut().remove(&env) {
            while holding.n_pages() > 0 {
                let page = holding.nth_page(0);
                holding.transfer_page(&page, &host, host.n_pages());
            }
        }
        // ...and any of the agent's shells here that have no tab at all yet.
        for entry in self.workspace.shells.list(Some(&env)) {
            if self.shell_tabs.borrow().contains_key(&entry.id) {
                continue;
            }
            if matches!(entry.kind, ShellKind::Agent | ShellKind::ExecJob) {
                self.add_shell_tab(&entry);
            }
        }
    }

    /// The unparented view holding one environment's stowed shell tabs.
    fn holding_shell_view(&self, env: &EnvironmentId) -> adw::TabView {
        if let Some(view) = self.stowed_shells.borrow().get(env) {
            return view.clone();
        }
        adw::TabView::new()
    }

    /// A live, read-only view of one shell the agent is running: the
    /// command as its title, its output as it arrives, and a Kill button.
    ///
    /// **Read-only VTE, not a TextView.** Build output is ANSI — colours,
    /// carriage-return progress bars, cursor moves — and a TextView shows
    /// the escape codes instead of obeying them. VTE without a pty renders
    /// exactly what it is fed and has nowhere to send keystrokes, which is
    /// the read-only part. The user's own terminals in this console are
    /// already VTE, so an agent's tab looks like a terminal because it is
    /// one.
    ///
    /// **Kill has no confirmation, deliberately.** It is supervision, not
    /// destruction: nothing is lost that a re-run cannot produce again, the
    /// output stays on screen afterwards, and the agent is told its command
    /// died. A confirmation here would be a dialog whose answer is always
    /// yes — the click-through training this project spends its consent
    /// prompts avoiding, so they still mean something where they guard
    /// something irreversible (`devcontainer_reload`, a force-publish).
    fn add_shell_tab(self: &Rc<Self>, entry: &taste_core::ShellEntry) {
        let Some((backlog, updates)) = self.workspace.shells.watch(entry.id) else {
            // Registered and gone again before we looked: nothing to show.
            return;
        };

        let title = gtk::Label::builder()
            .label(entry.label())
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build();
        let status = gtk::Label::builder()
            .label(entry.state.summary())
            .css_classes(["dim-label", "caption"])
            .build();
        let kill = gtk::Button::builder()
            .label("Kill")
            .tooltip_text(if entry.killable {
                "Stop this command. The output stays; the agent is told it died."
            } else {
                // Agent-owned terminals (what the pinned Claude Code adapter
                // reports) run inside the adapter's own process: there is no
                // child of ours to signal and no ACP request to ask for one.
                // Say so, rather than leaving a dead button to be puzzled at.
                "This command runs inside the agent itself, so the IDE cannot \
                 stop it — cancel the turn instead."
            })
            .css_classes(["destructive-action"])
            .valign(gtk::Align::Center)
            .sensitive(entry.killable)
            .build();
        {
            let shells = self.workspace.shells.clone();
            let id = entry.id;
            kill.connect_clicked(move |button| {
                // Insensitive immediately: the process takes a moment to
                // die, and a button that still invites clicking reads as
                // one that did nothing.
                button.set_sensitive(false);
                shells.kill(id);
            });
        }
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(12);
        header.set_margin_end(12);
        header.append(&title);
        header.append(&status);
        header.append(&kill);

        let terminal = self.read_only_terminal();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&terminal)
            .vexpand(true)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&scroller);

        let page = self.host().append(&content);
        page.set_title(&entry.label());
        page.set_icon(Some(&gtk::gio::ThemedIcon::new(match entry.kind {
            ShellKind::ExecJob => "system-run-symbolic",
            _ => "utilities-terminal-symbolic",
        })));
        // Ownership, as an indicator badge — the asymmetric half of a pair
        // with the user's own terminals (`add_terminal_tab`), which carry
        // no such badge because being the user's own is the default
        // assumption for a tab in this strip. It is the fact the retired
        // roster's "yours" tag used to carry, said from the side that is
        // the exception. `mark_tab_exited` overwrites it once the command
        // ends.
        page.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new("system-users-symbolic")));
        page.set_indicator_tooltip(
            "The agent's terminal — read only. Kill stops the command; closing the \
             tab just puts it away.",
        );
        self.shell_tabs
            .borrow_mut()
            .insert(entry.id, (entry.env.clone(), page.clone()));
        // Agent work must not steal the tab the user is reading. Unlike a
        // terminal the user asked for, nobody asked for this one.
        if self.host().selected_page().is_none() {
            self.host().set_selected_page(&page);
        }
        // Already over by the time the tab caught up — a shell registered
        // and finished between two passes of `sync_shell_tabs`.
        if !entry.state.is_running() {
            Self::mark_tab_exited(&page, "Command exited");
        }

        feed(&terminal, backlog.as_bytes());
        let page = page.clone();
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    taste_core::ShellUpdate::Output(bytes) => feed(&terminal, &bytes),
                    taste_core::ShellUpdate::State(state) => {
                        status.set_label(&state.summary());
                        kill.set_sensitive(false);
                        if !state.is_running() {
                            Self::mark_tab_exited(&page, "Command exited");
                        }
                    }
                }
            }
        });
    }

    /// TASTE_PROBE_CHECK only: stand in an agent terminal so the probe can
    /// SEE this tab.
    ///
    /// It has no other way to exist in a probe — a real one needs a
    /// relocated agent asking for one, which needs a container, an
    /// environment and a conversation. The roster is the console's only
    /// input here, so seeding it exercises the whole rendering path
    /// (watch, backlog replay, feed, header, Kill) rather than a mock of
    /// it. Same trick as the chat pane's seeded transcript.
    ///
    /// `exited`, when true, finishes the shell right after seeding it —
    /// through the SAME path a real exit takes (`ShellUpdate::State` →
    /// `mark_tab_exited` in `add_shell_tab`), so a frame that wants to show
    /// a terminal tab marked exited-with-output gets the genuine rendering
    /// rather than a hand-posed stand-in.
    pub fn seed_agent_terminal_for_probe(self: &Rc<Self>, env: &EnvironmentId, exited: bool) {
        let sink = self.workspace.shells.register(
            env.clone(),
            ShellKind::Agent,
            "cargo test --workspace",
            // Killable, so the button renders enabled: what the probe is
            // for is seeing the control, not pressing it.
            Some(std::sync::Arc::new(|| {})),
        );
        sink.push(
            b"   Compiling taste-core v0.1.0 (/workspaces/taste-ide/crates/taste-core)\n\
              \x1b[32m    Finished\x1b[0m `test` profile [unoptimized + debuginfo] target(s)\n\
              \x1b[32mtest\x1b[0m shells::tests::a_registered_shell_is_listed_for_its_environment_only ... ok\n\
              \x1b[32mtest\x1b[0m terminal::tests::create_output_exit_and_release ... ok\n",
        );
        if exited {
            sink.finish(taste_core::ShellState::Exited {
                code: Some(0),
                signal: None,
            });
        }
        self.sync_shell_roster(env);
    }

    /// TASTE_PROBE_CHECK only: put a build in the environment tab's log.
    ///
    /// Nothing has ever been built in a probe, so the log is honestly
    /// empty — and it is no longer one page of a switcher a shot could
    /// point somewhere else. It is the bottom two thirds of the tab that
    /// every console frame now shows, and a frame of an empty box says
    /// nothing about the thing it is a frame of. Fed through
    /// `append_env_log`, so what the shot catches is the real buffer, the
    /// real per-environment routing and the real tail behaviour.
    pub fn seed_log_for_probe(self: &Rc<Self>, env: &EnvironmentId) {
        // The container's name is derived, not typed: this log goes into
        // whichever environment the view is aimed at, and a fixture that
        // said `taste-ide-calm-1` under a frame captioned `wry-4` is the
        // shot contradicting itself.
        let container = format!("taste-ide-{env}");
        for line in [
            "[1/6] Reading .devcontainer/devcontainer.json".to_string(),
            "[2/6] Image ghcr.io/taste-ide/rust-gtk:1.84 is up to date".to_string(),
            format!("[3/6] Creating container {container}"),
            "[4/6] onCreateCommand: cargo fetch --locked".to_string(),
            "        Fetching 214 crates from crates.io".to_string(),
            "[5/6] postCreateCommand: build-aux/devcontainer-setup.sh".to_string(),
            "        gtk4 4.20.1, libadwaita 1.8.0, vte 0.80.3".to_string(),
            "[6/6] Container ready in 41.2s".to_string(),
        ] {
            self.append_env_log(env, &line);
        }
    }

    /// TASTE_PROBE_CHECK only: fabricate a fleet with more than one
    /// environment in it.
    ///
    /// Cloning real repositories headlessly would work and would take
    /// minutes; what a screenshot has to show is the rendering, and the
    /// rendering's only input is [`EnvFacts`]. So the facts are the seam,
    /// the same way the roster is for a terminal.
    /// TASTE_PROBE_CHECK only: a queue with something on it, for the
    /// backlog's screenshot.
    ///
    /// A probe instance has an empty issues ref — nothing has ever been
    /// filed in a workspace that exists for two seconds — so without this
    /// every shot would show the honest empty state. What is fabricated is
    /// the *issues*; the ordering, the claim lookup against the real fleet
    /// and the action columns are the genuine ones, which is why the
    /// fixture claims environments the fleet seed actually contains.
    pub fn seed_issues_for_probe(self: &Rc<Self>) {
        self.probe_issues.set(true);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let issue =
            |id: &str, title: &str, resolution, assignee: Option<&str>, age: i64, body: &str| {
                taste_git::Issue {
                    id: id.into(),
                    title: title.into(),
                    resolution,
                    reporter: "primary".into(),
                    assignee: assignee.map(str::to_string),
                    created: now - age,
                    updated: now - age / 2,
                    labels: Vec::new(),
                    links: Vec::new(),
                    body: body.into(),
                    comments: Vec::new(),
                }
            };
        // In the order the `order` file would put them: what the user
        // wants next is at the top, and it is NOT the lowest id — a
        // screenshot of a backlog that happened to be in id order would
        // not show that the order is authored.
        //
        // All four states are here, because the leading glyph is now the
        // whole of what a row says and a shot that showed two of them
        // would be a shot of half the vocabulary: two Active (claimed by
        // environments the fleet fixture really contains), one Queued, one
        // Completed, one Declined.
        *self.issues.borrow_mut() = vec![
            issue(
                "i-0007",
                "The composer loses a half-typed follow-up on switch",
                taste_git::Resolution::Open,
                Some("calm-1"),
                9_000,
                "Type into the prompt box, switch environments, come back: the text \
                 is gone. The pane is never destroyed, so the buffer should still be \
                 there.",
            ),
            issue(
                "i-0002",
                "Decide what a stopped environment costs",
                taste_git::Resolution::Open,
                // Held by the environment that flagged itself for review:
                // the two fixtures have to agree, or the backlog row and
                // the fleet row contradict each other in one frame.
                Some("wry-4"),
                52_000,
                "Idle-stop keeps the clone and the volumes, so the footprint does not \
                 move when a container stops — worth saying so on the row.",
            ),
            issue(
                "i-0009",
                "Sparklines should survive a fleet rebuild",
                taste_git::Resolution::Open,
                None,
                4_000,
                "The panel rebuilds its list when the entries change, and each rebuild \
                 starts every sparkline empty for up to a second.",
            ),
            issue(
                "i-0004",
                "Terminal tabs should keep their output after the process exits",
                taste_git::Resolution::Completed,
                Some("spry-2"),
                260_000,
                "Closing on exit throws away the record of what happened.",
            ),
            // Declined, with the decision on its trail — which is the
            // whole difference between declining and deleting, and the
            // thing the state glyph's tooltip reads back.
            {
                let mut declined = issue(
                    "i-0011",
                    "Add a per-project settings file",
                    taste_git::Resolution::Declined,
                    None,
                    180_000,
                    "One file per project for the things the IDE currently decides.",
                );
                declined.comments = vec![taste_git::Comment {
                    seq: 1,
                    author: "primary".into(),
                    created: now - 90_000,
                    body: "Declined: convention over configuration — this is the \
                           extension point the architecture refuses."
                        .into(),
                }];
                declined
            },
        ];
        self.announce_issues();
        self.announce_fleet();
    }

    /// `keep` bounds how many fabricated environments are seeded.
    ///
    /// The list scrolls past `max_content_height`, and a pane edge through
    /// the middle of a row reads as clipping rather than as more list — so a
    /// shot that is not *about* the fleet asks for a number of rows that
    /// fits, and the fleet's own shot asks for all of them.
    pub fn seed_fleet_for_probe(self: &Rc<Self>, keep: usize) {
        // The third tuple field is the orchestrator marker: exactly one
        // row can carry it, and the shot is there to check that it reads
        // as a role rather than as a status beside the busy spinner.
        // Disk is per environment on purpose: four rows carrying one
        // identical number is how a fabricated fleet gives itself away, and
        // the footprint really does diverge once each clone has built.
        let claim = |id: &str, title: &str| taste_git::Claim {
            id: id.into(),
            title: title.into(),
        };
        let make = |slug: &str,
                    state,
                    chat: Option<(&str, bool, bool, bool)>,
                    git,
                    disk_mib: (u64, u64),
                    spend,
                    shells,
                    review,
                    working_on| EnvFacts {
            env: EnvironmentId::parse(slug).expect("valid probe slug"),
            state,
            authority: taste_core::ConfigAuthority::Project,
            pending_rebuild: false,
            chat: chat.map(|(label, busy, awaits_user, orchestrator)| ChatBinding {
                label: label.to_string(),
                busy,
                awaits_user,
                orchestrator,
            }),
            git: Some(git),
            review,
            working_on,
            disk: Some(taste_devcontainer::DiskUsage {
                checkout_bytes: 1024 * 1024 * disk_mib.0,
                volume_bytes: 1024 * 1024 * disk_mib.1,
                volumes_measured: 2,
                volumes_unmeasured: 0,
            }),
            spend,
            shells,
        };
        // One of every state a fleet is actually found in: the orchestrator
        // working, a worker mid-build, a worker with work waiting for review,
        // and one stopped by the idle sweep. Safe mode is not a fourth state —
        // it is what every non-running row already says it is in.
        *self.probe_rows.borrow_mut() = vec![
            make(
                "calm-1",
                SupervisorState::Running {
                    container_id: "9f2c1a".into(),
                },
                Some(("Orchestrator", true, false, true)),
                EnvGit {
                    branch: Some("topic/composer-buffer".into()),
                    unpublished: 2,
                    dirty: 4,
                },
                (412, 1600),
                fleet::Spend {
                    requests: 37,
                    input_tokens: 412_000,
                    output_tokens: 21_400,
                },
                3,
                taste_core::ReviewState::Working,
                vec![claim(
                    "i-0007",
                    "The composer loses a half-typed follow-up on switch",
                )],
            ),
            make(
                "brisk-3",
                SupervisorState::Building,
                Some(("Varlink service", false, false, false)),
                EnvGit {
                    // A clone's own working branch. The branch of record
                    // is `agents/brisk-3` and is not what a clone has
                    // checked out — the fixture has to keep those apart,
                    // or the screenshot teaches the wrong model.
                    branch: Some("topic/fleet-varlink".into()),
                    unpublished: 0,
                    dirty: 0,
                },
                (401, 96),
                fleet::Spend {
                    requests: 6,
                    input_tokens: 41_200,
                    output_tokens: 2_800,
                },
                0,
                taste_core::ReviewState::Working,
                Vec::new(),
            ),
            // Done, and waiting on the user. Its container is stopped
            // because flagging stops it — which is why the row's light is
            // red and its rail is accent, and why the shot has to show
            // both at once.
            make(
                "wry-4",
                SupervisorState::Stopped,
                Some(("Disk accounting", false, false, false)),
                EnvGit {
                    branch: Some("topic/disk-footprint".into()),
                    unpublished: 0,
                    dirty: 0,
                },
                (395, 1180),
                fleet::Spend {
                    requests: 14,
                    input_tokens: 96_500,
                    output_tokens: 5_100,
                },
                0,
                taste_core::ReviewState::FlaggedForReview,
                vec![claim("i-0002", "Decide what a stopped environment costs")],
            ),
            make(
                "spry-2",
                SupervisorState::Running {
                    container_id: "3e7b04".into(),
                },
                Some(("Terminal roster", true, true, false)),
                EnvGit {
                    branch: Some("topic/keep-output".into()),
                    unpublished: 1,
                    dirty: 2,
                },
                (398, 2140),
                fleet::Spend {
                    requests: 22,
                    input_tokens: 188_400,
                    output_tokens: 9_600,
                },
                2,
                taste_core::ReviewState::Working,
                Vec::new(),
            ),
        ];
        // The order here is truncation order, not display order (the rows
        // sort by name): keeping the first three keeps one environment in
        // each state a fleet is actually found in — running, building, and
        // stopped — so a shot that cannot fit all of them still shows what
        // the states look like side by side.
        self.probe_rows.borrow_mut().truncate(keep);
        // Branches of record, one per environment — the shape the model
        // actually has. The dead `agents/<env>/<topic>` generation is not
        // in the fixture, because a screenshot of it would teach a naming
        // scheme nothing writes any more.
        *self.published.borrow_mut() = vec!["agents/calm-1".into(), "agents/wry-4".into()];
        // calm-1's configuration drifted under its running container.
        //
        // This is a fixture FIX, not a new pose: the seeded transcript in
        // every one of these frames is the agent asking "Rebuild calm-1
        // from the changed devcontainer.json? The config on disk differs
        // from the container that is running" — while the fleet said
        // nothing had drifted anywhere. Two halves of one frame
        // contradicting each other. It is also the honest way to
        // photograph the environment tab's indicator badge and its warn
        // icon, which have no other cause.
        for row in self.probe_rows.borrow_mut().iter_mut() {
            row.pending_rebuild = row.env.as_str() == "calm-1";
        }
        // What the review band knows about the flagged one. A probe has no
        // branches to walk, so the mergedness is fabricated — and it is
        // the honest interesting case: published, ahead, and not yet in.
        if let Ok(env) = EnvironmentId::parse("wry-4") {
            self.review_facts.borrow_mut().insert(
                env,
                ReviewFacts {
                    branch: "agents/wry-4".into(),
                    target: "main".into(),
                    mergedness: Some(taste_git::Mergedness {
                        branch: "agents/wry-4".into(),
                        checked: None,
                        ahead: 6,
                        merged: false,
                        note: None,
                    }),
                },
            );
        }
        self.refresh_fleet();
    }

    /// A subscription pool for the probe, since a screenshot has no account.
    ///
    /// The numbers are chosen to exercise the shapes that are easy to get
    /// wrong rather than to look comfortable: a session window past the
    /// warning threshold, a weekly window behind it (so the gauge has to
    /// pick the right one to show), a reset far enough out to read as
    /// hours, and an observation four minutes old — because "as of" is
    /// the part of this display that must never quietly disappear.
    pub fn seed_quota_for_probe(self: &Rc<Self>) {
        use taste_core::quota::{PlanWindow, Window};
        let now = std::time::SystemTime::now();
        *self.probe_quota.borrow_mut() = Some(QuotaSnapshot {
            observed_at: Some(now - std::time::Duration::from_secs(4 * 60)),
            observed_for: Some("calm-1".into()),
            session: PlanWindow {
                label: Some("unified-5h".into()),
                utilization: Some(0.68),
                window: Window {
                    reset: Some(now + std::time::Duration::from_secs(80 * 60)),
                    ..Default::default()
                },
                status: Some("allowed".into()),
            },
            weekly: PlanWindow {
                label: Some("unified-7d".into()),
                utilization: Some(0.41),
                window: Window {
                    reset: Some(now + std::time::Duration::from_secs(3 * 86_400 + 5 * 3600)),
                    ..Default::default()
                },
                status: None,
            },
            requests: Window {
                limit: Some(1_000),
                remaining: Some(986),
                reset: Some(now + std::time::Duration::from_secs(41)),
            },
            input_tokens: Window {
                limit: Some(2_000_000),
                remaining: Some(1_610_000),
                reset: Some(now + std::time::Duration::from_secs(38)),
            },
            ..Default::default()
        });
        self.refresh_pool();
    }

    /// A VTE with the console's theming and no pty: it renders what it is
    /// fed and has nowhere to send input.
    fn read_only_terminal(&self) -> vte4::Terminal {
        let terminal = vte4::Terminal::new();
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);
        terminal.set_bold_is_bright(true);
        terminal.set_scrollback_lines(10_000);
        terminal.set_input_enabled(false);
        apply_terminal_theme(&terminal);
        adw::StyleManager::default().connect_dark_notify(glib::clone!(
            #[weak]
            terminal,
            move |_| apply_terminal_theme(&terminal)
        ));
        terminal
    }

    fn spawn_tab(
        &self,
        title: &str,
        icon: &str,
        spec: taste_core::CommandSpec,
        extra_env: &[(String, String)],
        cwd: &Path,
    ) -> (vte4::Terminal, adw::TabPage) {
        let terminal = vte4::Terminal::new();
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);
        terminal.set_bold_is_bright(true);
        terminal.set_scrollback_lines(10_000);
        // VTE doesn't follow GTK theming by itself: apply light/dark colors
        // now and re-apply whenever the desktop mode flips.
        apply_terminal_theme(&terminal);
        adw::StyleManager::default().connect_dark_notify(glib::clone!(
            #[weak]
            terminal,
            move |_| apply_terminal_theme(&terminal)
        ));

        // Plain-text URLs (sign-in flows print them) become Ctrl+clickable,
        // GNOME Console style.
        const PCRE2_MULTILINE: u32 = 0x0000_0400;
        if let Ok(regex) = vte4::Regex::for_match(
            r"https?://[-a-zA-Z0-9@:%._+~#=]{1,256}\.[a-zA-Z0-9()]{1,8}\b[-a-zA-Z0-9()@:%_+.~#?&/=]*",
            PCRE2_MULTILINE,
        ) {
            terminal.match_add_regex(&regex, 0);
        }
        let click = gtk::GestureClick::new();
        click.set_button(1);
        {
            let terminal = terminal.clone();
            let events = self.workspace.events.clone();
            click.connect_pressed(move |gesture, _, x, y| {
                if !gesture
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    return;
                }
                let (matched, _) = terminal.check_match_at(x, y);
                if let Some(url) = matched {
                    events.publish(taste_core::Event::OpenUrlRequested(url.to_string()));
                }
            });
        }
        terminal.add_controller(click);

        // VTE ships no clipboard bindings: GNOME convention is
        // Ctrl+Shift+C / Ctrl+Shift+V (plain Ctrl+C/V belong to the shell).
        let key = gtk::EventControllerKey::new();
        // CAPTURE phase: VTE consumes keys itself at bubble time, so a
        // default-phase controller never sees Ctrl+Shift+V at all.
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let terminal = terminal.clone();
            key.connect_key_pressed(move |_, keyval, _, state| {
                let ctrl_shift = state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    && state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                if !ctrl_shift {
                    return glib::Propagation::Proceed;
                }
                match keyval {
                    gtk::gdk::Key::C | gtk::gdk::Key::c => {
                        terminal.copy_clipboard_format(vte4::Format::Text);
                        glib::Propagation::Stop
                    }
                    gtk::gdk::Key::V | gtk::gdk::Key::v => {
                        terminal.paste_clipboard();
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
        }
        terminal.add_controller(key);

        // Right-click: the standard terminal context menu.
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::builder()
            .child(&menu_box)
            .has_arrow(false)
            .build();
        popover.set_parent(&terminal);
        // Link items act on the URL under the pointer; disabled (never
        // hidden) when the click wasn't on one.
        let hovered_url: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let open_link_item = gtk::Button::builder()
            .label("Open Link")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let copy_link_item = gtk::Button::builder()
            .label("Copy Link")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let copy_item = gtk::Button::builder()
            .label("Copy")
            .css_classes(["flat"])
            .build();
        let paste_item = gtk::Button::builder()
            .label("Paste")
            .css_classes(["flat"])
            .build();
        let select_item = gtk::Button::builder()
            .label("Select All")
            .css_classes(["flat"])
            .build();
        for item in [
            &open_link_item,
            &copy_link_item,
            &copy_item,
            &paste_item,
            &select_item,
        ] {
            if let Some(child) = item.child() {
                child.set_halign(gtk::Align::Start);
            }
            menu_box.append(item);
        }
        {
            let terminal = terminal.clone();
            let popover = popover.clone();
            copy_item.connect_clicked(move |_| {
                terminal.copy_clipboard_format(vte4::Format::Text);
                popover.popdown();
            });
        }
        {
            let terminal = terminal.clone();
            let popover = popover.clone();
            paste_item.connect_clicked(move |_| {
                terminal.paste_clipboard();
                popover.popdown();
            });
        }
        {
            let terminal = terminal.clone();
            let popover = popover.clone();
            select_item.connect_clicked(move |_| {
                terminal.select_all();
                popover.popdown();
            });
        }
        {
            let events = self.workspace.events.clone();
            let popover = popover.clone();
            let hovered_url = hovered_url.clone();
            open_link_item.connect_clicked(move |_| {
                if let Some(url) = hovered_url.borrow().clone() {
                    events.publish(taste_core::Event::OpenUrlRequested(url));
                }
                popover.popdown();
            });
        }
        {
            let popover = popover.clone();
            let hovered_url = hovered_url.clone();
            copy_link_item.connect_clicked(move |button| {
                if let Some(url) = hovered_url.borrow().as_deref() {
                    button.clipboard().set_text(url);
                }
                popover.popdown();
            });
        }
        let right_click = gtk::GestureClick::builder().button(3).build();
        {
            let terminal = terminal.clone();
            let popover = popover.clone();
            let copy_item = copy_item.clone();
            right_click.connect_pressed(move |_, _, x, y| {
                let (url, _) = terminal.check_match_at(x, y);
                let url = url.map(|u| u.to_string());
                open_link_item.set_sensitive(url.is_some());
                copy_link_item.set_sensitive(url.is_some());
                *hovered_url.borrow_mut() = url;
                copy_item.set_sensitive(terminal.has_selection());
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            });
        }
        terminal.add_controller(right_click);
        // Popovers parented to a widget must be unparented at teardown.
        terminal.connect_destroy(move |_| popover.unparent());

        let argv: Vec<&str> = std::iter::once(spec.program.as_str())
            .chain(spec.args.iter().map(String::as_str))
            .collect();

        // Inherit the session environment (an empty envv would strip PATH
        // and TERM — no colors, broken shells) and advertise truecolor.
        // `podman exec` propagates TERM from this env into the container.
        let extra_keys: Vec<&str> = extra_env.iter().map(|(k, _)| k.as_str()).collect();
        let env: Vec<String> = std::env::vars()
            .filter(|(k, _)| k != "TERM" && k != "COLORTERM" && !extra_keys.contains(&k.as_str()))
            .map(|(k, v)| format!("{k}={v}"))
            .chain([
                "TERM=xterm-256color".to_string(),
                "COLORTERM=truecolor".to_string(),
            ])
            .chain(extra_env.iter().map(|(k, v)| format!("{k}={v}")))
            .collect();
        let env_refs: Vec<&str> = env.iter().map(String::as_str).collect();

        terminal.spawn_async(
            vte4::PtyFlags::DEFAULT,
            Some(&cwd.display().to_string()),
            &argv,
            &env_refs,
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            gtk::gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    tracing::warn!("terminal spawn failed: {e}");
                }
            },
        );

        let scroller = gtk::ScrolledWindow::builder().child(&terminal).build();
        let page = self.host().append(&scroller);
        page.set_title(title);
        page.set_icon(Some(&gtk::gio::ThemedIcon::new(icon)));
        self.host().set_selected_page(&page);
        (terminal, page)
    }
}

/// Record an environment's creation time in workspace state, off-thread.
fn note_created(root: &Path, env: &EnvironmentId) {
    let root = root.to_path_buf();
    let env = env.clone();
    crate::runtime::runtime().spawn_blocking(move || {
        let mut state = taste_core::state::load(&root);
        state.root = root.clone();
        state.note_environment_created(&env, taste_core::state::now_rfc3339());
        if let Err(e) = taste_core::state::save(&root, &state) {
            tracing::warn!("recording environment {env}: {e:#}");
        }
    });
}

/// Drop a destroyed environment's metadata: a name for a clone that no
/// longer exists is a second inventory disagreeing with the disk.
fn forget_environment(root: &Path, env: &EnvironmentId) {
    let root = root.to_path_buf();
    let env = env.clone();
    crate::runtime::runtime().spawn_blocking(move || {
        let mut state = taste_core::state::load(&root);
        state.root = root.clone();
        state.forget_environment(&env);
        if let Err(e) = taste_core::state::save(&root, &state) {
            tracing::warn!("forgetting environment {env}: {e:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{EnvGit, FleetRow};
    use taste_core::ConfigAuthority;

    #[test]
    fn every_section_comes_back_as_itself() {
        // The round trip the consolidated rung makes: the section on
        // screen is written down by name, the tabs move into the editor's
        // strip (or home again), and the name picks the same section out
        // of a strip that now also holds files and terminals.
        for (index, name) in SECTIONS.iter().enumerate() {
            assert_eq!(section_index(name), index, "{name} did not round-trip");
        }
    }

    #[test]
    fn an_unknown_section_lands_on_the_environment() {
        // Persisted state from a version with different sections, or a
        // name that was never a section at all: the environment itself is
        // what this pane is about when nothing else was asked for, and a
        // panic here would be a pane that cannot open.
        assert_eq!(section_index("services"), 0);
        assert_eq!(section_index(""), 0);
        assert_eq!(section_index("queue"), 0);
        // "log" and "shells" WERE sections. The log is inside the
        // environment tab now and the shells roster is gone entirely, so
        // both are names this fallback has to absorb rather than trip on.
        assert_eq!(section_index("log"), 0);
        assert_eq!(section_index("shells"), 0);
        assert_eq!(SECTIONS[section_index("nonsense")], "environment");
    }

    fn row(authority: ConfigAuthority, state: SupervisorState) -> FleetRow {
        FleetRow {
            env: EnvironmentId::primary(),
            primary: true,
            name: "Yours".into(),
            named: false,
            state,
            authority,
            pending_rebuild: false,
            chat: None,
            git: Some(EnvGit {
                branch: Some("main".into()),
                unpublished: 0,
                dirty: 2,
            }),
            published: 0,
            disk: None,
            spend: fleet::Spend::default(),
            shells: 0,
            review: taste_core::ReviewState::Working,
            working_on: Vec::new(),
        }
    }

    fn running() -> SupervisorState {
        SupervisorState::Running {
            container_id: "9f2c1a".into(),
        }
    }

    /// The header line is the one sentence this tab says about where the
    /// user is, and it is composed rather than typed: state first, then the
    /// git facts. This asserts all three rungs of the ladder at once,
    /// because the interesting part is what the ordinary case does NOT say.
    #[test]
    fn the_header_line_names_the_mode_only_when_it_is_not_the_ordinary_one() {
        // The project's own configuration in force: the normal case, and it
        // wears no mode word at all. "Container mode" said this, and said
        // nothing — every environment that is up is a container.
        assert_eq!(
            Console::env_facts_line(&row(ConfigAuthority::Project, running())),
            "running · main · 2 dirty"
        );
        // The IDE's baseline standing in. Something IS running, so the
        // state still reads "running"; what the label adds is whose config
        // it is running.
        assert_eq!(
            Console::env_facts_line(&row(ConfigAuthority::Baseline, running())),
            "safe mode · running · main · 2 dirty"
        );
        // Nothing to run in — the rung below both modes, where the agent is
        // confined outside a container with no exec target at all.
        assert_eq!(
            Console::env_facts_line(&row(ConfigAuthority::Project, SupervisorState::Stopped)),
            "no environment · stopped · main · 2 dirty"
        );
    }

    /// The state line — always on screen, unlike the tooltip
    /// `env_facts_line` composes — keeps the container facts (state,
    /// publish counts) and drops the working-tree ones. Those are the file
    /// tree's job now: the console stopped repeating what the panel and the
    /// tree already say.
    #[test]
    fn the_state_line_keeps_publish_counts_and_drops_the_working_tree() {
        let mut published = row(ConfigAuthority::Project, running());
        published.git = Some(EnvGit {
            branch: Some("main".into()),
            unpublished: 3,
            dirty: 5,
        });
        published.published = 2;
        assert_eq!(
            Console::env_state_line(&published),
            "running · 3 unpublished · ↑2 published"
        );
        // The tooltip is where the branch and the dirty count still live,
        // off the same row — the pair is the point.
        assert_eq!(
            Console::env_facts_line(&published),
            "running · main · 3 unpublished · 5 dirty · ↑2 published"
        );
        // And the ordinary case names nothing extra on either line.
        assert_eq!(
            Console::env_state_line(&row(ConfigAuthority::Project, running())),
            "running"
        );
    }

    /// Every rung explains itself in the tooltip, and no rung is silent
    /// there — the short form is for the glance, this is for the question
    /// the glance raises.
    #[test]
    fn every_rung_of_the_ladder_says_what_it_means_for_running_and_writing() {
        let project = row(ConfigAuthority::Project, running());
        let baseline = row(ConfigAuthority::Baseline, running());
        let none = row(ConfigAuthority::Project, SupervisorState::Stopped);

        assert!(project.mode_explainer().contains("project's own"));
        assert!(baseline.mode_explainer().contains("baseline"));
        assert!(none.mode_explainer().contains("Repairs only"));
        // The three are three, not one repeated.
        assert_ne!(project.mode_explainer(), baseline.mode_explainer());
        assert_ne!(baseline.mode_explainer(), none.mode_explainer());
    }
}
