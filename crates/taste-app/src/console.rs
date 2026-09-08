//! Bottom pane: one flat strip of tabs — `[resources] [terminal…]`.
//!
//! It is the selected environment's **machine room**: what podman is
//! holding for it, and the shells running in it. Everything that used to
//! be an "Environment" tab beside these is gone (2026-09-06): the state in
//! words and the traffic light are the backlog row's, the actions are that
//! row's `⋮` menu and the backlog header's, the interventions open under
//! the backlog's list, the review's judgment sits on the
//! review tab in the editor, and the build log is a document the Logs
//! section opens like a file. Every one of those had a second, better home
//! already; the tab was the last place that drew them twice.
//!
//! **No nested tab sets**: Resources used to be one page of an
//! `AdwViewStack` behind an inline switcher inside a single tab, which put
//! a row of tab-shaped controls under a row of tabs. Every leaf view is a
//! first-class tab in this pane's one strip — and below
//! `CONSOLIDATED_MAX_WIDTH_SP` these pages are *transferred* into the
//! editor's strip, because down there the window has one strip and this
//! pane is not one of its regions any more. Everything that adds or raises
//! a page asks `host()`, never `tabs`.
//!
//! It is still where the off-thread git and podman passes live, and it
//! still assembles the fleet — the rows the backlog, gadget mode and
//! varlink all render. That is a *model* job, not a drawing one, and it
//! stays here because this is where the passes are.
//!
//! Terminal tabs spawn in an execution context resolved at spawn time
//! through `ExecContext` — which is what makes container reloads invisible
//! to existing tabs and automatic for new ones — and register themselves in
//! the shell roster, so the user's own shells are as visible in the fleet
//! as the agent's are.

use adw::prelude::*;
use gtk::glib;

/// Match the terminal to the IDE's (= desktop's) light/dark mode, from the
/// one palette (`palette.rs`) — and give VTE's search highlight the hit
/// colours, so a selected hit looks the same in a terminal as in a file.
fn apply_terminal_theme(terminal: &vte4::Terminal) {
    let dark = adw::StyleManager::default().is_dark();
    let (fg, bg) = if dark {
        crate::palette::TERMINAL_DARK
    } else {
        crate::palette::TERMINAL_LIGHT
    };
    let palette: Vec<gtk::gdk::RGBA> = crate::palette::ANSI_TERMINAL
        .iter()
        .map(|c| crate::palette::rgba(c))
        .collect();
    let palette_refs: Vec<&gtk::gdk::RGBA> = palette.iter().collect();
    terminal.set_colors(
        Some(&crate::palette::rgba(fg)),
        Some(&crate::palette::rgba(bg)),
        &palette_refs,
    );
    terminal.set_color_highlight(Some(&crate::palette::rgba(crate::palette::hit_background(
        dark,
    ))));
    terminal.set_color_highlight_foreground(Some(&crate::palette::rgba(
        crate::palette::hit_foreground(dark),
    )));
}

/// Where a scrollback scan is: which terminal, which row, what it found.
struct ScrollbackScan {
    terminal: usize,
    row: i64,
    rows_done: i64,
    /// Listing items per terminal (the tab on screen's are shown).
    items: Vec<Vec<crate::results::Item>>,
    /// Hits per environment id, for the backlog rows.
    counts: HashMap<String, usize>,
    count: usize,
    running: bool,
    /// How many items the listing last drew, and when: the throttle.
    rendered_items: usize,
    rendered_at: std::time::Instant,
}

