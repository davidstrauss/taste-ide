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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use taste_core::environment::EnvironmentId;
use taste_core::{ShellId, ShellKind, ShellSink, Workspace};
use taste_devcontainer::{
    EnvironmentRegistry, ResourceInfo, ResourceKind, Supervisor, SupervisorState,
};
use vte4::prelude::*;

use crate::fleet::{self, ChatBinding, EnvFacts, EnvGit, FleetRow};

/// How the window answers "which chat works in this environment".
pub type ChatLookup = Box<dyn Fn(&EnvironmentId) -> Option<ChatBinding>>;
/// How the fleet asks the window to aim the panes at an environment.
pub type OpenEnvironmentHook = Box<dyn Fn(EnvironmentId)>;

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
    fleet_list: gtk::ListBox,
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
    shell_tabs: RefCell<HashMap<ShellId, adw::TabPage>>,
    detail_stack: gtk::Stack,
    /// Created lazily on the first Flatpak log line, so projects without a
    /// manifest never see the tab.
    flatpak_log: RefCell<Option<gtk::TextView>>,
    /// The pinned Services tab: systemd units + journal in the container.
    services: Rc<crate::services::ServicesPane>,
    /// The highest shell id this console has already opened a tab for.
    ///
    /// Roster ids are monotonic and never reused, so one number is the
    /// whole of "which shells have I seen" — no set to grow, and no way to
    /// resurrect a tab the user closed. A shell that ends, or is released,
    /// leaves its tab behind on purpose: the output is the record of what
    /// happened, and it stays until the user closes it.
    last_shell: Cell<ShellId>,
    /// The fleet's intervention panel: rename, and the destroy confirmation
    /// that lists what would be lost. Never a modal — the same convention
    /// the file tree's dirty-file flows follow.
    intervention: gtk::Box,
    /// Probe-only fabricated environments (TASTE_PROBE_CHECK).
    probe_rows: RefCell<Vec<EnvFacts>>,
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
        let new_environment = gtk::Button::builder()
            .label("New Environment")
            .tooltip_text(
                "Clone the workspace into a new environment. It gets its own \
                 checkout and devcontainer, and no chat until you give it one.",
            )
            .build();
        let action_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        action_bar.set_margin_top(8);
        action_bar.set_margin_bottom(6);
        action_bar.set_margin_start(12);
        action_bar.set_margin_end(12);
        let heading = gtk::Label::builder()
            .label("Environments")
            .css_classes(["heading"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        action_bar.append(&heading);
        action_bar.append(&tail_label);
        action_bar.append(&follow_log);
        action_bar.append(&new_environment);
        action_bar.append(&refresh_button);

        let fleet_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();
        let fleet_scroller = gtk::ScrolledWindow::builder()
            .child(&fleet_list)
            .propagate_natural_height(true)
            .max_content_height(220)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

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

        let detail_stack = gtk::Stack::new();
        detail_stack.set_vexpand(true);
        detail_stack.add_titled(&log_scroller, Some("log"), "Log");
        detail_stack.add_titled(&roster_scroller, Some("shells"), "Shells");
        detail_stack.add_titled(&resources_scroller, Some("resources"), "Resources");
        let switcher = gtk::StackSwitcher::builder()
            .stack(&detail_stack)
            .halign(gtk::Align::Start)
            .margin_start(12)
            .margin_top(2)
            .build();

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
        fleet_box.append(&fleet_scroller);
        fleet_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        fleet_box.append(&switcher);
        fleet_box.append(&detail_stack);
        fleet_box.append(&intervention);
        let fleet_page = tabs.append(&fleet_box);
        fleet_page.set_title("Environments");
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
            fleet_list: fleet_list.clone(),
            rows: RefCell::new(Vec::new()),
            selected: RefCell::new(EnvironmentId::primary()),
            git_facts: RefCell::new(HashMap::new()),
            disk_facts: RefCell::new(HashMap::new()),
            published: RefCell::new(Vec::new()),
            state: RefCell::new(taste_core::state::WorkspaceState::default()),
            chat_lookup: RefCell::new(None),
            on_open_environment: RefCell::new(None),
            logs: RefCell::new(HashMap::new()),
            lifecycle: RefCell::new(HashMap::new()),
            resources_list,
            roster_list,
            shell_tabs: RefCell::new(HashMap::new()),
            detail_stack,
            flatpak_log: RefCell::new(None),
            services,
            last_shell: Cell::new(0),
            intervention,
            probe_rows: RefCell::new(Vec::new()),
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
        new_environment.connect_clicked(move |button| {
            if let Some(console) = weak.upgrade() {
                console.create_environment(button.clone());
            }
        });
        let weak = Rc::downgrade(&console);
        fleet_list.connect_row_selected(move |_, row| {
            let (Some(console), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            let index = row.index();
            let env = console
                .rows
                .borrow()
                .get(index as usize)
                .map(|row| row.env.clone());
            if let Some(env) = env {
                if *console.selected.borrow() != env {
                    *console.selected.borrow_mut() = env;
                    console.show_selected_environment();
                }
            }
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
                    .filter(|(_, tab)| *tab == page)
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
        if *self.rows.borrow() == rows {
            return; // nothing moved; leave the rows (and any open menu) alone
        }
        *self.rows.borrow_mut() = rows;
        self.render_fleet();
        self.refresh_fleet_badge();
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
            env,
        }
    }

    fn render_fleet(self: &Rc<Self>) {
        while let Some(child) = self.fleet_list.first_child() {
            self.fleet_list.remove(&child);
        }
        let selected = self.selected.borrow().clone();
        let mut selected_index: Option<i32> = None;
        for (index, row) in self.rows.borrow().iter().enumerate() {
            self.fleet_list.append(&self.build_fleet_row(row));
            if row.env == selected {
                selected_index = Some(index as i32);
            }
        }
        // The selection survives a re-render: rows rebuild constantly (a
        // build's states arrive one after another) and a panel that jumped
        // back to the primary each time would be unusable.
        if let Some(row) = selected_index.and_then(|index| self.fleet_list.row_at_index(index)) {
            self.fleet_list.select_row(Some(&row));
        }
    }

    /// Move the selection without rebuilding anything.
    fn select_env(self: &Rc<Self>, env: &EnvironmentId) {
        let index = self
            .rows
            .borrow()
            .iter()
            .position(|row| row.env == *env)
            .map(|index| index as i32);
        match index.and_then(|index| self.fleet_list.row_at_index(index)) {
            Some(row) => self.fleet_list.select_row(Some(&row)),
            None => {
                // No row for it (a probe row, or one just destroyed): the
                // panel still has to follow.
                *self.selected.borrow_mut() = env.clone();
                self.show_selected_environment();
            }
        }
    }

    /// One environment, as a row.
    fn build_fleet_row(self: &Rc<Self>, row: &FleetRow) -> adw::ActionRow {
        let mut subtitle = row.state_text();
        if let Some(git) = &row.git {
            if let Some(branch) = &git.branch {
                subtitle.push_str(&format!(" · {branch}"));
            }
            // Two different facts, never added together: commits the
            // checkout has never seen, and files not committed at all.
            if git.unpublished > 0 {
                subtitle.push_str(&format!(" · {} unpublished", git.unpublished));
            }
            if git.dirty > 0 {
                subtitle.push_str(&format!(" · {} dirty", git.dirty));
            }
        }
        if row.published > 0 {
            subtitle.push_str(&format!(" · ↑{} published", row.published));
        }
        let title = if row.primary {
            format!("{} — your checkout", row.name)
        } else {
            row.name.clone()
        };
        let action_row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title))
            .title_lines(1)
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_lines(1)
            .activatable(true)
            .tooltip_text(if row.primary {
                "The main checkout — the environment your panes start aimed at".to_string()
            } else {
                format!("Environment {} — its own clone and devcontainer", row.env)
            })
            .build();
        action_row.add_prefix(&gtk::Image::from_icon_name(if row.container_mode() {
            if row.pending_rebuild {
                "taste-container-warn"
            } else {
                "taste-container-on"
            }
        } else {
            "taste-container-off"
        }));

        if let Some(chat) = &row.chat {
            let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            if chat.busy {
                let spinner = gtk::Spinner::new();
                spinner.start();
                box_.append(&spinner);
            }
            box_.append(
                &gtk::Label::builder()
                    .label(glib::markup_escape_text(&chat.label))
                    .css_classes(["caption", "dim-label"])
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            box_.set_tooltip_text(Some(&if chat.busy {
                format!("{} works here, and is working now", chat.label)
            } else {
                format!("{} works here", chat.label)
            }));
            action_row.add_suffix(&box_);
        }
        if row.has_unpublished_work() {
            let marker = gtk::Label::builder()
                .label("unpublished")
                .css_classes(["caption", "warning"])
                .tooltip_text(
                    "This environment holds commits (or edits) your checkout has \
                     never seen. Destroying it would lose them.",
                )
                .build();
            action_row.add_suffix(&marker);
        }
        for (text, tip) in [
            (
                row.disk_text(),
                "Disk: clone plus this environment's volumes",
            ),
            (
                row.spend_text(),
                "Tokens spent through the IDE's auth proxy",
            ),
        ] {
            // A dash for "not measured yet" belongs in a table with fixed
            // columns; here it is one more thing crowding the row's own
            // words out of a narrow pane. Nothing measured, nothing shown.
            if text == "—" {
                continue;
            }
            action_row.add_suffix(
                &gtk::Label::builder()
                    .label(&text)
                    .css_classes(["caption", "numeric", "dim-label"])
                    .tooltip_text(tip)
                    .build(),
            );
        }

        let menu = self.row_menu(row);
        action_row.add_suffix(&menu);
        {
            // Activating a row opens it — the same action the menu offers,
            // where the pointer already is.
            let weak = Rc::downgrade(self);
            let env = row.env.clone();
            action_row.connect_activated(move |_| {
                if let Some(console) = weak.upgrade() {
                    console.open_environment(env.clone());
                }
            });
        }
        action_row
    }

    /// The per-row action menu: lifecycle, watching, and destruction.
    fn row_menu(self: &Rc<Self>, row: &FleetRow) -> gtk::MenuButton {
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = gtk::Popover::builder().child(&menu_box).build();
        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("Actions for this environment")
            .popover(&popover)
            .build();

        let running = row.container_mode();
        let entries: Vec<(&str, &str, bool, &'static str, String)> = vec![
            (
                if row.primary {
                    "Return to This Checkout"
                } else {
                    "Open Environment"
                },
                "folder-open-symbolic",
                true,
                "open",
                if row.primary {
                    "Aim the file tree and git views back at your own checkout".into()
                } else {
                    format!(
                        "Point the file tree and git views at {}'s clone — read-only",
                        row.env
                    )
                },
            ),
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
        button
    }

    fn run_row_action(self: &Rc<Self>, action: &str, env: EnvironmentId) {
        let Some(supervisor) = self.environments.get(&env) else {
            // A probe row, or one destroyed under the open menu.
            if action == "open" {
                self.open_environment(env);
            }
            return;
        };
        match action {
            "open" => self.open_environment(env),
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

    /// Aim the window's panes at an environment (or back at the primary).
    fn open_environment(self: &Rc<Self>, env: EnvironmentId) {
        // Opening a row also selects it: the panel below must not keep
        // showing a different environment's log than the tree is showing.
        self.select_env(&env);
        let hook = self.on_open_environment.borrow();
        if let Some(hook) = hook.as_ref() {
            hook(env);
        }
    }

    /// The window's answer to "the panes are aimed elsewhere now" — used
    /// when something other than a row click moved them (an environment
    /// being destroyed, say).
    pub fn note_watching(self: &Rc<Self>, env: &EnvironmentId) {
        if *self.selected.borrow() == *env {
            return;
        }
        self.select_env(env);
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
            *console.published.borrow_mut() = published;
            let mut cache = console.git_facts.borrow_mut();
            for (env, git) in facts {
                cache.insert(env, git);
            }
            drop(cache);
            console.refresh_fleet();
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

    /// Badge the fleet tab: how many environments, how many are up, and
    /// whether any of them wants attention.
    fn refresh_fleet_badge(&self) {
        let rows = self.rows.borrow();
        let total = rows.len();
        let running = rows.iter().filter(|row| row.container_mode()).count();
        let pending = rows.iter().filter(|row| row.pending_rebuild).count();
        let failed = rows
            .iter()
            .filter(|row| matches!(row.state, SupervisorState::Failed { .. }))
            .count();
        // Said in words as well as badged. A badge alone is a shape you
        // have to already know the meaning of, and a tooltip needs a hover
        // to exist at all.
        let title = match (pending, failed) {
            (0, 0) => format!("Environments · {running}/{total} up"),
            (_, 0) => format!("Environments · {running}/{total} up · {pending} need rebuild"),
            (0, _) => format!("Environments · {running}/{total} up · {failed} failed"),
            (_, _) => {
                format!("Environments · {running}/{total} up · {pending} stale, {failed} failed")
            }
        };
        self.fleet_page.set_title(&title);
        self.fleet_page.set_needs_attention(failed > 0);
        self.fleet_page
            .set_icon(Some(&gtk::gio::ThemedIcon::new(if pending > 0 {
                "taste-container-warn"
            } else if running > 0 {
                "taste-container-on"
            } else {
                "taste-container-off"
            })));
        match pending {
            0 => {
                self.fleet_page.set_indicator_icon(gtk::gio::Icon::NONE);
                self.fleet_page.set_indicator_tooltip("");
            }
            n => {
                self.fleet_page
                    .set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(
                        "software-update-available-symbolic",
                    )));
                self.fleet_page.set_indicator_tooltip(&format!(
                    "{n} environment(s) whose configuration changed under a running container"
                ));
            }
        }
    }

    // --- the selected environment's panel --------------------------------

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
            if entry.kind.interactive() {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label("yours")
                        .css_classes(["caption", "dim-label"])
                        .tooltip_text("Your own terminal: type in it, and close the tab to end it")
                        .build(),
                );
            }
            let show = gtk::Button::builder()
                .label("Show")
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
                    let page = console.shell_tabs.borrow().get(&id).cloned();
                    if let Some(page) = page {
                        console.tabs.set_selected_page(&page);
                    }
                });
            }
            row.add_suffix(&show);
            if entry.killable {
                let kill = gtk::Button::builder()
                    .label("Kill")
                    .css_classes(["flat", "destructive-action"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Stop this command. The output stays.")
                    .build();
                let shells = self.workspace.shells.clone();
                let id = entry.id;
                kill.connect_clicked(move |button| {
                    button.set_sensitive(false);
                    shells.kill(id);
                });
                row.add_suffix(&kill);
            }
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
        let mut ordered: Vec<&ResourceInfo> = Vec::new();
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
    fn create_environment(self: &Rc<Self>, button: gtk::Button) {
        let taken = self.environments.ids();
        let id = match crate::chat_tabs::fresh_environment_id(taken.len() as u32 + 1, &taken) {
            Ok(id) => id,
            Err(e) => {
                self.workspace
                    .events
                    .publish(taste_core::Event::Toast(format!("{e:#}")));
                return;
            }
        };
        button.set_sensitive(false);
        let registry = self.environments.clone();
        let events = self.workspace.events.clone();
        let root = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let created = id.clone();
            // Never on the GTK thread: this is a git clone.
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || registry.create(created).map(|_| ()));
            let outcome = handle.await;
            button.set_sensitive(true);
            match outcome {
                Ok(Ok(())) => {
                    events.publish(taste_core::Event::Toast(format!("Created {id}")));
                    note_created(&root, &id);
                    if let Some(console) = weak.upgrade() {
                        console.refresh_environment_data(false);
                    }
                }
                Ok(Err(e)) => events.publish(taste_core::Event::Toast(format!("{e:#}"))),
                Err(e) => events.publish(taste_core::Event::Toast(format!(
                    "the clone task did not finish: {e}"
                ))),
            }
        });
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
                if supervisor.exec().is_container() {
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
        self.shell_tabs.borrow_mut().insert(sink.id(), page.clone());
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
        let mut highest = self.last_shell.get();
        for entry in self.workspace.shells.list(None) {
            if entry.id <= self.last_shell.get() {
                continue;
            }
            highest = highest.max(entry.id);
            if matches!(entry.kind, ShellKind::Agent | ShellKind::ExecJob) {
                self.add_shell_tab(&entry);
            }
        }
        self.last_shell.set(highest);
        if *self.selected.borrow() == *env {
            self.refresh_roster();
        }
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
        self.shell_tabs.borrow_mut().insert(entry.id, page.clone());
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

    /// TASTE_PROBE_CHECK only: fabricate a fleet with more than one
    /// environment in it.
    ///
    /// Cloning real repositories headlessly would work and would take
    /// minutes; what a screenshot has to show is the rendering, and the
    /// rendering's only input is [`EnvFacts`]. So the facts are the seam,
    /// the same way the roster is for a terminal.
    pub fn seed_fleet_for_probe(self: &Rc<Self>) {
        let make = |slug: &str, state, chat: Option<(&str, bool)>, git, spend| EnvFacts {
            env: EnvironmentId::parse(slug).expect("valid probe slug"),
            state,
            pending_rebuild: false,
            chat: chat.map(|(label, busy)| ChatBinding {
                label: label.to_string(),
                busy,
            }),
            git: Some(git),
            disk: Some(taste_devcontainer::DiskUsage {
                checkout_bytes: 1024 * 1024 * 412,
                volume_bytes: 1024 * 1024 * 1600,
                volumes_measured: 2,
                volumes_unmeasured: 0,
            }),
            spend,
        };
        *self.probe_rows.borrow_mut() = vec![
            make(
                "calm-1",
                SupervisorState::Running {
                    container_id: "9f2c1a".into(),
                },
                Some(("Claude 2", true)),
                EnvGit {
                    branch: Some("topic/inbox-filter".into()),
                    unpublished: 2,
                    dirty: 4,
                },
                fleet::Spend {
                    requests: 37,
                    input_tokens: 412_000,
                    output_tokens: 21_400,
                },
            ),
            make(
                "spry-2",
                SupervisorState::Stopped,
                Some(("Claude 3", false)),
                EnvGit {
                    branch: Some("main".into()),
                    unpublished: 0,
                    dirty: 0,
                },
                fleet::Spend {
                    requests: 4,
                    input_tokens: 8_100,
                    output_tokens: 900,
                },
            ),
        ];
        *self.published.borrow_mut() = vec![
            "agents/calm-1/inbox-filter".into(),
            "agents/spry-2/docs-pass".into(),
        ];
        self.refresh_fleet();
        self.tabs.set_selected_page(&self.fleet_page);
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
