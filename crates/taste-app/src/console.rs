//! Bottom pane: tabbed console, with the fleet view pinned at its left.
//!
//! The pinned first tab **is** the environments view (ENVIRONMENTS.md →
//! "Supervision: fleet view"): one row per environment, with its mode and
//! container state live, the chat bound to it, its branch, what it has
//! published, what it costs on disk and what it has spent — and the
//! per-environment actions, including opening it for watching. Selecting a
//! row swaps the panel below it to that environment's build log, its shell
//! roster, and its podman resources.
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

use anyhow::Context;
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

use crate::fleet::{self, ChatBinding, EnvFacts, EnvGit, FleetRow, Light, PoolFacts};

/// How the window answers "which chat works in this environment".
pub type ChatLookup = Box<dyn Fn(&EnvironmentId) -> Option<ChatBinding>>;
/// How the fleet asks the window to aim the panes at an environment.
pub type OpenEnvironmentHook = Box<dyn Fn(EnvironmentId)>;
/// How the assembled fleet reaches its other renderers: the rows, the
/// `agents/*` branch names behind their published counts, and the number
/// of open issues — which is not derivable from the rows, because an
/// unclaimed issue belongs to no environment.
pub type FleetChangedHook = Box<dyn Fn(&[FleetRow], &[String], usize)>;

/// Who renders the subscription pool. Separate from the fleet hook on
/// purpose: the fleet is per-environment and this is the one pool all of
/// it draws on, so they change for different reasons and at different
/// times.
pub type PoolChangedHook = Box<dyn Fn(&PoolFacts)>;

pub struct Console {
    pub widget: gtk::Box,
    tabs: adw::TabView,
    /// The selected environment's build/lifecycle log. One buffer per
    /// environment (see `log_buffer`), swapped in on selection — a single
    /// buffer would paint one environment's build over another's.
    supervisor_log: gtk::TextView,
    fleet_page: adw::TabPage,
    services_page: adw::TabPage,
    follow_log: gtk::Switch,
    /// Shell tabs running on the machine/IDE-container — retired when the
    /// devcontainer attaches (work belongs inside it).
    host_shells: RefCell<Vec<adw::TabPage>>,
    /// The fleet: one row per environment.
    /// The header of the one-environment detail tab: which environment
    /// it is, what it is doing, and what can be done to it.
    env_dot: gtk::Box,
    env_heading: gtk::Label,
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
    /// Who else renders this fleet: gadget mode and the varlink service.
    /// The console assembles once and tells them; neither goes back to the
    /// six sources for a second opinion.
    on_fleet_changed: RefCell<Option<FleetChangedHook>>,
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
    /// The selected environment's shells.
    roster_list: gtk::ListBox,
    /// Which console tab shows which shell, so the roster can bring one to
    /// the front instead of opening a second view of it.
    /// The tab showing each shell, and the environment it belongs to.
    ///
    /// The environment is recorded rather than looked up, because a shell
    /// that has EXITED leaves the roster while its tab is deliberately
    /// kept (the output is worth reading after the command ends) — and a
    /// tab whose environment could not be answered would be a tab that
    /// belongs to whichever one is selected.
    shell_tabs: RefCell<HashMap<ShellId, (EnvironmentId, adw::TabPage)>>,
    /// Shell tabs of the environments that are not on screen. Unparented
    /// `AdwTabView`s, exactly as the editor stows its pages: a shell tab
    /// holds a live VTE — the user's own terminal among them — so it is
    /// moved out of sight, never closed.
    stowed_shells: RefCell<HashMap<EnvironmentId, adw::TabView>>,
    detail_stack: adw::ViewStack,
    /// The workspace's issue queue, read off `refs/taste/issues` in the
    /// main checkout. Held rather than re-read, for the same reason the
    /// git facts are: a render must not touch the filesystem.
    issues: RefCell<Vec<taste_git::Issue>>,
    issue_list: gtk::ListBox,
    /// The selected issue, rendered in the pane below the queue. The id,
    /// not the issue: the queue refreshes underneath and a stale copy
    /// would go on showing a state that moved.
    selected_issue: RefCell<Option<String>>,
    issue_detail: gtk::Box,
    /// Names the environment this tab is showing.
    issue_heading: gtk::Label,
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
            .build();
        // New-terminal lives at the END of the strip: pinned tabs and
        // their icons keep the left edge.
        tab_bar.set_end_action_widget(Some(&new_tab_button));

        // --- the fleet tab -------------------------------------------------
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
        // This tab is ONE environment — the one the panes are aimed at —
        // so its header names that environment rather than the category.
        // The enumeration of environments lives in the file tree's panel
        // and nowhere else: two lists of the same `FleetRow`s are two
        // things to keep in agreement, and the one that goes stale is
        // whichever the user is not looking at. The panel is on screen
        // whether this tab is selected or not, so it wins.
        let env_dot = gtk::Box::builder()
            .css_classes(["env-dot", "unknown"])
            .valign(gtk::Align::Center)
            .build();
        let heading = gtk::Label::builder()
            .label(crate::envstrip::PRIMARY_TITLE)
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
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

        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title_row.append(&env_dot);
        title_row.append(&heading);
        title_row.append(&env_chat);
        title_row.append(
            &gtk::Box::builder()
                .hexpand(true)
                .orientation(gtk::Orientation::Horizontal)
                .build(),
        );
        title_row.append(&tail_label);
        title_row.append(&follow_log);
        title_row.append(&env_actions);
        title_row.append(&refresh_button);

        let facts_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        facts_row.append(&env_state);
        facts_row.append(&env_disk);
        facts_row.append(&env_spend);