/// The VTE inside a tab page, if the page is a terminal's.
fn find_terminal(widget: &gtk::Widget) -> Option<vte4::Terminal> {
    if let Some(terminal) = widget.downcast_ref::<vte4::Terminal>() {
        return Some(terminal.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_terminal(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

/// Rows `start..=end` of a terminal's scrollback as plain text, one line
/// per row. Through `vte_terminal_get_text_range_format`, which the
/// binding does not wrap; rows are the terminal's own absolute row
/// numbers, the ones its vertical adjustment scrolls in.
fn terminal_rows(terminal: &vte4::Terminal, start: i64, end: i64) -> String {
    use glib::translate::ToGlibPtr;
    if end < start {
        return String::new();
    }
    let columns = terminal.column_count() as std::ffi::c_long;
    let mut length: usize = 0;
    // SAFETY: a live VTE on the GTK thread; the returned string is ours to
    // free, and is copied out before it is.
    unsafe {
        let pointer = vte4::ffi::vte_terminal_get_text_range_format(
            terminal.to_glib_none().0,
            vte4::ffi::VTE_FORMAT_TEXT,
            start as std::ffi::c_long,
            0,
            end as std::ffi::c_long,
            columns,
            &mut length,
        );
        if pointer.is_null() {
            return String::new();
        }
        let text = std::ffi::CStr::from_ptr(pointer)
            .to_string_lossy()
            .into_owned();
        glib::ffi::g_free(pointer as *mut _);
        text
    }
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
use crate::hover::FullTextOnHover;

/// How the window answers "which chat works in this environment".
pub type ChatLookup = Box<dyn Fn(&EnvironmentId) -> Option<ChatBinding>>;
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
    /// The judgment row's fact line: the branch, the target, and how far
    /// apart they are — asked fresh, never latched. A force-moved target
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

/// How this pane raises an intervention — rename, destroy, reject — in the
/// backlog's panel, under the row the environment is. Returns the panel's
/// content box.
///
/// A hook rather than a panel of its own: the convention is a panel at the
/// bottom of the subpanel the question is about (`intervention.rs`) and
/// never a modal, an environment is a backlog row, and the pane that used
/// to hold a panel of its own is gone.
pub type OpenInterventionHook = Box<dyn Fn(&str) -> gtk::Box>;

/// What the Resources tab is for, before podman has said how big it is.
const RESOURCES_TOOLTIP: &str = "This environment's containers, volumes, and images";

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
    /// This pane's one fixture: the selected environment's podman objects.
    resources_page: adw::TabPage,
    /// The results listing at the pane's foot (results.rs): hits in every
    /// terminal's scrollback. Scrollback is read on the GTK thread by
    /// necessity (VTE owns it), so it is read in bounded chunks per frame
    /// with the rule of progress up, and a new query stops the old scan
    /// (`Search::is_current`).
    results: Rc<crate::results::ResultsPanel>,
    /// Re-lists the tab on screen's hits from the current scan, for a tab
    /// change under a standing query.
    search_render: RefCell<Option<Rc<dyn Fn()>>>,
    /// The glyph each terminal tab wore before a count took its place, to
    /// put back when the query clears.
    tab_glyphs: RefCell<HashMap<adw::TabPage, gtk::gio::Icon>>,
    /// The terminals the current listing's rows point into, by the index a
    /// `Target::Terminal` carries. Snapshotted per query: a tab closed
    /// mid-scan is a target that is simply gone.
    search_terminals: RefCell<Vec<(adw::TabPage, vte4::Terminal)>>,
    /// Shell tabs running on the machine/IDE-container — retired when the
    /// devcontainer attaches (work belongs inside it).
    host_shells: RefCell<Vec<adw::TabPage>>,
    /// The tab bar this pane owns, so the New Terminal button can be put
    /// back on it when the window grows out of the consolidated rung.
    tab_bar: adw::TabBar,
    /// "Add one more tab", which belongs to whichever BAR is currently
    /// drawing this pane's tabs. It is bar furniture, and bar furniture
    /// does not travel with a graft — see `release_new_terminal_button`.
    new_tab_button: gtk::Button,
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
    on_open_review: RefCell<Option<OpenReviewHook>>,
    /// Leaving the review, when a judgment has settled the environment.
    on_close_review: RefCell<Option<Box<dyn Fn()>>>,
    /// Who draws the mergedness and the judgment: the editor's review
    /// tabs, which is where the user is when they are looking at the work.
    on_review_facts: RefCell<Option<Box<dyn Fn(&[ReviewFacts])>>>,
    /// The window's one intervention slot (`filetree.rs`), for rename,
    /// destroy and reject.
    on_open_intervention: RefCell<Option<OpenInterventionHook>>,
    on_close_intervention: RefCell<Option<Box<dyn Fn()>>>,
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
    /// The lifecycle roster entry mirroring each environment's build
    /// output. The stream is a roster row like any other shell — it is
    /// what an environment is "running" while it is building itself.
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
    /// be a walk per row for a judgment nobody is looking at.
    review_facts: RefCell<HashMap<EnvironmentId, ReviewFacts>>,
    /// Created lazily on the first Flatpak log line, so projects without a
    /// manifest never see the tab.
    flatpak_log: RefCell<Option<gtk::TextView>>,
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

        // New Terminal lives on the TAB BAR, at its end — the strip it adds
        // a tab to is the thing it acts on, and the platform puts "add one
        // more of these" at the end of the bar that holds them.
        //
        // Bar furniture does not graft, and that is the trap this button
        // fell into once: at the consolidated rung `Editor::graft_pages`
        // moves this pane's PAGES into the editor's strip while this tab bar
        // stays behind with the pane, so an end-action widget left here
        // would quietly leave the window at 960px. The fix is not to hide
        // the button in a page's content — it is for the rung change to
        // install it on whichever bar is hosting the family
        // (`release_new_terminal_button` / `reclaim_new_terminal_button`,
        // driven from `window.rs`'s `set_rung`).
        let new_tab_button = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New terminal in the selected environment")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        // The same way to a scrolled-off tab the editor's strip has — a
        // menu of the pages, as GNOME Builder's frames do
        // (`crate::pages_menu`): an environment with two sections and
        // a few terminals already scrolls this bar in a 700px pane. The
        // + comes first and the menu keeps the far right end, the order
        // the editor's bar has (its pencil, then its menu) — the two strips
        // stack, and their ends should read the same way (David,
        // 2026-09-06: "swap these"). In a box so the + can be handed to
        // the editor's bar at the consolidated rung without taking the
        // menu with it (`release_new_terminal_button`).
        let end_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        end_actions.append(&new_tab_button);
        end_actions.append(&crate::pages_menu::pages_menu(&tabs));
        tab_bar.set_end_action_widget(Some(&end_actions));

        // There is no header, no `⋮` menu and no Refresh here any more.
        // Every one of them was about the SELECTED environment, and the
        // backlog row in the flank is what the user selects it on: its
        // light and its second line say what the container is doing, its
        // `⋮` menu carries Rename and Nuke, and Refresh sits on that
        // panel's own header beside Start/Stop/Rebuild/Delete. This pane
        // draws the machine room and nothing about identity.

        // --- the selected environment's podman objects ---------------------
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

        // **No nested tab sets, and no pane header either.** Resources was
        // one page of an `AdwViewStack` behind an `AdwInlineViewSwitcher`
        // INSIDE one "Environment" tab, which put a second row of
        // tab-shaped controls under the first and made "which strip am I
        // in" a question the eye had to answer twice. It is a sibling of
        // the terminals now — every leaf view is a first-class tab in its
        // region's one strip.
        let resources_page = tabs.append(&resources_scroller);
        resources_page.set_title("Resources");
        resources_page.set_icon(Some(&gtk::gio::ThemedIcon::new("drive-harddisk-symbolic")));
        resources_page.set_tooltip(RESOURCES_TOOLTIP);

        let results = crate::results::ResultsPanel::new();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&tab_bar);
        widget.append(&tabs);
        widget.append(&results.widget);
        tabs.set_vexpand(true);

        let console = Rc::new(Self {
            widget,
            host: RefCell::new(tabs.clone()),
            host_watch: RefCell::new(None),
            tabs,
            resources_page: resources_page.clone(),
            results,
            search_render: RefCell::new(None),
            tab_glyphs: RefCell::new(HashMap::new()),
            search_terminals: RefCell::new(Vec::new()),
            host_shells: RefCell::new(Vec::new()),
            tab_bar: tab_bar.clone(),
            new_tab_button: new_tab_button.clone(),
            rows: RefCell::new(Vec::new()),
            selected: RefCell::new(EnvironmentId::primary()),
            git_facts: RefCell::new(HashMap::new()),
            claim_facts: RefCell::new(HashMap::new()),
            disk_facts: RefCell::new(HashMap::new()),
            published: RefCell::new(Vec::new()),
            state: RefCell::new(taste_core::state::WorkspaceState::default()),
            chat_lookup: RefCell::new(None),
            on_open_review: RefCell::new(None),
            on_close_review: RefCell::new(None),
            on_review_facts: RefCell::new(None),
            on_open_intervention: RefCell::new(None),
            on_close_intervention: RefCell::new(None),
            on_fleet_changed: RefCell::new(None),
            on_issues_changed: RefCell::new(None),
            pool: RefCell::new(PoolFacts::default()),
            on_pool_changed: RefCell::new(None),
            lifecycle: RefCell::new(HashMap::new()),
            resources_list,
            shell_tabs: RefCell::new(HashMap::new()),
            stowed_shells: RefCell::new(HashMap::new()),
            issues: RefCell::new(Vec::new()),
            review_facts: RefCell::new(HashMap::new()),
            flatpak_log: RefCell::new(None),
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
        // Resources is a permanent fixture.
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
        console.refresh_resources();
        // ...and the pane opens on the terminal it just made. It is the
        // only page here with anything in it at startup — Resources on an
        // environment nothing has built yet is an honest empty state and a
        // poor thing to open on.
        console.add_terminal_tab();
        console.refresh_environment_data(false);
        console
    }

    // --- the one query ------------------------------------------------------

    /// Answer the one query (SEARCH.md): every terminal's scrollback,
    /// listed at the pane's foot; each environment's terminal hits are
    /// counted for its backlog row through `on_inner_hits(counts, done,
    /// total)`, which also carries the scan's progress for the header's
    /// rule.
    pub fn attach_search(
        self: &Rc<Self>,
        search: &Rc<crate::search::Search>,
        on_inner_hits: impl Fn(HashMap<String, usize>, usize, usize) + 'static,
    ) {
        let on_inner_hits: Rc<dyn Fn(HashMap<String, usize>, usize, usize)> =
            Rc::new(on_inner_hits);
        {
            let weak = Rc::downgrade(self);
            let search = Rc::downgrade(search);
            let on_inner_hits = on_inner_hits.clone();
            search
                .upgrade()
                .expect("live")
                .subscribe("console", move |query, generation| {
                    let (Some(console), Some(search)) = (weak.upgrade(), search.upgrade()) else {
                        return;
                    };
                    console.answer_search(query.clone(), generation, search, on_inner_hits.clone());
                });
        }
        {
            let weak = Rc::downgrade(self);
            self.results.attach_search(search);
            search.register_placeholder(crate::search::Panel::Terminal, &self.results);
            search.register_stepper(crate::search::Panel::Terminal, move |step| {
                weak.upgrade()
                    .is_some_and(|console| console.results.step(step))
            });
        }
        {
            let weak = Rc::downgrade(self);
            let search = Rc::downgrade(search);
            let reveal: Rc<dyn Fn(&crate::results::Target)> = Rc::new(move |target| {
                let (Some(console), Some(search)) = (weak.upgrade(), search.upgrade()) else {
                    return;
                };
                console.reveal_hit(target, &search.query());
            });
            let on_select = reveal.clone();
            self.results.set_on_select(move |target| on_select(target));
            self.results.set_on_activate(move |target| reveal(target));
        }
    }

    /// Rows of scrollback an `ide_find` reads per terminal, from the end:
    /// a bounded synchronous read on the GTK thread, since the tool is
    /// answered in one turn rather than in chunks per frame.
    const FIND_ROWS_PER_TERMINAL: i64 = 5000;

    /// Hits in every terminal's scrollback — the caller's environment's,
    /// or everyone's — for `ide_find`.
    pub fn find_in_scrollback(
        &self,
        query: &crate::search::Query,
        scope: &taste_core::orchestration::FindScope,
    ) -> Vec<taste_core::orchestration::TerminalHit> {
        let host = self.host();
        let mut hits = Vec::new();
        for index in 0..host.n_pages() {
            let page = host.nth_page(index);
            let Some(terminal) = find_terminal(&page.child()) else {
                continue;
            };
            let env = self
                .shell_tabs
                .borrow()
                .values()
                .find(|(_, tab)| *tab == page)
                .map(|(env, _)| env.clone())
                .unwrap_or_else(|| self.selected.borrow().clone());
            if let taste_core::orchestration::FindScope::Environment(wanted) = scope {
                if *wanted != env {
                    continue;
                }
            }
            let Some(adjustment) = terminal.vadjustment() else {
                continue;
            };
            let hi = adjustment.upper() as i64;
            let lo = (adjustment.lower() as i64).max(hi - Self::FIND_ROWS_PER_TERMINAL);
            let text = terminal_rows(&terminal, lo, hi - 1);
            let (_, lines) = taste_core::search::search_text(&text, query, 40);
            let title = page.title().to_string();
            for (line, snippet) in lines {
                hits.push(taste_core::orchestration::TerminalHit {
                    env: env.clone(),
                    tab: title.clone(),
                    row: lo + i64::from(line) - 1,
                    text: snippet,
                });
            }
        }
        hits
    }

    /// Rows of scrollback read per frame. VTE hands text back as one
    /// string per range, so this is also the size of the string searched
    /// per step; a ten-thousand-line scrollback is twenty-five steps.
    const SCROLLBACK_CHUNK_ROWS: i64 = 400;

    /// The listing at this pane's foot is the **tab on screen's** (SEARCH.md
    /// rule 2): the selected terminal's scrollback lines, and nothing on
    /// Resources. Every terminal is still scanned, for the per-environment
    /// counts the backlog rows wear — but only the selected one is listed,
    /// and a tab change re-lists from the scan already done.
    ///
    /// The environment log is not searched here any more: it is a document
    /// in the editor's strip (`logview.rs`), and the editor answers the
    /// query for its own documents.
    fn answer_search(
        self: &Rc<Self>,
        query: crate::search::Query,
        generation: u64,
        search: Rc<crate::search::Search>,
        on_inner_hits: Rc<dyn Fn(HashMap<String, usize>, usize, usize)>,
    ) {
        use crate::results::{safe_markup, Item, Target};
        debug_assert_eq!(
            search.generation(),
            generation,
            "answering a query that is not the box's"
        );
        *self.search_render.borrow_mut() = None;
        if query.is_empty() {
            self.results.hide();
            search.report("console", crate::search::Status::default());
            search.set_panel_hits(crate::search::Panel::Terminal, 0);
            on_inner_hits(HashMap::new(), 0, 0);
            // The glyphs come back onto the tabs that wore a count.
            for (page, glyph) in self.tab_glyphs.borrow_mut().drain() {
                page.set_icon(Some(&glyph));
            }
            return;
        }
        // The terminals: every page in the strip with a VTE in it, whoever
        // it belongs to — the user's shells, the agent's, the mirrors.
        let host = self.host();
        let mut terminals: Vec<(adw::TabPage, vte4::Terminal, EnvironmentId, String)> = Vec::new();
        for index in 0..host.n_pages() {
            let page = host.nth_page(index);
            let Some(terminal) = find_terminal(&page.child()) else {
                continue;
            };
            let env = self
                .shell_tabs
                .borrow()
                .values()
                .find(|(_, tab)| *tab == page)
                .map(|(env, _)| env.clone())
                .unwrap_or_else(|| self.selected.borrow().clone());
            terminals.push((page.clone(), terminal, env, page.title().to_string()));
        }
        *self.search_terminals.borrow_mut() = terminals
            .iter()
            .map(|(page, terminal, _, _)| (page.clone(), terminal.clone()))
            .collect();
        // Each terminal's tab wears its count where its glyph was, once the
        // scan has one; the glyph is remembered to come back.
        let glyphs: Rc<Vec<Option<gtk::gio::Icon>>> = Rc::new(
            terminals
                .iter()
                .map(|(page, _, _, _)| {
                    if page
                        .icon()
                        .is_some_and(|icon| icon.is::<gtk::gdk::Texture>())
                    {
                        None // already a badge; the real glyph was kept before
                    } else {
                        page.icon()
                    }
                })
                .collect(),
        );
        {
            let mut kept = self.tab_glyphs.borrow_mut();
            for (index, (page, _, _, _)) in terminals.iter().enumerate() {
                if let Some(glyph) = &glyphs[index] {
                    kept.entry(page.clone()).or_insert_with(|| glyph.clone());
                }
            }
        }
        let ranges: Vec<(i64, i64)> = terminals
            .iter()
            .map(|(_, terminal, _, _)| match terminal.vadjustment() {
                Some(adjustment) => (adjustment.lower() as i64, adjustment.upper() as i64),
                None => (0, 0),
            })
            .collect();
        let total_rows: i64 = ranges.iter().map(|(lo, hi)| (hi - lo).max(0)).sum();
        let state = Rc::new(RefCell::new(ScrollbackScan {
            terminal: 0,
            row: ranges.first().map(|(lo, _)| *lo).unwrap_or(0),
            rows_done: 0,
            items: vec![Vec::new(); terminals.len()],
            counts: HashMap::new(),
            count: 0,
            running: !terminals.is_empty(),
            rendered_items: 0,
            rendered_at: std::time::Instant::now(),
        }));
        let weak = Rc::downgrade(self);
        let terminals = Rc::new(terminals);
        let ranges = Rc::new(ranges);
        let query = Rc::new(query);
        // What the listing shows: the tab on screen's hits, from the scan
        // so far. Kept, so a tab change re-lists without rescanning.
        let render: Rc<dyn Fn()> = {
            let weak = weak.clone();
            let search = search.clone();
            let on_inner_hits = on_inner_hits.clone();
            let terminals = terminals.clone();
            let query = query.clone();
            let state = state.clone();
            Rc::new(move || {
                let Some(console) = weak.upgrade() else {
                    return;
                };
                let scan = state.borrow();
                let selected = console.host().selected_page();
                let (subject, items) = if let Some(index) = terminals
                    .iter()
                    .position(|(page, _, _, _)| Some(page) == selected.as_ref())
                {
                    (
                        terminals[index].3.clone(),
                        scan.items.get(index).cloned().unwrap_or_default(),
                    )
                } else {
                    ("this tab".to_string(), Vec::new())
                };
                let listed = items.len();
                console.results.show(
                    &query,
                    &subject,
                    vec![crate::results::Group {
                        title: String::new(),
                        items,
                    }],
                    scan.running,
                    scan.rows_done as usize,
                    total_rows.max(1) as usize,
                );
                search.set_panel_hits(crate::search::Panel::Terminal, listed);
                search.report(
                    "console",
                    crate::search::Status {
                        hits: scan.count,
                        done: scan.rows_done as usize,
                        total: total_rows.max(1) as usize,
                        running: scan.running,
                    },
                );
                let envs = terminals.len();
                on_inner_hits(
                    scan.counts.clone(),
                    if scan.running { scan.terminal } else { envs },
                    envs,
                );
                // The tabs' badges, from the rows scanned so far.
                for (index, (page, _, _, _)) in terminals.iter().enumerate() {
                    let count = scan.items.get(index).map(Vec::len).unwrap_or(0);
                    if count > 0 {
                        page.set_icon(Some(&crate::search::badge_texture(count)));
                    } else if let Some(glyph) = console.tab_glyphs.borrow().get(page) {
                        page.set_icon(Some(glyph));
                    }
                }
            })
        };
        *self.search_render.borrow_mut() = Some(render.clone());
        render();
        if terminals.is_empty() {
            return;
        }
        // One chunk per frame, until every terminal is read or a newer
        // query has taken over.
        glib::idle_add_local(move || {
            let Some(console) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !search.is_current(generation) {
                return glib::ControlFlow::Break;
            }
            let mut scan = state.borrow_mut();
            let Some((_, terminal, env, _)) = terminals.get(scan.terminal) else {
                scan.running = false;
                drop(scan);
                render();
                return glib::ControlFlow::Break;
            };
            let (_, hi) = ranges[scan.terminal];
            if scan.row >= hi {
                scan.terminal += 1;
                scan.row = ranges.get(scan.terminal).map(|(lo, _)| *lo).unwrap_or(0);
                if scan.terminal >= terminals.len() {
                    scan.running = false;
                    drop(scan);
                    render();
                    return glib::ControlFlow::Break;
                }
                return glib::ControlFlow::Continue;
            }
            let end = (scan.row + Self::SCROLLBACK_CHUNK_ROWS).min(hi);
            let text = terminal_rows(terminal, scan.row, end - 1);
            let (count, hits) = taste_core::search::search_text(&text, &query, 40);
            if count > 0 {
                scan.count += count;
                *scan.counts.entry(env.as_str().to_string()).or_default() += count;
            }
            let page_index = scan.terminal;
            let first_row = scan.row;
            for (line, snippet) in hits {
                let items = &mut scan.items[page_index];
                if items.len() >= 300 {
                    break;
                }
                items.push(Item {
                    primary: safe_markup(&query.highlight_markup(&snippet), &snippet),
                    secondary: format!("row {}", first_row + i64::from(line)),
                    target: Target::Terminal {
                        page: page_index,
                        row: first_row + i64::from(line) - 1,
                    },
                });
            }
            scan.rows_done += end - scan.row;
            scan.row = end;
            let finished = scan.terminal + 1 >= terminals.len() && scan.row >= hi;
            scan.running = !finished;
            let listed: usize = scan.items.iter().map(Vec::len).sum();
            let stale = listed != scan.rendered_items;
            let due = scan.rendered_at.elapsed() >= std::time::Duration::from_millis(150);
            if finished || (stale && due) {
                scan.rendered_items = listed;
                scan.rendered_at = std::time::Instant::now();
                drop(scan);
                render();
            } else {
                console.results.set_progress(
                    true,
                    scan.rows_done as usize,
                    total_rows.max(1) as usize,
                );
            }
            if finished {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    /// A hit was selected in the listing: show it in the tab — the
    /// terminal scrolled to the row, with the match highlighted by VTE's
    /// own search.
    fn reveal_hit(self: &Rc<Self>, target: &crate::results::Target, query: &crate::search::Query) {
        // Terminals are the only thing this pane lists now. A hit in a log
        // is the editor's to reveal — the log is a document there.
        let crate::results::Target::Terminal { page, row } = target else {
            return;
        };
        let target = self.search_terminals.borrow().get(*page).cloned();
        let Some((tab, terminal)) = target else {
            return;
        };
        self.host().set_selected_page(&tab);
        if let Some(adjustment) = terminal.vadjustment() {
            adjustment.set_value(*row as f64);
        }
        // VTE highlights its search matches itself; this hands it the query
        // as a literal, case-folded like the box.
        const PCRE2_CASELESS: u32 = 0x0000_0008;
        const PCRE2_MULTILINE: u32 = 0x0000_0400;
        let flags = PCRE2_MULTILINE
            | if query.case_sensitive() {
                0
            } else {
                PCRE2_CASELESS
            };
        if let Ok(regex) =
            vte4::Regex::for_search(&crate::search::literal_pattern(&query.text), flags)
        {
            terminal.search_set_regex(Some(&regex), 0);
            terminal.search_set_wrap_around(true);
            terminal.search_find_next();
        }
    }

    /// The next hit in the tab on screen, wrapping: a second click on the
    /// environment's row in the backlog when the chat had none.
    pub fn step_results(&self) -> bool {
        self.results.step_cycle()
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

    /// Take the New Terminal button off this pane's tab bar, for the rung
    /// that hosts these tabs somewhere else.
    ///
    /// **Bar furniture does not graft.** `Editor::graft_pages` moves this
    /// pane's PAGES; an `AdwTabBar` action widget belongs to the bar, and
    /// this pane's bar stays behind with the pane. So the button is handed
    /// over explicitly and taken back explicitly, from the one function
    /// that knows which rung is in force (`window.rs` → `set_rung`) —
    /// which is also why this returns the widget rather than hiding it: a
    /// control that is on screen at one rung and quietly gone at the next
    /// is the bug this dance exists to prevent.
    pub fn release_new_terminal_button(&self) -> gtk::Button {
        // Only the button leaves; the pages menu beside it stays with this
        // bar, which keeps its own pages to list.
        if let Some(parent) = self.new_tab_button.parent().and_downcast::<gtk::Box>() {
            parent.remove(&self.new_tab_button);
        }
        self.new_tab_button.clone()
    }

    /// The button itself, for the caller that has to name what it is taking
    /// back off the other bar.
    pub fn new_terminal_button(&self) -> gtk::Button {
        self.new_tab_button.clone()
    }

    /// Put it back on this pane's own bar, before the pages menu.
    pub fn reclaim_new_terminal_button(&self) {
        if let Some(end_actions) = self.tab_bar.end_action_widget().and_downcast::<gtk::Box>() {
            if self.new_tab_button.parent().is_none() {
                end_actions.prepend(&self.new_tab_button);
            }
        }
    }

    /// Name the environment a new terminal would open in.
    ///
    /// The button is on a tab bar that says nothing about which environment
    /// is selected — at the consolidated rung it is the *editor's* bar,
    /// with somebody's files on it — so the tooltip carries what the button
    /// acts on. The panel's title, because the panel is what the user
    /// selected it in.
    fn set_new_terminal_tooltip(&self, row: Option<&FleetRow>) {
        let tip = match row {
            None => "New terminal in the selected environment".to_string(),
            Some(row) => {
                let name = crate::backlog::title_of(row);
                // `terminal_target` falls back to the workspace's own
                // context when a non-primary environment has nowhere to
                // run, and a tooltip that promised that environment's shell
                // would be the same attribution lie the fallback exists to
                // avoid.
                if row.primary || row.container_running() {
                    format!("New terminal in {name}")
                } else {
                    format!(
                        "New terminal for {name} — nothing is running there, \
                         so it opens where the IDE does"
                    )
                }
            }
        };
        self.new_tab_button.set_tooltip_text(Some(&tip));
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
    /// The sections are fixtures and refuse; a shell's tab
    /// closing is how the user ends it — for their own terminals that IS
    /// the kill, and for the agent's it means nothing here shows that shell
    /// any more.
    pub fn close_request(&self, view: &adw::TabView, page: &adw::TabPage) -> glib::Propagation {
        // The sections are fixtures and refuse. Every other tab here is
        // the user's own terminal, and closing it is how they end it.
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

    /// Follow the selection in whichever view holds the pages, so the
    /// results listing is the tab on screen's.
    fn watch_host(self: &Rc<Self>) {
        let host = self.host();
        if let Some((view, id)) = self.host_watch.borrow_mut().take() {
            // The old host is not ours to keep signalling: at the
            // consolidated rung it is the editor's strip, whose file tabs
            // have nothing to say about this pane.
            glib::signal_handler_disconnect(&view, id);
        }
        let weak = Rc::downgrade(self);
        let id = host.connect_selected_page_notify(move |_| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            // The listing is the tab on screen's.
            let render = console.search_render.borrow().clone();
            if let Some(render) = render {
                render();
            }
        });
        *self.host_watch.borrow_mut() = Some((host.clone(), id));
    }

    /// The pages that are this pane rather than something running in it.
    /// They never close, and they are the ones that get pinned. (There
    /// were three until 2026-09-06; the Services tab is shelved —
    /// docs/spikes/systemd-services.md — and the Environment tab was
    /// dissolved into the surfaces its facts already had.)
    fn fixtures(&self) -> [adw::TabPage; 1] {
        [self.resources_page.clone()]
    }

    fn is_fixture(&self, page: &adw::TabPage) -> bool {
        self.fixtures().contains(page)
    }

    /// Icon-only and unclosable, which `AdwTabBar` renders for exactly one
    /// kind of page: a pinned one.
    ///
    /// A fixture that never moves and never closes, ahead of the terminals,
    /// in a pane 700px wide where a few words of title are a tab's worth of
    /// room — and the same is true of it in the editor's strip at the
    /// consolidated rung, so **the pin now crosses with it**. It used to
    /// come off at the door, on the reasoning that a pinned page is forced
    /// leftmost and the panes must not sit in front of the user's files;
    /// what that produced was a row of labelled guests scrolled off the end
    /// of a 900px strip. The pinned section is its own non-scrolling box
    /// and interleaves with nothing, so the files stay together either way
    /// — see `Editor::graft`.
    ///
    /// Done explicitly rather than trusting `transfer_page` to carry or drop
    /// the flag: libadwaita's pinned state is bookkeeping in the *view*
    /// (`n_pinned_pages` and the page's position in it), not a property of
    /// the page alone, so what a transfer does with it is an implementation
    /// detail of a version. This is one call either way and no version has
    /// an opinion about it. It also stays off for the crossing itself —
    /// [`Console::begin_migration`].
    fn pin_fixtures(&self, pinned: bool) {
        let host = self.host();
        // Pinning REORDERS: libadwaita lifts the page out of the view's
        // list and reinserts it at the pinned boundary, and a list that
        // loses its selected row hands the selection to its neighbour. So
        // the selection is put back by hand afterwards, and the fixture is
        // told plainly where it goes — pinned or not, it leads this strip,
        // which is a legal position in both cases.
        let keep = host.selected_page();
        for page in self.fixtures() {
            host.set_page_pinned(&page, pinned);
        }
        // ...but only in OUR strip. Where it sits among somebody else's
        // pages is that strip's business, and an absolute position asserted
        // here would be this pane reaching into it: in the editor's strip
        // the chat's faces are pinned ahead of it, and reordering to 0 would
        // push the family that arrived first out of the way.
        if host == self.tabs {
            for (at, page) in self.fixtures().iter().enumerate() {
                host.reorder_page(page, at as i32);
            }
        }
        if let Some(keep) = keep {
            host.set_selected_page(&keep);
        }
    }

    /// About to move this pane's pages to another strip: take the pins off
    /// so the fixture crosses as an ordinary page. Paired with
    /// [`Console::set_host`], which is what ends the migration.
    pub fn begin_migration(&self) {
        self.pin_fixtures(false);
    }

    /// Say where this pane's pages now live. The caller has already moved
    /// them (an `AdwTabPage` is transferred between views, never rebuilt —
    /// a terminal's pty has to survive the crossing).
    pub fn set_host(self: &Rc<Self>, view: &adw::TabView) {
        if self.host() != *view {
            *self.host.borrow_mut() = view.clone();
            self.watch_host();
        }
        // Landed: the fixture is pinned again, in whichever strip that is.
        // It is the same unclosable, icon-only page in both — see
        // [`Console::pin_fixtures`]. The pin only ever comes off for the
        // crossing itself.
        self.pin_fixtures(true);
    }

    /// Tell the fleet how to find the chat bound to an environment, and
    /// what to do when the user opens one.
    pub fn set_chat_lookup(
        &self,
        lookup: impl Fn(&EnvironmentId) -> Option<ChatBinding> + 'static,
    ) {
        *self.chat_lookup.borrow_mut() = Some(Box::new(lookup));
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

    /// Who draws the mergedness and offers the judgment: the editor's
    /// review tabs (`editor.rs`), which are what the user is looking at
    /// when they are looking at the work.
    ///
    /// The console keeps the facts and does the merging — this is where
    /// the off-thread git passes live — and hands them over whenever the
    /// pass has re-answered.
    pub fn set_on_review_facts(&self, hook: impl Fn(&[ReviewFacts]) + 'static) {
        *self.on_review_facts.borrow_mut() = Some(Box::new(hook));
        self.announce_review_facts();
    }

    fn announce_review_facts(&self) {
        let hook = self.on_review_facts.borrow();
        let Some(hook) = hook.as_ref() else { return };
        let facts: Vec<ReviewFacts> = self.review_facts.borrow().values().cloned().collect();
        hook(&facts);
    }

    /// Where this pane's interventions are drawn: the left column's one
    /// bottom panel (`filetree.rs`).
    ///
    /// Rename, the destroy confirmation and Reject are all non-modal input
    /// surfaces, and this window has exactly one place for those. The
    /// console used to keep a second panel inside its environment tab; that
    /// tab is gone, and a modal was never an option — see the intervention
    /// convention in ARCHITECTURE.md.
    pub fn set_intervention_host(
        &self,
        open: impl Fn(&str) -> gtk::Box + 'static,
        close: impl Fn() + 'static,
    ) {
        *self.on_open_intervention.borrow_mut() = Some(Box::new(open));
        *self.on_close_intervention.borrow_mut() = Some(Box::new(close));
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
            self.refresh_env_glance();
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
        // the question this answers is "what is eating it". Named by id —
        // an environment's name is its issue's title now, and three titles
        // in one line is a paragraph where a reference was wanted.
        let mut spenders: Vec<(String, u64)> = self
            .rows
            .borrow()
            .iter()
            .filter(|row| !row.spend.is_zero())
            .map(|row| {
                let label = if row.primary {
                    crate::backlog::PRIMARY_TITLE.to_string()
                } else {
                    row.env.to_string()
                };
                (label, row.spend.tokens())
            })
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
            // happens to be the started_by of a settled issue is history, not
            // work in flight, and the panel would go on saying it was
            // working on it.
            if issue.resolution.is_resolved() {
                continue;
            }
            let Some(env) = issue
                .started_by
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

    /// Follow the selection: the two tooltips in this pane that name what
    /// the selected environment is.
    ///
    /// This was a list of every environment, then a header naming one, then
    /// a whole tab detailing one. All three are gone: the backlog row in
    /// the flank enumerates them, names them, lights them and carries their
    /// actions, and every fact this pane used to draw beside that had a
    /// better home — the log is a document, the judgment is on the review
    /// tab, the counts are on the row's tooltip. What is left here is the
    /// machine room, and the only thing it has to say about identity is
    /// *which* environment its Resources and its terminals belong to.
    fn refresh_env_glance(self: &Rc<Self>) {
        let env = self.selected.borrow().clone();
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.env == env)
            .cloned();
        // The footprint, on the tab that is about the things it is the sum
        // of — and only once podman has answered.
        self.resources_page.set_tooltip(&match &row {
            Some(row) => Self::resources_tooltip(row),
            None => RESOURCES_TOOLTIP.to_string(),
        });
        self.set_new_terminal_tooltip(row.as_ref());
    }

    /// What the Resources tab says it is for — and, once podman has been
    /// asked, how big it all is.
    ///
    /// The footprint used to be a permanent figure in a header, two slots
    /// from the container's state and in the same dim caption, which made
    /// "2.0 GiB" look like part of the sentence about what the environment
    /// was doing. It belongs to the tab that enumerates the containers,
    /// volumes and images it is the sum of.
    fn resources_tooltip(row: &FleetRow) -> String {
        match row.disk_text().as_str() {
            "—" => RESOURCES_TOOLTIP.to_string(),
            size => format!("{RESOURCES_TOOLTIP} — {size} on disk"),
        }
    }
    /// Open the review of one environment's branch: the git views aimed at
    /// its branch of record, against the branch it would be merged into.
    ///
    /// The backlog row's `⋮` menu is what asks. It replaced a banner in the
    /// console that announced the flag and carried this as its own button —
    /// a second rendering of a fact the row already draws (the accent rail
    /// and the review glyph), on a surface the user had to be looking at
    /// for it to say anything.
    pub fn open_review_for(self: &Rc<Self>, env: &EnvironmentId) {
        let facts = self.review_facts.borrow().get(env).cloned();
        if let Some(facts) = facts {
            self.open_review(&facts.branch, &facts.target);
        }
    }

    /// Merge or reject the branch a review tab is showing.
    ///
    /// The tab knows the branch; the console knows which environment that
    /// branch belongs to, and owns the git. Every one of these is
    /// USER-initiated — a button on the review tab is the only thing that
    /// presses them — which is what makes Merge's host-side libgit2 fine
    /// here.
    pub fn rule_on_review(self: &Rc<Self>, branch: &str, action: &str) {
        let found = self
            .review_facts
            .borrow()
            .iter()
            .find(|(_, facts)| facts.branch == branch)
            .map(|(env, facts)| (env.clone(), facts.clone()));
        let Some((env, facts)) = found else { return };
        match action {
            "merge" => self.clone().merge_review(env, facts),
            "reject" => self.clone().reject_intervention(&env),
            _ => {}
        }
    }

    /// Aim the git views at an environment's branch — the file tree's
    /// changed-file list over `changed_since_base`, and the diffs its rows
    /// open.
    ///
    /// The target travels with the branch. The git pass computed it to say
    /// how far ahead the work is; the list diffs against it and the tabs
    /// name it, so all three are answering with the same "in".
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
            .map(crate::backlog::title_of)
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
                .max_width_chars(40)
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

    /// The backlog header's Stop: the same action the row's own menu runs.
    pub fn stop_environment(self: &Rc<Self>, env: EnvironmentId) {
        self.run_row_action("stop", env);
    }

    /// The backlog header's Rebuild — the user applying a configuration,
    /// which is their half of the authority split.
    pub fn rebuild_environment(self: &Rc<Self>, env: EnvironmentId) {
        self.run_row_action("rebuild", env);
    }

    /// The backlog row menu's Rename: the one thing the clone directory
    /// cannot say.
    pub fn rename_environment(self: &Rc<Self>, env: EnvironmentId) {
        self.run_row_action("rename", env);
    }

    /// The backlog row menu's Nuke: container and image, so the next start
    /// rebuilds from scratch.
    pub fn nuke_environment(self: &Rc<Self>, env: EnvironmentId) {
        self.run_row_action("nuke", env);
    }

    /// The backlog header's Delete on a row with an environment: the
    /// destroy intervention, which names what the clone holds and asks.
    pub fn destroy_environment(self: &Rc<Self>, env: EnvironmentId) {
        self.run_row_action("destroy", env);
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

    /// Follow the panes. Nothing here picks an environment — the backlog
    /// does, and this is how it brings this pane along: the Resources tab
    /// and the shell tabs re-aim together, because a console listing one
    /// environment's containers over another's shells is the disagreement
    /// deleting its second listing was meant to make impossible.
    pub fn note_watching(self: &Rc<Self>, env: &EnvironmentId) {
        if *self.selected.borrow() == *env {
            return;
        }
        *self.selected.borrow_mut() = env.clone();
        self.refresh_env_glance();
        self.refresh_resources();
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
        // environment on every pass would be a revwalk per row for a
        // judgment nobody is making.
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
                drop(review_cache);
                // The review tabs redraw from this, in place: a
                // force-moved target un-merges work that was in, and a tab
                // still offering Merge for it would be the one lie this
                // lifecycle exists to avoid.
                console.announce_review_facts();
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

    // --- the selected environment's machine room -------------------------

    fn selected_supervisor(&self) -> Option<Arc<Supervisor>> {
        self.environments.get(&self.selected.borrow())
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
                .build()
                .full_text_on_hover();
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
                     names, volumes, and its socket keep using it.",
                )
                .css_classes(["caption", "dim-label"])
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(40)
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
            .max_width_chars(40)
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
                "Destroying removes the clone, the container, and this \
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
                console.announce_review_facts();
                // ...and the board's own cache, so a slug that comes round
                // again does not inherit the last tenant's verdict.
                console.workspace.review.forget(&env);
                console.disk_facts.borrow_mut().remove(&env);
                if let Some(sink) = console.lifecycle.borrow_mut().remove(&env) {
                    sink.remove();
                }
                if *console.selected.borrow() == env {
                    *console.selected.borrow_mut() = EnvironmentId::primary();
                    console.refresh_resources();
                }
                console.refresh_environment_data(false);
            }
        });
    }

    // --- interventions, in the backlog's panel ------------------------------

    /// Raise this pane's intervention under the backlog's list.
    ///
    /// A question about an environment opens under the row that is the
    /// environment (ARCHITECTURE.md → the intervention convention: the
    /// bottom of the subpanel it is about, never a modal). The console used
    /// to keep a panel inside its environment tab; there is no such tab now.
    ///
    /// With no host wired — a headless test — the widgets are built into a
    /// box nothing draws, so the flow still runs and nothing panics.
    fn open_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
        match self.on_open_intervention.borrow().as_ref() {
            Some(open) => open(title),
            None => gtk::Box::new(gtk::Orientation::Vertical, 6),
        }
    }

    fn close_intervention(&self) {
        if let Some(close) = self.on_close_intervention.borrow().as_ref() {
            close();
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
        page.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(
            "media-playback-stop-symbolic",
        )));
        page.set_indicator_tooltip(&format!(
            "{what} — the output stays until you close this tab"
        ));
    }

    /// Mirror one environment's build/startup output into its lifecycle
    /// roster row.
    ///
    /// The console keeps no log buffer of its own any more: the log is a
    /// document, opened from the tree's Logs section into the editor's
    /// strip (`logview.rs`), seeded from the supervisor's own ring and fed
    /// live by `Editor::append_log` from the same event. What stays here is
    /// the roster entry — an environment building itself is something it is
    /// running, and the roster is where the fleet says what is running.
    pub fn append_env_log(&self, env: &EnvironmentId, line: &str) {
        self.lifecycle_sink(env)
            .push(format!("{line}\n").as_bytes());
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
        // No ownership indicator: every tab in this strip is the user's
        // own terminal now. What the AGENT runs is shown in the chat, on
        // the step that ran it, and nowhere else (David, 2026-09-08: "it's
        // honestly sufficient to just have it in the chat").
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
    /// **No tab is opened for what the AGENT runs.** Its commands appear
    /// in the chat, on the step that ran them, with the output and a Kill
    /// there (David, 2026-09-08: "it's honestly sufficient to just have it
    /// in the chat"). This console's tabs are the user's own terminals,
    /// which it spawned itself; the roster still records every agent shell,
    /// because the fleet counts, `chat_status` and varlink read it.
    ///
    /// So what is left for this pass is stowing and unstowing: the
    /// environment that changed only matters to `sync_shell_tabs`, which
    /// reads the selected one off `self.selected` itself.
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
        let mut leaving: Vec<(EnvironmentId, adw::TabPage)> = Vec::new();
        for (owner, page) in self.shell_tabs.borrow().values() {
            if *owner != env && on_screen.contains(page) {
                leaving.push((owner.clone(), page.clone()));
            }
        }
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
    }

    /// The unparented view holding one environment's stowed shell tabs.
    fn holding_shell_view(&self, env: &EnvironmentId) -> adw::TabView {
        if let Some(view) = self.stowed_shells.borrow().get(env) {
            return view.clone();
        }
        adw::TabView::new()
    }

    /// TASTE_PROBE_CHECK only: bring a terminal to the front of this
    /// pane's strip.
    ///
    /// A shot of this pane should be a shot of something running in it.
    /// Aiming the panes at an environment stows the previous one's shells,
    /// and a view that loses its selected page hands the selection to its
    /// neighbour — which is Resources, whose honest answer for a
    /// fabricated environment is one row about the IDE's own container.
    /// The last terminal is the newest, which is the tab every caption
    /// about this pane is talking about.
    pub fn select_terminal_for_probe(&self) {
        let host = self.host();
        for index in (0..host.n_pages()).rev() {
            let page = host.nth_page(index);
            if find_terminal(&page.child()).is_some() {
                host.set_selected_page(&page);
                return;
            }
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
            |id: &str, title: &str, resolution, started_by: Option<&str>, age: i64, body: &str| {
                taste_git::Issue {
                    id: id.into(),
                    title: title.into(),
                    resolution,
                    reporter: "primary".into(),
                    started_by: started_by.map(str::to_string),
                    agent: None,
                    model: None,
                    created: now - age,
                    updated: now - age / 2,
                    labels: Vec::new(),
                    links: Vec::new(),
                    body: body.into(),
                    comments: Vec::new(),
                    attachments: Vec::new(),
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
                Some("david@atelier"),
                9_000,
                "Type into the prompt box, switch environments, come back: the text \
                 is gone. The pane is never destroyed, so the buffer should still be \
                 there.",
            ),
            issue(
                "i-0002",
                "Decide what a stopped environment costs",
                taste_git::Resolution::Open,
                // Started, and its environment (the fleet row with this id)
                // has flagged itself for review: the two fixtures have to
                // agree, or the row contradicts itself in one frame.
                Some("david@atelier"),
                52_000,
                "Idle-stop keeps the clone and the volumes, so the footprint does not \
                 move when a container stops — worth saying so on the row.",
            ),
            issue(
                "i-0005",
                "Serve the fleet over varlink",
                taste_git::Resolution::Open,
                Some("david@atelier"),
                30_000,
                "One socket the gadget and the shell read the same rows from, so a \
                 second renderer is never a second derivation of podman and git.",
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
                Some("david@atelier"),
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
                "i-0007",
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
                "i-0005",
                SupervisorState::Building,
                Some(("Varlink service", false, false, false)),
                EnvGit {
                    // A clone's own working branch. The branch of record
                    // is `agents/i-0005` and is not what a clone has
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
                vec![claim("i-0005", "Serve the fleet over varlink")],
            ),
            // Done, and waiting on the user. Its container is stopped
            // because flagging stops it — which is why the row's light is
            // grey and its rail is accent, and why the shot has to show
            // both at once.
            make(
                "i-0002",
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
                "i-0004",
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
                vec![claim(
                    "i-0004",
                    "Terminal tabs should keep their output after the process exits",
                )],
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
        *self.published.borrow_mut() = vec!["agents/i-0007".into(), "agents/i-0002".into()];
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
            row.pending_rebuild = row.env.as_str() == "i-0007";
        }
        // What the review band knows about the flagged one. A probe has no
        // branches to walk, so the mergedness is fabricated — and it is
        // the honest interesting case: published, ahead, and not yet in.
        if let Ok(env) = EnvironmentId::parse("i-0002") {
            self.review_facts.borrow_mut().insert(
                env,
                ReviewFacts {
                    branch: "agents/i-0002".into(),
                    target: "main".into(),
                    mergedness: Some(taste_git::Mergedness {
                        branch: "agents/i-0002".into(),
                        checked: None,
                        ahead: 6,
                        merged: false,
                        note: None,
                    }),
                },
            );
            self.announce_review_facts();
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
            observed_for: Some("i-0007".into()),
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
            name: "Personal".into(),
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

    /// The footprint rides the tab that enumerates what it is the sum of,
    /// and only once podman has answered. It used to be a permanent figure
    /// on a header line, between the container's state and the token spend,
    /// as if the three were one sentence.
    #[test]
    fn the_footprint_rides_the_tab_it_is_the_sum_of() {
        let quiet = row(ConfigAuthority::Project, running());
        assert_eq!(
            Console::resources_tooltip(&quiet),
            "This environment's containers, volumes, and images"
        );
        let mut measured = quiet.clone();
        measured.disk = Some(taste_devcontainer::DiskUsage {
            checkout_bytes: 2 * 1024 * 1024 * 1024,
            volume_bytes: 0,
            volumes_measured: 0,
            volumes_unmeasured: 0,
        });
        assert_eq!(
            Console::resources_tooltip(&measured),
            format!(
                "This environment's containers, volumes, and images — {} on disk",
                measured.disk_text()
            )
        );
    }

    /// The mergedness sentence the review tab shows, in the two shapes
    /// worth being sure of: never published, and published but not in.
    #[test]
    fn the_review_detail_says_what_is_actually_true_of_the_branch() {
        let never = ReviewFacts {
            branch: "agents/i-0002".into(),
            target: "main".into(),
            mergedness: None,
        };
        assert!(never.detail().contains("never been published"));
        // ...and there is nothing to merge, so no Merge is offered.
        assert!(!never.mergeable());

        let ahead = ReviewFacts {
            mergedness: Some(taste_git::Mergedness {
                branch: "agents/i-0002".into(),
                checked: None,
                ahead: 6,
                merged: false,
                note: None,
            }),
            ..never.clone()
        };
        assert_eq!(ahead.detail(), "agents/i-0002 → main · 6 commits ahead");
        assert!(ahead.mergeable());

        let merged = ReviewFacts {
            mergedness: Some(taste_git::Mergedness {
                branch: "agents/i-0002".into(),
                checked: None,
                ahead: 0,
                merged: true,
                note: None,
            }),
            ..never
        };
        assert_eq!(merged.detail(), "agents/i-0002 → main · already in main");
        // Work already in the target has nothing to merge, and a button
        // that would do nothing is worse than no button.
        assert!(!merged.mergeable());
    }
}