        let action_bar = gtk::Box::new(gtk::Orientation::Vertical, 2);
        action_bar.set_margin_top(8);
        action_bar.set_margin_bottom(6);
        action_bar.set_margin_start(12);
        action_bar.set_margin_end(12);
        action_bar.append(&title_row);
        action_bar.append(&facts_row);

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

        let roster_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let roster_scroller = gtk::ScrolledWindow::builder()
            .child(&roster_list)
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

        // --- the workspace's issue queue -----------------------------------
        // Not the selected environment's: issues live on one ref for the
        // whole workspace, and an unclaimed one belongs to nobody. It
        // shares this stack because the alternative is a fourth band in an
        // already-dense tab, and the heading says whose queue it is so the
        // switcher's per-environment neighbours cannot mislead.
        // "Workspace", said once and in the heading: this pane's three
        // neighbours in the switcher belong to the selected environment
        // and this one does not, and one word is cheaper than a user
        // wondering whose queue they are looking at.
        let issue_title = gtk::Label::builder()
            .label("Workspace issues")
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
        let issue_heading = gtk::Label::builder()
            .label("none yet")
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let new_issue = gtk::Button::builder()
            .label("New Issue")
            .tooltip_text(
                "Write down work for later — yours, or an agent's. Issues live on a \
                 git ref every environment can read, and reach a remote only when \
                 you push.",
            )
            .build();
        let issue_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        issue_bar.set_margin_top(8);
        issue_bar.set_margin_bottom(2);
        issue_bar.set_margin_start(12);
        issue_bar.set_margin_end(12);
        issue_bar.append(&issue_title);
        issue_bar.append(&issue_heading);
        issue_bar.append(&new_issue);

        let issue_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let issue_scroller = gtk::ScrolledWindow::builder()
            .child(&issue_list)
            .propagate_natural_height(true)
            .max_content_height(200)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let issue_detail = gtk::Box::new(gtk::Orientation::Vertical, 6);
        issue_detail.set_margin_start(14);
        issue_detail.set_margin_end(14);
        issue_detail.set_margin_bottom(10);
        let issue_detail_scroller = gtk::ScrolledWindow::builder()
            .child(&issue_detail)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let issues_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        issues_box.append(&issue_bar);
        issues_box.append(&issue_scroller);
        issues_box.append(&issue_detail_scroller);

        // The platform's own switcher over the platform's own stack.
        // This was a `GtkStackSwitcher`, which draws four separate toggle
        // buttons with the theme's own button margins between them — a row
        // whose spacing nothing in this file could make even, because the
        // gaps were the buttons' and the ends were ours. `AdwViewStack`
        // plus `AdwInlineViewSwitcher` is one widget with one padding, and
        // it is what libadwaita puts above a view like this.
        let detail_stack = adw::ViewStack::new();
        detail_stack.set_vexpand(true);
        detail_stack.add_titled(&log_scroller, Some("log"), "Log");
        detail_stack.add_titled(&roster_scroller, Some("shells"), "Shells");
        detail_stack.add_titled(&resources_scroller, Some("resources"), "Resources");
        detail_stack.add_titled(&issues_box, Some("issues"), "Issues");
        let switcher = adw::InlineViewSwitcher::builder()
            .stack(&detail_stack)
            .display_mode(adw::InlineViewSwitcherDisplayMode::Labels)
            .halign(gtk::Align::Start)
            .build();
        // One margin box around it, on the 6/12 grid the rest of this tab
        // uses, so the switcher sits at the same left edge as the header
        // above it and breathes equally above and below.
        let switcher_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        switcher_bar.set_margin_start(12);
        switcher_bar.set_margin_end(12);
        switcher_bar.set_margin_top(6);
        switcher_bar.set_margin_bottom(6);
        switcher_bar.append(&switcher);

        let intervention = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .visible(false)
            .build();

        let fleet_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        fleet_box.append(&action_bar);
        fleet_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        fleet_box.append(&switcher_bar);
        fleet_box.append(&detail_stack);
        fleet_box.append(&intervention);
        let fleet_page = tabs.append(&fleet_box);
        fleet_page.set_title("Environment");
        // Pinned tabs render icon-only: without an icon they draw as the
        // missing-image placeholder.
        fleet_page.set_icon(Some(&gtk::gio::ThemedIcon::new("taste-container-off")));

        let services = crate::services::ServicesPane::new(workspace.clone());
        let services_page = tabs.append(&services.widget);
        services_page.set_title("Services");
        // Neutral until the first real answer: red is reserved for issues.
        services_page.set_icon(Some(&gtk::gio::ThemedIcon::new("taste-services-none")));

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&tab_bar);
        widget.append(&tabs);
        tabs.set_vexpand(true);

        let console = Rc::new(Self {
            widget,
            tabs,
            supervisor_log,
            fleet_page: fleet_page.clone(),
            services_page: services_page.clone(),
            follow_log,
            host_shells: RefCell::new(Vec::new()),
            env_dot: env_dot.clone(),
            env_heading: heading.clone(),
            env_state: env_state.clone(),
            env_chat: env_chat.clone(),
            env_disk: env_disk.clone(),
            env_spend: env_spend.clone(),
            env_actions: env_actions.clone(),
            rows: RefCell::new(Vec::new()),
            selected: RefCell::new(EnvironmentId::primary()),
            git_facts: RefCell::new(HashMap::new()),
            disk_facts: RefCell::new(HashMap::new()),
            published: RefCell::new(Vec::new()),
            state: RefCell::new(taste_core::state::WorkspaceState::default()),
            chat_lookup: RefCell::new(None),
            on_open_environment: RefCell::new(None),
            on_fleet_changed: RefCell::new(None),
            pool: RefCell::new(PoolFacts::default()),
            on_pool_changed: RefCell::new(None),
            logs: RefCell::new(HashMap::new()),
            lifecycle: RefCell::new(HashMap::new()),
            resources_list,
            roster_list,
            shell_tabs: RefCell::new(HashMap::new()),
            stowed_shells: RefCell::new(HashMap::new()),
            detail_stack,
            issues: RefCell::new(Vec::new()),
            issue_list: issue_list.clone(),
            selected_issue: RefCell::new(None),
            issue_detail,
            issue_heading,
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
        let weak = Rc::downgrade(&console);
        new_issue.connect_clicked(move |_| {
            if let Some(console) = weak.upgrade() {
                console.compose_issue();
            }
        });
        let weak = Rc::downgrade(&console);
        issue_list.connect_row_selected(move |_, row| {
            let (Some(console), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            let id = console
                .issues
                .borrow()
                .get(row.index() as usize)
                .map(|issue| issue.id.clone());
            *console.selected_issue.borrow_mut() = id;
            console.show_selected_issue();
        });
        // The fleet and Services tabs are permanent fixtures.
        {
            let weak = Rc::downgrade(&console);
            console.tabs.connect_close_page(move |tabs, page| {
                let Some(console) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if *page == console.fleet_page || *page == console.services_page {
                    tabs.close_page_finish(page, false);
                    return glib::Propagation::Stop;
                }
                // Closing a shell's tab is how the user ends it: for their
                // own terminals that IS the kill, and for the agent's it
                // means nothing here shows that shell any more.
                let closing: Vec<ShellId> = console
                    .shell_tabs
                    .borrow()
                    .iter()
                    .filter(|(_, (_, tab))| *tab == *page)
                    .map(|(id, _)| *id)
                    .collect();
                for id in closing {
                    console.shell_tabs.borrow_mut().remove(&id);
                    if console
                        .workspace
                        .shells
                        .get(id)
                        .is_some_and(|entry| entry.kind == ShellKind::User)
                    {
                        console.workspace.shells.remove(id);
                    }
                }
                glib::Propagation::Proceed
            });
        }

        console.refresh_fleet();
        console.show_selected_environment();
        console.add_terminal_tab();
        console.refresh_environment_data(false);
        console
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
    pub fn set_on_fleet_changed(&self, hook: impl Fn(&[FleetRow], &[String], usize) + 'static) {
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
            hook(
                &self.rows.borrow(),
                &self.published.borrow(),
                self.open_issues(),
            );
        }
    }

    // --- the issue queue ---------------------------------------------------

    /// Issues nobody has finished. The number the gadget card and the
    /// varlink service publish, taken from the one list the queue renders.
    pub fn open_issues(&self) -> usize {
        self.issues
            .borrow()
            .iter()
            .filter(|issue| issue.state == taste_git::IssueState::Open)
            .count()
    }

    /// Land on the queue: raise the fleet tab and switch the panel to it.
    /// Where gadget mode's issues row goes.
    pub fn reveal_issues(self: &Rc<Self>) {
        self.tabs.set_selected_page(&self.fleet_page);
        self.detail_stack.set_visible_child_name("issues");
    }

    /// Re-read `refs/taste/issues` from the user's main checkout.
    ///
    /// Off the main thread, like every other git pass here: this is a tree
    /// walk plus a blob read per issue, and the queue moves whenever an
    /// agent files, claims or closes something.
    pub fn refresh_issues(self: &Rc<Self>) {
        if self.probe_issues.get() {
            return; // a fabricated queue is the point of a probe instance
        }
        let main_checkout = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                taste_git::GitWorkspace::discover(&main_checkout)
                    .and_then(|git| git.issues().ok())
                    .unwrap_or_default()
            });
            let Ok(issues) = handle.await else { return };
            let Some(console) = weak.upgrade() else {
                return;
            };
            if *console.issues.borrow() == issues {
                return; // nothing moved; leave the selection alone
            }
            *console.issues.borrow_mut() = issues;
            console.render_issues();
            console.show_selected_issue();
            console.announce_fleet();
        });
    }

    fn render_issues(self: &Rc<Self>) {
        while let Some(child) = self.issue_list.first_child() {
            self.issue_list.remove(&child);
        }
        let issues = self.issues.borrow();
        let open = issues
            .iter()
            .filter(|issue| issue.state == taste_git::IssueState::Open)
            .count();
        self.issue_heading.set_label(&match (issues.len(), open) {
            (0, _) => "none yet".to_string(),
            (total, open) if open == total => format!("{open} open"),
            (total, open) => format!("{open} open · {} closed", total - open),
        });
        // The empty state is a dim row in the list, not a StatusPage.
        // This band is about 150px tall between the fleet list and the
        // read view, and a StatusPage in it renders as one enormous
        // cropped glyph with its own text pushed off the bottom — the
        // roster and the resources list next door solved the same problem
        // the same way.
        if issues.is_empty() {
            let empty = adw::ActionRow::builder()
                .title("Nothing on the queue")
                .subtitle(
                    "Issues are how work outlives a conversation: write one and any \
                     environment can pick it up. An agent that finishes one cannot \
                     close it until its branch is merged.",
                )
                .css_classes(["dim-label"])
                .selectable(false)
                .activatable(false)
                .build();
            empty.set_subtitle_lines(3);
            empty.add_prefix(&gtk::Image::from_icon_name("checkbox-symbolic"));
            self.issue_list.append(&empty);
        }

        let selected = self.selected_issue.borrow().clone();
        let mut selected_index: Option<i32> = None;
        for (index, issue) in issues.iter().enumerate() {
            self.issue_list.append(&build_issue_row(issue));
            if Some(&issue.id) == selected.as_ref() {
                selected_index = Some(index as i32);
            }
        }
        drop(issues);
        if let Some(row) = selected_index.and_then(|index| self.issue_list.row_at_index(index)) {
            self.issue_list.select_row(Some(&row));
        } else {
            *self.selected_issue.borrow_mut() = None;
        }
    }

    /// The read view: an issue in the pane below the queue.
    ///
    /// Rendered in place rather than materialised as an editor tab. A
    /// read-only badged buffer is the right answer for a *file* in another
    /// checkout — it has a path the user can act on — but an issue has no
    /// path, and inventing one to hang a badge on would be a temporary
    /// file outside every checkout that `write_allowed` reasons about.
    fn show_selected_issue(self: &Rc<Self>) {
        while let Some(child) = self.issue_detail.first_child() {
            self.issue_detail.remove(&child);
        }
        let selected = self.selected_issue.borrow().clone();
        let issues = self.issues.borrow();
        let Some(issue) = selected.and_then(|id| issues.iter().find(|issue| issue.id == id)) else {
            self.issue_detail.append(
                &gtk::Label::builder()
                    .label(if issues.is_empty() {
                        ""
                    } else {
                        "Select an issue to read it."
                    })
                    .css_classes(["dim-label"])
                    .xalign(0.0)
                    .margin_top(8)
                    .build(),
            );
            return;
        };

        self.issue_detail.append(
            &gtk::Label::builder()
                .label(&issue.title)
                .css_classes(["title-4"])
                .xalign(0.0)
                .wrap(true)
                .margin_top(10)
                .build(),
        );
        let mut facts = vec![
            issue.id.clone(),
            issue.state.as_str().to_string(),
            match &issue.assignee {
                Some(env) => format!("claimed by {env}"),
                None => "unclaimed".to_string(),
            },
            crate::filetree::relative_age(issue.created),
            // The composer files as "user"; saying so back to the person
            // who typed it is a machine talking about them in the third
            // person.
            format!(
                "filed by {}",
                if issue.reporter == "user" {
                    "you"
                } else {
                    &issue.reporter
                }
            ),
        ];
        if !issue.labels.is_empty() {
            facts.push(issue.labels.join(", "));
        }
        self.issue_detail.append(
            &gtk::Label::builder()
                .label(facts.join("  ·  "))
                .css_classes(["caption", "dim-label"])
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        for link in &issue.links {
            self.issue_detail.append(
                &gtk::Label::builder()
                    .label(format!("↳ {}", link.branch))
                    .css_classes(["caption", "monospace", "dim-label"])
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
        }
        if !issue.body.is_empty() {
            self.issue_detail.append(
                &gtk::Label::builder()
                    .label(&issue.body)
                    .xalign(0.0)
                    .yalign(0.0)
                    .wrap(true)
                    .selectable(true)
                    .margin_top(4)
                    .build(),
            );
        }
        for comment in &issue.comments {
            self.issue_detail
                .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            self.issue_detail.append(
                &gtk::Label::builder()
                    .label(format!(
                        "{} · {}",
                        comment.author,
                        crate::filetree::relative_age(comment.created)
                    ))
                    .css_classes(["caption-heading", "dim-label"])
                    .xalign(0.0)
                    .build(),
            );
            self.issue_detail.append(
                &gtk::Label::builder()
                    .label(&comment.body)
                    .xalign(0.0)
                    .wrap(true)
                    .selectable(true)
                    .build(),
            );
        }
    }

    /// Write an issue. The intervention panel, not a dialog — the same
    /// convention rename and destroy follow, and the same one the file
    /// tree's dirty-file flows do.
    fn compose_issue(self: &Rc<Self>) {
        let content = self.open_intervention("New Issue");
        content.append(
            &gtk::Label::builder()
                .label(
                    "Filed on refs/taste/issues in this checkout, where every \
                     environment can read it. It reaches a remote only when you push.",
                )
                .css_classes(["caption", "dim-label"])
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        let title = gtk::Entry::builder()
            .placeholder_text("What needs doing")
            .hexpand(true)
            .build();
        content.append(&title);

        content.append(
            &gtk::Label::builder()
                .label("Body — markdown, optional")
                .css_classes(["caption-heading", "dim-label"])
                .xalign(0.0)
                .margin_top(2)
                .build(),
        );
        let body = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .build();
        let body_frame = gtk::ScrolledWindow::builder()
            .child(&body)
            .height_request(84)
            .css_classes(["card"])
            .build();
        content.append(&body_frame);

        let file = gtk::Button::builder()
            .label("File Issue")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::End)
            .build();
        actions.append(&file);
        content.append(&actions);

        // An issue with no title is not an issue.
        {
            let file = file.clone();
            title.connect_changed(move |entry| {
                file.set_sensitive(!entry.text().trim().is_empty());
            });
        }

        let weak = Rc::downgrade(self);
        let body_buffer = body.buffer();
        let title_entry = title.clone();
        let submit = move || {
            let Some(console) = weak.upgrade() else {
                return;
            };
            let title = title_entry.text().trim().to_string();
            if title.is_empty() {
                return;
            }
            let (start, end) = body_buffer.bounds();
            let body = body_buffer.text(&start, &end, false).to_string();
            let root = console.workspace.root().to_path_buf();
            let events = console.workspace.events.clone();
            let weak = Rc::downgrade(&console);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    taste_git::GitWorkspace::discover(&root)
                        .context("this workspace is not a git repository")?
                        .issue_create(&title, &body, &[], "user")
                });
                let filed = match handle.await {
                    Ok(Ok(issue)) => issue,
                    Ok(Err(e)) => {
                        events.publish(taste_core::Event::Toast(format!("{e:#}")));
                        return;
                    }
                    Err(_) => return,
                };
                let Some(console) = weak.upgrade() else {
                    return;
                };
                console.close_intervention();
                *console.selected_issue.borrow_mut() = Some(filed.id.clone());
                events.publish(taste_core::Event::Toast(format!(
                    "Filed {} — push to share it",
                    filed.id
                )));
                console.refresh_issues();
            });
        };
        {
            let submit = submit.clone();
            file.connect_clicked(move |_| submit());
        }
        title.connect_activate(move |_| submit());
    }

    /// Is the fleet the console tab the user can see? The notification
    /// rule's "already looking at it" for an environment.
    pub fn fleet_on_screen(&self) -> bool {
        self.tabs.selected_page().as_ref() == Some(&self.fleet_page)
    }

    /// Land on one environment's row: raise the fleet tab and select it.
    /// Where a notification click about an environment, and gadget mode's
    /// click-through on a row with no chat, both end up.
    pub fn reveal_environment(self: &Rc<Self>, env: &EnvironmentId) {
        self.tabs.set_selected_page(&self.fleet_page);
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

    /// The header of the one environment this tab is about.
    ///
    /// This was a list of every environment. The list is gone: the file
    /// tree's panel enumerates them permanently, with a traffic light and
    /// a sparkline each, so a second list here was a second rendering of
    /// the same `FleetRow`s for the same glance. What a one-line panel row
    /// cannot carry is what stayed — the state in words, the lifecycle
    /// actions, the build log, the shell roster, podman's resources, the
    /// queue.
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
            // destroyed under the tab. Name it and admit the rest.
            self.env_heading.set_label(env.as_str());
            self.env_state.set_label("state not known yet");
            self.env_state.set_tooltip_text(None);
            self.env_disk.set_label("");
            self.env_spend.set_label("");
            self.set_env_dot(Light::Unknown);
            self.env_actions.set_sensitive(false);
            return;
        };
        self.env_heading.set_label(&crate::envstrip::title_of(&row));
        self.env_state.set_label(&Self::env_facts_line(&row));
        // The short form is on the line; what it MEANS for what can run and
        // what can be written is a sentence, and a sentence belongs in a
        // tooltip rather than in a header the eye scans.
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
        self.set_env_dot(row.light());
        self.env_actions.set_sensitive(true);
        self.env_actions.set_popover(Some(&self.env_menu(&row)));

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

    /// The dot beside the heading: the same three lights the panel shows,
    /// from the same mapping. Two surfaces colouring one environment
    /// differently is the whole reason that mapping lives in `fleet.rs`.
    fn set_env_dot(&self, light: Light) {
        for other in [Light::Green, Light::Amber, Light::Red, Light::Unknown] {
            self.env_dot.remove_css_class(other.css());
        }
        self.env_dot.add_css_class(light.css());
    }

    /// Everything about this environment that is a fact rather than a
    /// state, in one line — the subtitle the fleet row used to carry, which
    /// has nowhere else to go now that the row is gone.
    ///
    /// It opens with [`FleetRow::state_text`], which no longer spends its
    /// first two words saying "container mode": every environment that is
    /// up is a container, so the normal case is unmarked and the line
    /// starts with what is actually happening. See
    /// [`FleetRow::mode_text`] for the ladder it does name.
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
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let published = taste_git::GitWorkspace::discover(&main_checkout)
                    .and_then(|git| git.branches_matching(taste_git::AGENT_BRANCH_PREFIX).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|branch| branch.name)
                    .collect::<Vec<String>>();
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
                (published, facts)
            });
            let Ok((published, facts)) = handle.await else {
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
                self.tabs.close_page(&page);
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

    /// Title and badge the tab: the environment it shows, and what that
    /// environment is doing.
    ///
    /// It used to aggregate the fleet — "Environments · 3/5 up · 1 failed"
    /// — and that number is now on screen permanently as one traffic light
    /// per row in the file tree's panel, tab selected or not. An aggregate
    /// here would compete with those dots for the same glance, so the tab
    /// says instead the thing a panel row has no width for: what THIS
    /// environment is doing, in words.
    fn refresh_fleet_badge(&self) {
        let env = self.selected.borrow().clone();
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == env)
            .cloned();
        let Some(row) = row else {
            self.fleet_page.set_title("Environment");
            self.fleet_page.set_tooltip("");
            self.fleet_page.set_needs_attention(false);
            self.fleet_page
                .set_icon(Some(&gtk::gio::ThemedIcon::new("taste-container-off")));
            self.fleet_page.set_indicator_icon(gtk::gio::Icon::NONE);
            self.fleet_page.set_indicator_tooltip("");
            return;
        };
        // The name alone. A tab is a handful of characters wide and the
        // state text does not survive the ellipsis — it goes in the
        // tooltip, and in the header two lines below, where there is room
        // to read it.
        self.fleet_page.set_title(&crate::envstrip::title_of(&row));
        self.fleet_page
            .set_tooltip(&format!("{}\n{}", row.env, Self::env_facts_line(&row)));
        self.fleet_page
            .set_needs_attention(matches!(row.state, SupervisorState::Failed { .. }));
        // Pinned tabs render icon-only: without an icon they draw as the
        // missing-image placeholder.
        self.fleet_page.set_icon(Some(&gtk::gio::ThemedIcon::new(
            if row.pending_rebuild || row.baseline() {
                // A baseline container is up but standing in, which is
                // the warn icon's meaning and matches the amber light
                // the same row reports.
                "taste-container-warn"
            } else if row.container_mode() {
                "taste-container-on"
            } else {
                "taste-container-off"
            },
        )));
        if row.pending_rebuild {
            self.fleet_page
                .set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(
                    "software-update-available-symbolic",
                )));
            self.fleet_page.set_indicator_tooltip(
                "This environment's configuration changed under a running container",
            );
        } else {
            self.fleet_page.set_indicator_icon(gtk::gio::Icon::NONE);
            self.fleet_page.set_indicator_tooltip("");
        }
    }

    // --- the selected environment's detail -------------------------------

    fn selected_supervisor(&self) -> Option<Arc<Supervisor>> {
        self.environments.get(&self.selected.borrow())
    }

    fn show_selected_environment(self: &Rc<Self>) {
        let env = self.selected.borrow().clone();
        self.supervisor_log.set_buffer(Some(&self.log_buffer(&env)));
        self.scroll_log_to_end();
        self.refresh_roster();
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

    /// The selected environment's shells: the user's, the agent's, the
    /// `ide_exec` mirrors, and the lifecycle stream.
    fn refresh_roster(self: &Rc<Self>) {
        while let Some(child) = self.roster_list.first_child() {
            self.roster_list.remove(&child);
        }
        let env = self.selected.borrow().clone();
        let entries = self.workspace.shells.list(Some(&env));
        if entries.is_empty() {
            self.roster_list.append(
                &adw::ActionRow::builder()
                    .title("Nothing running here")
                    .subtitle(
                        "The user's terminals, the agent's terminals and its \
                         ide_exec commands appear here while they run.",
                    )
                    .css_classes(["dim-label"])
                    .build(),
            );
            return;
        }
        let tag_group = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        for entry in entries {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&entry.command))
                .title_lines(1)
                .subtitle(format!("{} · {}", entry.kind.noun(), entry.state.summary()))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(match entry.kind {
                ShellKind::User => "utilities-terminal-symbolic",
                ShellKind::Agent => "system-users-symbolic",
                ShellKind::ExecJob => "system-run-symbolic",
                ShellKind::Lifecycle => "package-x-generic-symbolic",
            }));
            // The ownership tag is a column too, not just the buttons: it
            // appears on some rows and not others, and a word that comes
            // and goes moves everything to its right. The size group gives
            // every row the widest tag's width, so the empty ones hold the
            // space open without anything hard-coded about how wide the
            // word "yours" renders.
            let tag_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            tag_slot.set_halign(gtk::Align::End);
            tag_group.add_widget(&tag_slot);
            if entry.kind.interactive() {
                tag_slot.append(
                    &gtk::Label::builder()
                        .label("yours")
                        .css_classes(["caption", "dim-label"])
                        .tooltip_text("Your own terminal: type in it, and close the tab to end it")
                        .build(),
                );
            }
            row.add_suffix(&tag_slot);
            // The actions are icons in fixed columns, and the reason is
            // that the rows are read as a column rather than one at a time:
            // words made every row a different width, and a row without
            // Kill slid Show under the previous row's Kill. Icons say the
            // same thing in the same place, and the words move to the
            // tooltips, where a list row's actions conventionally keep them.
            let show = gtk::Button::builder()
                .icon_name("go-jump-symbolic")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .tooltip_text(match entry.kind {
                    ShellKind::Lifecycle => "Show this environment's build log",
                    _ => "Bring this shell's console tab to the front",
                })
                .build();
            {
                let weak = Rc::downgrade(self);
                let id = entry.id;
                let lifecycle = entry.kind == ShellKind::Lifecycle;
                show.connect_clicked(move |_| {
                    let Some(console) = weak.upgrade() else {
                        return;
                    };
                    if lifecycle {
                        console.detail_stack.set_visible_child_name("log");
                        return;
                    }
                    let page = console
                        .shell_tabs
                        .borrow()
                        .get(&id)
                        .map(|(_, page)| page.clone());
                    if let Some(page) = page {
                        console.tabs.set_selected_page(&page);
                    }
                });
            }
            row.add_suffix(&show);
            // Kill's column exists on every row, whether or not this shell
            // can be killed: the user's own terminals end by closing their
            // tab, and an empty slot keeps Show where the eye left it. The
            // placeholder is the same button, so the reserved width is the
            // real width — `visible(false)` would have collapsed the box
            // and put us back where we started. Invisible to the eye, to
            // the pointer, to the keyboard and to the screen reader alike.
            // The same glyph the chat's stop button wears: `process-stop`
            // draws an ✗ at this size, which reads as "close this row"
            // rather than "stop what it is running" — and closing a row is
            // exactly what Kill does not do, since the output stays.
            let kill = gtk::Button::builder()
                .icon_name("media-playback-stop-symbolic")
                .css_classes(["flat", "destructive-action"])
                .valign(gtk::Align::Center)
                .build();
            if entry.killable {
                kill.set_tooltip_text(Some("Stop this command. The output stays."));
                let shells = self.workspace.shells.clone();
                let id = entry.id;
                kill.connect_clicked(move |button| {
                    button.set_sensitive(false);
                    shells.kill(id);
                });
            } else {
                kill.set_opacity(0.0);
                kill.set_sensitive(false);
                kill.set_can_focus(false);
                kill.set_can_target(false);
                kill.update_state(&[gtk::accessible::State::Hidden(true)]);
            }
            row.add_suffix(&kill);
            self.roster_list.append(&row);
        }
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
            if unpublished.is_empty() && dirty == 0 {
                text.push_str(
                    "Nothing here is unpublished: everything this environment \
                     committed is already in your checkout.\n\n",
                );
            } else {
                text.push_str("This environment holds work nobody else has:\n");
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
            self.tabs.close_page(&page);
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
        let tabs = self.tabs.clone();
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

    /// Bring the fleet tab's log view to the front for one environment (the
    /// safe-mode banner's "View Log" lands here).
    pub fn show_devcontainer_log(self: &Rc<Self>, env: &EnvironmentId) {
        self.note_watching(env);
        self.detail_stack.set_visible_child_name("log");
        self.tabs.set_selected_page(&self.fleet_page);
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
            let page = self.tabs.append(&scroller);
            page.set_title("Flatpak");
            page.set_icon(Some(&gtk::gio::ThemedIcon::new("folder-download-symbolic")));
            self.tabs.set_page_pinned(&page, true);
            self.tabs.set_selected_page(&page);
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
        // A shell that exits takes its console with it — after a 5s
        // countdown toast the user can cancel.
        // The page handle comes straight from spawn_tab: walking widget
        // parents into TabView internals made tabs.page() panic inside a
        // GTK callback — a non-unwinding abort on host runs.
        {
            let weak = Rc::downgrade(self);
            let page = page.clone();
            let sink = sink.clone();
            terminal.connect_child_exited(move |_, status| {
                sink.finish(taste_core::ShellState::Exited {
                    code: Some(status),
                    signal: None,
                });
                if let Some(console) = weak.upgrade() {
                    console.countdown_close(page.clone(), "Shell exited");
                }
            });
        }
        // Closing the tab is what ends it; the close handler above takes
        // the roster entry with it.
        if !in_devcontainer && !exec.is_inside_container() {
            self.host_shells.borrow_mut().push(page);
        }
    }

    /// Open tabs for shells this console has not seen yet, and refresh the
    /// roster of the environment that changed.
    ///
    /// Driven by `Event::ShellRosterChanged`, which is deliberately coarse
    /// — it says "look again", not what changed. Output never travels on
    /// the bus; each tab subscribes to its own shell and pumps from there.
    ///
    /// Only the agent's shells get tabs. The user's own terminals are
    /// already tabs (this console spawned them), and the lifecycle stream
    /// is the log view.
    pub fn sync_shell_roster(self: &Rc<Self>, env: &EnvironmentId) {
        self.sync_shell_tabs();
        if *self.selected.borrow() == *env {
            self.refresh_roster();
        }
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
        let on_screen: Vec<adw::TabPage> = (0..self.tabs.n_pages())
            .map(|index| self.tabs.nth_page(index))
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
            self.tabs.transfer_page(&page, &holding, holding.n_pages());
            self.stowed_shells.borrow_mut().insert(owner, holding);
        }
        // Back in: everything this environment had stowed, in the order it
        // was stowed in.
        if let Some(holding) = self.stowed_shells.borrow_mut().remove(&env) {
            while holding.n_pages() > 0 {
                let page = holding.nth_page(0);
                holding.transfer_page(&page, &self.tabs, self.tabs.n_pages());
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

        let page = self.tabs.append(&content);
        page.set_title(&entry.label());
        page.set_icon(Some(&gtk::gio::ThemedIcon::new(match entry.kind {
            ShellKind::ExecJob => "system-run-symbolic",
            _ => "utilities-terminal-symbolic",
        })));
        self.shell_tabs
            .borrow_mut()
            .insert(entry.id, (entry.env.clone(), page.clone()));
        // Agent work must not steal the tab the user is reading. Unlike a
        // terminal the user asked for, nobody asked for this one.
        if self.tabs.selected_page().is_none() {
            self.tabs.set_selected_page(&page);
        }

        feed(&terminal, backlog.as_bytes());
        let weak = Rc::downgrade(self);
        let env = entry.env.clone();
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                match update {
                    taste_core::ShellUpdate::Output(bytes) => feed(&terminal, &bytes),
                    taste_core::ShellUpdate::State(state) => {
                        status.set_label(&state.summary());
                        kill.set_sensitive(false);
                        if let Some(console) = weak.upgrade() {
                            if *console.selected.borrow() == env {
                                console.refresh_roster();
                            }
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
    pub fn seed_agent_terminal_for_probe(self: &Rc<Self>, env: &EnvironmentId) {
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
        self.sync_shell_roster(env);
    }

    /// TASTE_PROBE_CHECK only: choose which of the fleet tab's detail pages
    /// is showing.
    ///
    /// The default is the build log, which on a probe is empty because
    /// nothing has been built — a large void under the fleet list. A
    /// screenshot of a pane with nothing in it says nothing about the pane,
    /// so a shot that is not specifically about the log picks a page that
    /// has content.
    pub fn seed_detail_page_for_probe(self: &Rc<Self>, name: &str) {
        self.detail_stack.set_visible_child_name(name);
    }

    /// TASTE_PROBE_CHECK only: fabricate a fleet with more than one
    /// environment in it.
    ///
    /// Cloning real repositories headlessly would work and would take
    /// minutes; what a screenshot has to show is the rendering, and the
    /// rendering's only input is [`EnvFacts`]. So the facts are the seam,
    /// the same way the roster is for a terminal.
    /// A queue with something in it, for the screenshot. `mode` picks what
    /// the shot is of: the populated queue, its empty state, or the
    /// composer over it.
    pub fn seed_issues_for_probe(self: &Rc<Self>, mode: &str) {
        self.probe_issues.set(true);
        // "hidden" seeds the data without taking over the panel: gadget
        // mode reads the count off the same snapshot and the console's
        // other screenshots keep showing the log.
        if mode != "hidden" {
            self.detail_stack.set_visible_child_name("issues");
        }
        if mode != "empty" {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let issue = |id: &str,
                         title: &str,
                         state,
                         assignee: Option<&str>,
                         age: i64,
                         links: Vec<&str>,
                         body: &str,
                         comments: Vec<(&str, &str)>| {
                taste_git::Issue {
                    id: id.into(),
                    title: title.into(),
                    state,
                    reporter: "user".into(),
                    assignee: assignee.map(str::to_string),
                    created: now - age,
                    updated: now - age / 2,
                    labels: Vec::new(),
                    links: links
                        .into_iter()
                        .map(|branch| taste_git::IssueLink {
                            branch: branch.into(),
                            tip: None,
                        })
                        .collect(),
                    body: body.into(),
                    comments: comments
                        .into_iter()
                        .enumerate()
                        .map(|(index, (author, text))| taste_git::Comment {
                            seq: index as u32 + 1,
                            author: author.into(),
                            created: now - age / 3,
                            body: text.into(),
                        })
                        .collect(),
                }
            };
            *self.issues.borrow_mut() = vec![
                issue(
                    "i-0001",
                    "The inbox filter forgets its scroll position",
                    taste_git::IssueState::Open,
                    Some("calm-1"),
                    9_000,
                    vec!["agents/calm-1/inbox-scroll"],
                    "Open the Inbox filter, scroll to the bottom, open a branch \
                     and come back: the list is at the top again.\n\n\
                     The row model is rebuilt on every status refresh, which is \
                     the right thing; the scroll adjustment should survive it.",
                    vec![(
                        "calm-1",
                        "Published agents/calm-1/inbox-scroll — the adjustment is \
                         restored after the rebuild rather than before it.",
                    )],
                ),
                issue(
                    "i-0002",
                    "Decide what a stopped environment costs",
                    taste_git::IssueState::Open,
                    None,
                    52_000,
                    Vec::new(),
                    "The fleet shows a footprint per environment. Idle-stop keeps \
                     the clone and the volumes, so the number does not move when \
                     a container stops — worth saying so in the row.",
                    Vec::new(),
                ),
                issue(
                    "i-0003",
                    "Terminal tabs should keep their output after the process exits",
                    taste_git::IssueState::Closed,
                    Some("spry-2"),
                    260_000,
                    vec!["agents/spry-2/keep-output"],
                    "Closing on exit throws away the record of what happened.",
                    Vec::new(),
                ),
            ];
            *self.selected_issue.borrow_mut() = Some("i-0001".into());
        }
        self.render_issues();
        self.show_selected_issue();
        if mode == "composer" {
            self.compose_issue();
        }
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
        let make = |slug: &str,
                    state,
                    chat: Option<(&str, bool, bool, bool)>,
                    git,
                    disk_mib: (u64, u64),
                    spend,
                    shells| EnvFacts {
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
                    branch: Some("topic/inbox-filter".into()),
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
            ),
            make(
                "brisk-3",
                SupervisorState::Building,
                Some(("Varlink service", false, false, false)),
                EnvGit {
                    branch: Some("agents/brisk-3/fleet-varlink".into()),
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
            ),
            make(
                "wry-4",
                SupervisorState::Stopped,
                Some(("Disk accounting", false, false, false)),
                EnvGit {
                    branch: Some("agents/wry-4/disk-footprint".into()),
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
            ),
            make(
                "spry-2",
                SupervisorState::Running {
                    container_id: "3e7b04".into(),
                },
                Some(("Terminal roster", true, true, false)),
                EnvGit {
                    branch: Some("agents/spry-2/keep-output".into()),
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
            ),
        ];
        // The order here is truncation order, not display order (the rows
        // sort by name): keeping the first three keeps one environment in
        // each state a fleet is actually found in — running, building, and
        // stopped — so a shot that cannot fit all of them still shows what
        // the states look like side by side.
        self.probe_rows.borrow_mut().truncate(keep);
        *self.published.borrow_mut() = vec![
            "agents/calm-1/inbox-scroll".into(),
            "agents/spry-2/keep-output".into(),
        ];
        self.refresh_fleet();
        self.tabs.set_selected_page(&self.fleet_page);
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
        let page = self.tabs.append(&scroller);
        page.set_title(title);
        page.set_icon(Some(&gtk::gio::ThemedIcon::new(icon)));
        self.tabs.set_selected_page(&page);
        (terminal, page)
    }
}

/// Record an environment's creation time in workspace state, off-thread.
/// One issue, as a queue row: what it is, who has it, and how long it has
/// been waiting. Closed issues stay in the list — a queue that hides its
/// history makes "is this done?" a question you have to ask git.
fn build_issue_row(issue: &taste_git::Issue) -> adw::ActionRow {
    let mut subtitle = vec![issue.id.clone()];
    match &issue.assignee {
        Some(env) => subtitle.push(format!("claimed by {env}")),
        None if issue.state == taste_git::IssueState::Open => subtitle.push("unclaimed".into()),
        None => {}
    }
    if !issue.links.is_empty() {
        subtitle.push(format!(
            "{} branch{}",
            issue.links.len(),
            if issue.links.len() == 1 { "" } else { "es" }
        ));
    }
    if !issue.comments.is_empty() {
        subtitle.push(format!(
            "{} comment{}",
            issue.comments.len(),
            if issue.comments.len() == 1 { "" } else { "s" }
        ));
    }
    subtitle.push(crate::filetree::relative_age(issue.updated));

    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&issue.title))
        .subtitle(subtitle.join(" · "))
        .activatable(true)
        .build();
    let closed = issue.state == taste_git::IssueState::Closed;
    row.add_prefix(
        &gtk::Image::builder()
            .icon_name(if closed {
                "checkbox-checked-symbolic"
            } else {
                "checkbox-symbolic"
            })
            .css_classes(if closed {
                vec!["dim-label"]
            } else {
                Vec::new()
            })
            .build(),
    );
    if closed {
        row.add_css_class("dim-label");
    }
    row
}

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
