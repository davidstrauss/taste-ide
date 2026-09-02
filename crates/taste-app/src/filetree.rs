//! Left pane: the file tree, which *is* the git interface.
//!
//! Rows show git state; hovering a changed file exposes stage/unstage; the
//! pane header carries branch, commit message entry, commit and push. There
//! is deliberately no other git UI.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::glib::BoxedAnyObject;
use taste_core::{Event, Workspace};
use taste_git::{FileState, GitWorkspace};

#[derive(Clone)]
struct FileNode {
    path: PathBuf,
    is_dir: bool,
    /// A suggested-but-not-yet-created file (allowlisted config the
    /// workspace could have). Activating it creates the real file.
    ghost: bool,
}

pub struct FileTree {
    pub widget: gtk::Box,
    workspace: Workspace,
    /// The environment whose checkout the tree and the git views are aimed
    /// at — `None` is the user's own. Watching (ENVIRONMENTS.md → "Watching
    /// an environment") points these panes at another environment's clone,
    /// **read, never edit**, by explicit action only: no chat-tab switch and
    /// no event ever moves it, and it is not persisted — a fresh IDE opens
    /// on the user's checkout.
    watching: RefCell<Option<(taste_core::environment::EnvironmentId, PathBuf)>>,
    /// The environment panel, pinned to the bottom of this pane: the one
    /// indicator of where the panes are aimed, and the way to aim them
    /// somewhere else — one row per environment, always visible (see
    /// `envstrip.rs`).
    strip: Rc<crate::envstrip::EnvPanel>,
    /// The backlog, the panel's sibling below it: the workspace's issue
    /// queue in the order the user put it in (see `backlog.rs`). Below
    /// rather than above because the environment panel names where you
    /// ARE, and the backlog is something you consult — and collapsible for
    /// the same reason.
    backlog: Rc<crate::backlog::BacklogPanel>,
    /// The project-folder row's label: it names whichever checkout is on
    /// screen.
    root_label: gtk::Label,
    git: RefCell<Option<GitWorkspace>>,
    status: Rc<RefCell<HashMap<PathBuf, FileState>>>,
    list_holder: gtk::ScrolledWindow,
    branch_label: gtk::MenuButton,
    /// The label inside it: ellipsizing, so a long branch name cannot
    /// widen this pane (see the construction site).
    branch_child: gtk::Label,
    init_button: gtk::Button,
    branch_popover: gtk::Popover,
    sync_label: gtk::Label,
    sync_button: gtk::Button,
    pull_button: gtk::Button,
    push_button: gtk::Button,
    /// A push or sync is running. Status refreshes land constantly (the
    /// watcher, every save, the agent), and one arriving mid-operation
    /// carries the counts from BEFORE it — rebuilding the row over the
    /// spinner, then correcting itself a moment later. Three states for one
    /// click. While this is set the row belongs to the spinner.
    sync_busy: std::cell::Cell<bool>,
    abort_button: gtk::Button,
    continue_button: gtk::Button,
    /// Throttle for the background fetch riding on status refreshes.
    last_fetch: std::cell::Cell<Option<std::time::Instant>>,
    commit_entry: gtk::Entry,
    search_entry: gtk::SearchEntry,
    /// While searching: show all files with non-matches ghosted, instead
    /// of matches only.
    search_ghosts_toggle: gtk::ToggleButton,
    search_view: RefCell<Option<Rc<SearchView>>>,
    /// Which file the bottom match panel is currently showing.
    intervention_file: RefCell<Option<PathBuf>>,
    /// Background search index: the workspace's searchable file list.
    index: RefCell<Option<std::sync::Arc<Vec<PathBuf>>>>,
    index_building: std::cell::Cell<bool>,
    index_bar: gtk::ProgressBar,
    /// Bottom intervention panel: non-modal input surface for dirty-file
    /// workflows; closing it cancels and gives the list its height back.
    intervention: gtk::Box,
    all_toggle: gtk::ToggleButton,
    commit_box: gtk::Box,
    ignore_rules: std::cell::Cell<usize>,
    ignored_toggle: gtk::ToggleButton,
    dirty_toggle: gtk::ToggleButton,
    staged_toggle: gtk::ToggleButton,
    stashed_toggle: gtk::ToggleButton,
    conflicts_toggle: gtk::ToggleButton,
    /// The branch these views are aimed at for review, if any.
    ///
    /// ENVIRONMENTS.md → "The review lifecycle: environments, not an
    /// inbox". This pane used to carry an Inbox filter listing every
    /// published branch; it does not, because review is a state an
    /// *environment* is in and the fleet is already the list. What
    /// survived the deletion is the half worth keeping — one branch's
    /// changed files against the merge base — and the console's review
    /// band is what aims it here.
    review: RefCell<Option<ReviewAim>>,
    /// Paths (repo-relative) touched by stash entries.
    stashed: RefCell<HashSet<PathBuf>>,
    /// Checked files in the changed list, awaiting a bulk action.
    selection: RefCell<HashSet<PathBuf>>,
    /// True while render_changed_list checks rows programmatically: the
    /// per-checkbox pane rebuild waits for the single one at the end.
    syncing_selection: std::cell::Cell<bool>,
    /// Mode the rows were last styled for (container vs safe): a flip must
    /// restyle the read-only locks even when git status is unchanged.
    container_mode: std::cell::Cell<bool>,
    /// The commit composer wrapped with its partial-selection blocker;
    /// parented into the intervention pane while the Staged view is open.
    commit_overlay: gtk::Overlay,
    commit_blocker: gtk::Box,
    /// What the intervention slot currently holds: selection-derived panes
    /// are closed/rebuilt when the list re-renders; ad-hoc flows (discard,
    /// stash, ignore, commit-confirm) are left alone.
    pane: std::cell::Cell<PaneKind>,
    /// Non-repo workspaces have nothing to diff between refreshes; this
    /// lets the unchanged-guard engage for them too.
    rendered_non_repo: std::cell::Cell<bool>,
    show_ignored: Rc<RefCell<bool>>,
    on_open: RefCell<Option<OpenCallback>>,
    /// Changed-list rows open as diffs (the editor's Changes face).
    on_open_diff: RefCell<Option<OpenDiffCallback>>,
    /// A REVIEW row opens a different diff: the branch's blob against the
    /// merge target's, which has no working-tree side and no file on disk.
    on_open_review_diff: RefCell<Option<OpenReviewDiffCallback>>,
    /// Fired when the pane leaves a review, so the tabs that review opened
    /// can go with it.
    on_review_ended: RefCell<Option<Box<dyn Fn()>>>,
    /// Routes a staged diff to the chat agent, reply → commit entry.
    commit_suggester: RefCell<Option<SuggestCallback>>,
    /// The open context menu, closed before row rebinds dispose its anchor.
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
    /// The rows currently bound in the list, keyed by absolute path, so an
    /// ordinary git-status tick can restyle the ones that moved instead of
    /// resetting the factory. See [`FileTree::restyle_changed_rows`].
    rows: RefCell<HashMap<PathBuf, RowHandle>>,
    /// Collapses the watcher's per-path event fan-out into one status query.
    refresh: RefreshGate,
}

/// A bound row, addressable after the fact.
///
/// Rebuilding a row means destroying its widgets, and destroying the widget
/// under the pointer is what made hovering a watched checkout lag: the
/// agent writes, every row is rebuilt, and prelight, tooltip and the open
/// context menu go with them — several times a second. A handle lets one
/// badge change without touching anything else.
struct RowHandle {
    is_dir: bool,
    /// The state the row is currently PAINTED as, which is what a restyle
    /// diffs against — not what the status map said at any other moment.
    state: std::cell::Cell<FileState>,
    /// The row carries a lock. Its dim-label is the lock's, not the Ignored
    /// state's, so leaving Ignored must not un-dim it.
    locked: bool,
    label: glib::WeakRef<gtk::Label>,
    badge: glib::WeakRef<gtk::Label>,
}

/// How long a refresh request waits for company.
///
/// A burst of agent writes reaches the tree as one `FileChanged` per path —
/// dozens of them for a single edit round — and each used to run its own
/// `git status` and repaint the list. Short enough that staging a file
/// still feels instant.
const REFRESH_COALESCE: std::time::Duration = std::time::Duration::from_millis(120);

/// What the find-in-project entry says when it is idle.
///
/// Named because the indexer borrows the same slot to report progress and
/// has to be able to put it back — see `rebuild_index`.
const SEARCH_PLACEHOLDER: &str = "Find in project";

/// The refresh coalescer: at most one armed timer, at most one query in
/// flight, and at most one trailing re-run behind it.
///
/// Queries can outlive their own window on a big checkout, so requests
/// arriving during one are not dropped (the status would go stale) and not
/// stacked either (they would queue behind each other for ever) — they
/// collapse into a single re-run when it lands.
#[derive(Default)]
struct RefreshGate {
    armed: std::cell::Cell<bool>,
    inflight: std::cell::Cell<bool>,
    trailing: std::cell::Cell<bool>,
}

impl RefreshGate {
    /// A refresh was asked for. `true` means arm the timer.
    fn request(&self) -> bool {
        if self.inflight.get() {
            self.trailing.set(true);
            return false;
        }
        !self.armed.replace(true)
    }

    /// The timer fired. `true` means run the query now.
    fn fire(&self) -> bool {
        self.armed.set(false);
        if self.inflight.get() {
            self.trailing.set(true);
            return false;
        }
        self.inflight.set(true);
        true
    }

    /// The query finished, applied or not. `true` means something asked for
    /// another one while it ran.
    fn finish(&self) -> bool {
        self.inflight.set(false);
        self.trailing.replace(false)
    }
}

/// What the pane is reviewing: one environment's branch of record, and the
/// branch it would be merged into.
///
/// The target is carried rather than re-derived because it is what the diff
/// is *against* and what the tabs say they are comparing. The console
/// computed it (`ReviewFacts`) to render the band; deriving it a second
/// time here is how the band and the diff would eventually disagree about
/// what "in" means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAim {
    pub branch: String,
    pub target: String,
}

type OpenCallback = Box<dyn Fn(PathBuf, Option<u32>)>;
type OpenDiffCallback = Box<dyn Fn(PathBuf)>;
/// Repository-relative path, the branch under review, the branch it is read
/// against.
type OpenReviewDiffCallback = Box<dyn Fn(PathBuf, String, String)>;

/// What occupies the bottom intervention slot.
#[derive(Clone, Copy, PartialEq)]
enum PaneKind {
    None,
    /// A one-shot flow: discard/stash/ignore confirmation, commit-message
    /// confirmation. Survives list re-renders.
    Adhoc,
    /// The bulk-ops pane for the checked files (Dirty/Stashed views).
    Selection,
    /// The Staged view's resting pane: bulk ops + the commit composer.
    Staged,
}
type SuggestCallback = Box<dyn Fn(String, Box<dyn FnOnce(String)>)>;

/// Everything the header + row badges need, computed off the main thread.
struct StatusSnapshot {
    status: HashMap<PathBuf, FileState>,
    stashed: HashSet<PathBuf>,
    /// Count of .gitignore RULES (not ignored files — counting those
    /// would need a full walk).
    ignore_rules: usize,
    branch: Option<String>,
    sync: Option<taste_git::SyncStatus>,
    rebasing: bool,
}

/// How long a fetch, rebase or push may run before it is called failed. A
/// hung network step must not leave the sync row spinning for ever.
const GIT_NETWORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run one git step, bounded. `Err` carries something worth putting in a
/// toast.
async fn run_git_step(program: String, args: Vec<String>) -> Result<std::process::Output, String> {
    let run = tokio::process::Command::new(&program).args(&args).output();
    match tokio::time::timeout(GIT_NETWORK_TIMEOUT, run).await {
        Ok(Ok(out)) if out.status.success() => Ok(out),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("failed")
            .to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "timed out after {}s",
            GIT_NETWORK_TIMEOUT.as_secs()
        )),
    }
}

/// A commit time as a review row wants it: coarse, short, and never a
/// timestamp the reader has to subtract from today's date. Shared with the
/// issue queue, which reads ages the same way for the same reason.
pub(crate) fn relative_age(unix_seconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix_seconds);
    let elapsed = now.saturating_sub(unix_seconds);
    match elapsed {
        // A clock skew (or a commit from a container an hour ahead) reads
        // as "now" rather than as a negative age.
        i64::MIN..=59 => "now".to_string(),
        60..=3599 => format!("{}m", elapsed / 60),
        3600..=86_399 => format!("{}h", elapsed / 3600),
        86_400..=2_591_999 => format!("{}d", elapsed / 86_400),
        _ => format!("{}w", elapsed / 604_800),
    }
}

/// Swap a button's content for a running spinner until the operation ends
/// (set_label replaces the child).
fn button_busy(button: &gtk::Button) {
    // Freeze the allocated width first: the spinner must not reflow the
    // row it sits in.
    button.set_width_request(button.width());
    let spinner = gtk::Spinner::new();
    spinner.start();
    button.set_child(Some(&spinner));
    button.set_sensitive(false);
}

impl FileTree {
    pub fn new(workspace: Workspace) -> Rc<Self> {
        // The branch is a dropdown: switch to any local branch, or type a
        // name to create one.
        //
        // Its label ellipsizes and is capped, because a branch name is
        // arbitrary text from the repository and this pane's natural width
        // is load-bearing: an unbounded label lets
        // `release/2026-q3-migration-step-two` widen the file tree and take
        // the room from the editor — and the pane's minimum is what decides
        // whether GNOME will tile this window at all. The full name is in
        // the tooltip and in the dropdown.
        // Both bounds, and both matter: the maximum stops a long name
        // widening the pane, and the minimum stops an ellipsizing label
        // giving up all its width and collapsing to "w…49".
        let branch_child = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .width_chars(14)
            .max_width_chars(22)
            .xalign(0.0)
            .build();
        let branch_label = gtk::MenuButton::builder()
            .css_classes(["flat"])
            .direction(gtk::ArrowType::Down)
            .child(&branch_child)
            .build();
        let commit_entry = gtk::Entry::builder()
            .placeholder_text("Commit message")
            .hexpand(true)
            .css_classes(["flat-entry"])
            .build();
        let commit_button = gtk::Button::builder()
            .icon_name("object-select-symbolic")
            .tooltip_text("Commit staged changes")
            .css_classes(["flat", "circular"])
            .build();
        let push_button = gtk::Button::builder()
            .label("↑ 0")
            .tooltip_text("Push commits to the remote")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let pull_button = gtk::Button::builder()
            .label("↓ 0")
            .tooltip_text("Pull: fetch, then rebase onto the remote tip")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let ignored_toggle = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .tooltip_text("Show ignored files")
            .css_classes(["flat"])
            .build();
        // Filters in change-flow order: Stashed ↔ Dirty ↔ Staged, with
        // All (no git filter) leading. Counts live on the buttons.
        let all_toggle = gtk::ToggleButton::builder()
            .label("All")
            .tooltip_text("Every unignored file")
            .css_classes(["flat", "caption"])
            .active(true)
            .build();
        let stashed_toggle = gtk::ToggleButton::builder()
            .label("Stashed")
            .tooltip_text("Files touched by stash entries")
            .css_classes(["flat", "caption"])
            .build();
        let dirty_toggle = gtk::ToggleButton::builder()
            .label("Dirty")
            .tooltip_text("Unstaged changes and untracked files")
            .css_classes(["flat", "caption"])
            .build();
        let staged_toggle = gtk::ToggleButton::builder()
            .label("Staged")
            .tooltip_text("Files staged for the next commit")
            .css_classes(["flat", "caption"])
            .build();
        // Off the pipeline, action-required: appears only while conflicts
        // exist (a paused rebase, an agent's merge) and hides again after.
        let conflicts_toggle = gtk::ToggleButton::builder()
            .label("Conflicts")
            .tooltip_text("Files with unresolved conflicts")
            .css_classes(["flat", "caption", "error"])
            .visible(false)
            .build();
        // search_delay debounces keystrokes; run_search additionally drops
        // stale results, so typing never staggers the UI.
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(SEARCH_PLACEHOLDER)
            .search_delay(200)
            .build();
        let search_ghosts_toggle = gtk::ToggleButton::builder()
            .icon_name("taste-ghost-symbolic")
            .tooltip_text("Show all files, ghosting non-matches")
            .css_classes(["flat"])
            .sensitive(false)
            .build();

        let suggest_button = gtk::Button::builder()
            .icon_name("starred-symbolic")
            .tooltip_text("Ask the AI to suggest a commit message for the staged changes")
            .css_classes(["flat", "circular"])
            .build();

        // The shared composer widget: AI spark left, message field
        // center, commit checkmark right.
        let commit_row = crate::composer::Composer::new(
            &suggest_button,
            &commit_entry,
            &[commit_button.clone().upcast()],
        )
        .widget;
        // The composer lives in the Staged view's bottom pane, grayed under
        // this small banner whenever a staged file is unchecked (a commit
        // takes the whole index — partial selections can't commit).
        let commit_blocker = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["osd", "toolbar"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        commit_blocker.append(
            &gtk::Label::builder()
                .label(
                    "A commit takes every staged file — select them all, \
                     or unstage the ones to leave out",
                )
                .css_classes(["caption"])
                .build(),
        );
        let commit_overlay = gtk::Overlay::new();
        commit_overlay.set_child(Some(&commit_row));
        commit_overlay.add_overlay(&commit_blocker);

        // The sync tool: fetch (read-only remote op) + rebase onto the
        // remote tip. This — not merge-pulls — is how local work meets the
        // remote in taste-ide.
        let sync_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        let sync_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Fetch the remote (refreshes the counts)")
            .css_classes(["flat"])
            .build();
        let abort_button = gtk::Button::builder()
            .label("Abort Rebase")
            .tooltip_text("Give up: put everything back the way it was before the sync")
            .css_classes(["destructive-action"])
            .visible(false)
            .build();
        let continue_button = gtk::Button::builder()
            .label("Continue Rebase")
            .tooltip_text("Resume once every conflict is resolved and marked")
            .css_classes(["suggested-action"])
            .visible(false)
            .build();
        // Not a repo: one honest action instead of inert git chrome.
        let init_button = gtk::Button::builder()
            .label("Initialize Repository")
            .css_classes(["suggested-action"])
            .visible(false)
            .hexpand(true)
            .build();
        // The branch dropdown lives with its consequences: pull/push
        // counts and the fetch button share this row.
        branch_label.set_halign(gtk::Align::Start);
        branch_label.set_hexpand(true);
        let sync_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let git_glyph = gtk::Image::from_icon_name("taste-branch-symbolic");
        git_glyph.add_css_class("dim-label");
        sync_row.append(&git_glyph);
        sync_row.append(&branch_label);
        sync_row.append(&sync_label);
        sync_row.append(&abort_button);
        sync_row.append(&continue_button);
        sync_row.append(&init_button);
        sync_row.append(&pull_button);
        sync_row.append(&push_button);
        sync_row.append(&sync_button);

        // Which environment the panes are aimed at is said once, by the
        // panel at the bottom of this pane — not by a bar that appears in
        // the header and pushes the tree down when it does.
        let strip = crate::envstrip::EnvPanel::new(workspace.activity.clone());
        // ...and the backlog under it. Workspace-scoped where the panel
        // above is environment-scoped, which is why it reads the main
        // checkout's root rather than whatever the panes are aimed at: the
        // queue lives on one ref for the whole workspace, and watching an
        // environment does not change whose backlog this is.
        let backlog = crate::backlog::BacklogPanel::new(workspace.root().to_path_buf());

        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(12);
        header.set_margin_end(12);
        let branch_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let branch_popover = gtk::Popover::new();
        branch_label.set_popover(Some(&branch_popover));
        let filter_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        // One state at a time: the git filters are radio-grouped with All,
        // so exactly one is ever active and All can't turn off into nothing.
        stashed_toggle.set_group(Some(&all_toggle));
        dirty_toggle.set_group(Some(&all_toggle));
        staged_toggle.set_group(Some(&all_toggle));
        conflicts_toggle.set_group(Some(&all_toggle));
        filter_box.append(&all_toggle);
        filter_box.append(&stashed_toggle);
        filter_box.append(&dirty_toggle);
        filter_box.append(&staged_toggle);
        filter_box.append(&conflicts_toggle);
        // A sixth toggle would not fit beside the eye, and the eye never
        // belonged here: hiding ignored files is a *listing* choice, like
        // ghosting search non-matches. It moves up beside its twin, and the
        // filter group gets the row to itself (ROADMAP: crowded header).
        branch_row.append(&filter_box);
        // Index progress, as a hairline pulsing along the BOTTOM EDGE of
        // the search entry — never as text.
        //
        // It used to be a `.osd` progress bar with `show-text`, centred
        // over the entry. An OSD trough is translucent and a progress
        // bar's label sits on its own centre line, which is exactly where
        // the entry draws its placeholder: "Indexing…" and "Find in
        // project" rendered on top of each other, one string legibly
        // ruining the other. Two widgets cannot share one baseline.
        //
        // So the words move into the entry — `rebuild_index` says
        // "Indexing… n files" in the placeholder, the one string slot
        // that already exists — and what stays on the overlay is a line
        // with no text in it at all. Since a placeholder is drawn only
        // while the entry is empty, a typed query replaces the progress
        // wording instead of colliding with it, by construction.
        let index_bar = gtk::ProgressBar::builder()
            .show_text(false)
            .visible(false)
            .can_target(false)
            // Bottom edge, inset so the line stays inside the entry's
            // rounded corners rather than running out of them.
            .valign(gtk::Align::End)
            .margin_bottom(3)
            .margin_start(9)
            .margin_end(9)
            .hexpand(true)
            .css_classes(["index-bar"])
            .build();
        let search_overlay = gtk::Overlay::new();
        search_overlay.set_child(Some(&search_entry));
        search_overlay.add_overlay(&index_bar);
        // Section one: version control (branch, counts, commit).
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        search_overlay.set_hexpand(true);
        search_row.append(&search_overlay);
        search_row.append(&search_ghosts_toggle);
        search_row.append(&ignored_toggle);
        header.append(&sync_row);
        let section_break = gtk::Separator::new(gtk::Orientation::Horizontal);
        section_break.set_margin_top(4);
        section_break.set_margin_bottom(4);
        header.append(&section_break);
        // Section two: finding and filtering files, right above the tree
        // they act on.
        header.append(&search_row);
        header.append(&branch_row);

        let list_holder = gtk::ScrolledWindow::builder().vexpand(true).build();

        // Dirty-file workflows that need input steal space from the
        // bottom of the file list — never a modal dialog.
        let intervention = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(6)
            .visible(false)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_width_request(180);
        widget.append(&header);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        // The project folder itself: always the first row, never
        // collapsible; its context menu creates top-level items.
        let root_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root_row.set_margin_top(4);
        root_row.set_margin_bottom(4);
        root_row.set_margin_start(12);
        root_row.set_margin_end(12);
        root_row.append(&gtk::Image::from_icon_name("folder-open-symbolic"));
        let root_label = gtk::Label::builder()
            .label(
                workspace
                    .root()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
            .css_classes(["heading"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        root_row.append(&root_label);
        widget.append(&root_row);
        widget.append(&list_holder);
        widget.append(&intervention);
        // Last, and permanent: the environment panel sits below everything
        // else this pane can open, including the intervention panel, so
        // the context it names is never the thing that gets displaced.
        widget.append(&strip.widget);
        // ...and the backlog under it, the one thing in this pane that is
        // allowed below the panel. It earns the place by being the panel's
        // other half: a row up there says what an environment is working
        // on, a row down here says which environment claimed it, and the
        // selection moves between them. It folds away; the panel does not.
        widget.append(&backlog.widget);

        let tree = Rc::new(Self {
            widget,
            workspace: workspace.clone(),
            watching: RefCell::new(None),
            strip: strip.clone(),
            backlog: backlog.clone(),
            root_label,
            git: RefCell::new(GitWorkspace::discover(workspace.root())),
            status: Rc::new(RefCell::new(HashMap::new())),
            list_holder,
            intervention: intervention.clone(),
            branch_label,
            branch_child,
            init_button: init_button.clone(),
            branch_popover: branch_popover.clone(),
            sync_label,
            sync_button: sync_button.clone(),
            pull_button: pull_button.clone(),
            push_button: push_button.clone(),
            sync_busy: std::cell::Cell::new(false),
            abort_button: abort_button.clone(),
            continue_button: continue_button.clone(),
            last_fetch: std::cell::Cell::new(None),
            conflicts_toggle: conflicts_toggle.clone(),
            commit_entry,
            search_entry: search_entry.clone(),
            search_ghosts_toggle: search_ghosts_toggle.clone(),
            search_view: RefCell::new(None),
            intervention_file: RefCell::new(None),
            index: RefCell::new(None),
            index_building: std::cell::Cell::new(false),
            index_bar: index_bar.clone(),
            all_toggle: all_toggle.clone(),
            commit_box: commit_row.clone(),
            ignore_rules: std::cell::Cell::new(0),
            ignored_toggle: ignored_toggle.clone(),
            dirty_toggle: dirty_toggle.clone(),
            staged_toggle: staged_toggle.clone(),
            stashed_toggle: stashed_toggle.clone(),
            review: RefCell::new(None),
            on_open_review_diff: RefCell::new(None),
            on_review_ended: RefCell::new(None),
            stashed: RefCell::new(HashSet::new()),
            selection: RefCell::new(HashSet::new()),
            syncing_selection: std::cell::Cell::new(false),
            container_mode: std::cell::Cell::new(workspace.exec.is_container()),
            commit_overlay,
            commit_blocker,
            pane: std::cell::Cell::new(PaneKind::None),
            rendered_non_repo: std::cell::Cell::new(false),
            show_ignored: Rc::new(RefCell::new(false)),
            on_open: RefCell::new(None),
            on_open_diff: RefCell::new(None),
            commit_suggester: RefCell::new(None),
            open_menu: RefCell::new(None),
            rows: RefCell::new(HashMap::new()),
            refresh: RefreshGate::default(),
        });

        let weak = Rc::downgrade(&tree);
        commit_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
                tree.commit();
            }
        });
        let weak = Rc::downgrade(&tree);
        {
            let weak = Rc::downgrade(&tree);
            init_button.connect_clicked(move |button| {
                let Some(tree) = weak.upgrade() else { return };
                button_busy(button);
                let root = tree.workspace.root().to_path_buf();
                let events = tree.workspace.events.clone();
                let weak = Rc::downgrade(&tree);
                glib::spawn_future_local(async move {
                    let handle = crate::runtime::runtime()
                        .spawn_blocking(move || taste_git::GitWorkspace::init(&root));
                    match handle.await {
                        Ok(Ok(())) => {
                            events.publish(Event::Toast("Repository initialized".into()));
                        }
                        Ok(Err(e)) => events.publish(Event::Toast(format!("git init: {e}"))),
                        Err(_) => {}
                    }
                    // Success or not, re-discover: apply_status restores
                    // the button (or swaps in the git interface).
                    if let Some(tree) = weak.upgrade() {
                        *tree.git.borrow_mut() =
                            taste_git::GitWorkspace::discover(tree.workspace.root());
                        tree.refresh_status();
                        tree.rebuild();
                    }
                });
            });
        }
        push_button.connect_clicked(move |button| {
            if let Some(tree) = weak.upgrade() {
                tree.begin_sync_op(button);
                tree.push();
            }
        });
        let weak = Rc::downgrade(&tree);
        let weak_branches = Rc::downgrade(&tree);
        branch_popover.connect_show(move |_| {
            if let Some(tree) = weak_branches.upgrade() {
                tree.populate_branch_menu();
            }
        });
        {
            let context = gtk::GestureClick::builder().button(3).build();
            let weak = Rc::downgrade(&tree);
            let row_anchor = root_row.clone();
            let root_node = FileNode {
                path: tree.workspace.root().to_path_buf(),
                is_dir: true,
                ghost: false,
            };
            context.connect_released(move |_, _, _, _| {
                if let Some(tree) = weak.upgrade() {
                    tree.show_context_menu(&row_anchor, &root_node);
                }
            });
            root_row.add_controller(context);
        }
        let weak_pull = Rc::downgrade(&tree);
        pull_button.connect_clicked(move |button| {
            if let Some(tree) = weak_pull.upgrade() {
                tree.begin_sync_op(button);
                tree.sync();
            }
        });
        sync_button.connect_clicked(move |button| {
            if let Some(tree) = weak.upgrade() {
                tree.begin_sync_op(button);
                tree.sync();
            }
        });
        let weak = Rc::downgrade(&tree);
        abort_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
                tree.abort_rebase();
            }
        });
        let weak = Rc::downgrade(&tree);
        continue_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
                tree.continue_rebase();
            }
        });
        let weak = Rc::downgrade(&tree);
        search_entry.connect_search_changed(move |entry| {
            let Some(tree) = weak.upgrade() else { return };
            let query = entry.text().to_string();
            if query.trim().is_empty() {
                *tree.search_view.borrow_mut() = None;
                tree.search_ghosts_toggle.set_sensitive(false);
                if tree.filters_active() {
                    tree.render_filter_view();
                } else {
                    tree.rebuild();
                }
            } else {
                tree.search_ghosts_toggle.set_sensitive(true);
                tree.run_search(query);
            }
        });
        let weak = Rc::downgrade(&tree);
        search_ghosts_toggle.connect_toggled(move |_| {
            let Some(tree) = weak.upgrade() else { return };
            let query = tree.search_entry.text().trim().to_string();
            if !query.is_empty() {
                tree.run_search(query);
            }
        });
        let weak = Rc::downgrade(&tree);
        suggest_button.connect_clicked(move |button| {
            let Some(tree) = weak.upgrade() else { return };
            tree.suggest_commit_message(button.clone());
        });
        // Radio semantics: each click fires toggled twice (the member
        // leaving and the member entering); only the newly-active member
        // drives the update.
        for toggle in [
            &dirty_toggle,
            &staged_toggle,
            &stashed_toggle,
            &conflicts_toggle,
        ] {
            let weak = Rc::downgrade(&tree);
            toggle.connect_toggled(move |toggle| {
                let Some(tree) = weak.upgrade() else { return };
                if !toggle.is_active() {
                    return;
                }
                tree.selection.borrow_mut().clear();
                // A filter is a different question from a review: entering
                // one drops the branch the review band aimed these views
                // at, rather than leaving a stale header over the wrong
                // list.
                tree.leave_review();
                tree.sync_filter_counts();
                tree.search_entry.set_text("");
                tree.render_filter_view();
            });
        }
        {
            let weak = Rc::downgrade(&tree);
            all_toggle.connect_toggled(move |toggle| {
                let Some(tree) = weak.upgrade() else { return };
                if !toggle.is_active() {
                    return;
                }
                tree.selection.borrow_mut().clear();
                tree.leave_review();
                tree.close_intervention();
                // Same rule as the git filters: entering a view resets the
                // search, whichever radio member was hit.
                tree.search_entry.set_text("");
                tree.rebuild();
            });
        }
        let weak = Rc::downgrade(&tree);
        ignored_toggle.connect_toggled(move |button| {
            if let Some(tree) = weak.upgrade() {
                *tree.show_ignored.borrow_mut() = button.is_active();
                // Never clobber active search results — or a filter view
                // (which never lists ignored files anyway) — with the
                // tree; the flag applies when the tree next shows.
                if tree.search_entry.text().trim().is_empty() && !tree.filters_active() {
                    tree.rebuild();
                }
            }
        });

        // The panel does not aim the panes itself: the window owns that
        // transition, because it also drops the environment's watcher and
        // re-aims the editor, and two places deciding what "watching"
        // means is how they come to disagree. The panel's own current-view
        // marker follows from `aim_at`, which the window calls back into.
        tree.strip.set_current(None);

        tree.refresh_status();
        tree.rebuild();
        tree.rebuild_index();
        tree
    }

    /// What the tree is looking at: the watched environment's clone, or the
    /// user's own checkout.
    fn view_root(&self) -> PathBuf {
        match self.watching.borrow().as_ref() {
            Some((_, root)) => root.clone(),
            None => self.workspace.root().to_path_buf(),
        }
    }

    /// Whether what is on screen belongs to someone else. Non-primary
    /// environments are read-only to the user — the intervention path is
    /// reviewing a published branch or taking over the chat, never editing
    /// under a running agent.
    fn read_only(&self) -> bool {
        self.watching.borrow().is_some()
    }

    /// Aim the tree and the git views at an environment's checkout, or
    /// (with `None`) back at the user's own.
    ///
    /// The active FILTER is deliberately kept: the Dirty view over an
    /// agent's clone is a live review of work in progress, and having it
    /// reset on arrival would be the opposite of what watching is for. The
    /// search, the selections and any open panel do go — they were about
    /// the other checkout.
    pub fn aim_at(
        self: &Rc<Self>,
        target: Option<(taste_core::environment::EnvironmentId, PathBuf)>,
    ) {
        if *self.watching.borrow() == target {
            return;
        }
        self.close_open_menu();
        *self.watching.borrow_mut() = target;
        let root = self.view_root();
        *self.git.borrow_mut() = GitWorkspace::discover(&root);
        self.selection.borrow_mut().clear();
        // A review belongs to the checkout it was opened against, so
        // aiming the panes elsewhere leaves it rather than showing one
        // environment's branch over another's files.
        self.leave_review();
        self.close_intervention();
        self.search_entry.set_text("");
        *self.search_view.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.stashed.borrow_mut().clear();
        // Force the next status snapshot through the unchanged-guard: the
        // maps above are empty now, so anything the new checkout has is a
        // change, and an EMPTY one still has to repaint the rows.
        self.rendered_non_repo.set(false);
        // The panel is the indicator, and the only one: it says where the
        // panes are aimed, tints itself when that is not home, and holds
        // the way back. The root row goes back to naming the project.
        self.strip.set_current(self.watching());
        self.root_label.set_label(
            &self
                .workspace
                .root()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        self.apply_view_permissions();
        self.refresh_status();
        if self.filters_active() {
            self.render_filter_view();
        } else {
            self.rebuild();
        }
        self.rebuild_index();
    }

    /// Set the branch shown in the header, with the full name in the
    /// tooltip: the label ellipsizes, so the tooltip is where a long name
    /// stays readable.
    fn set_branch_label(&self, branch: &str) {
        self.branch_child.set_label(branch);
        self.branch_child.set_tooltip_text(Some(branch));
    }

    /// Which environment the panes are aimed at (`None` = the primary).
    pub fn watching(&self) -> Option<taste_core::environment::EnvironmentId> {
        self.watching.borrow().as_ref().map(|(env, _)| env.clone())
    }

    /// Aim the git views at one environment's branch of record: its
    /// changed files against the merge base with the current branch, rows
    /// opening as diffs like every other changed list in this pane.
    ///
    /// This is the console review band's Open Review, and it is all that
    /// the deleted Inbox filter is replaced by *here*. The band asks the
    /// judgment questions — how far ahead, already merged, merge or reject
    /// — because those are questions about an environment; this pane
    /// answers the only one that is about files.
    pub fn open_review(self: &Rc<Self>, branch: String, target: String) {
        // Whatever filter was on stays on underneath: a review is a view
        // of its own and replaces the list, rather than joining the radio
        // group as a sixth state to get out of.
        let aim = ReviewAim { branch, target };
        *self.review.borrow_mut() = Some(aim.clone());
        self.close_intervention();
        self.render_review_files(aim);
    }

    /// Leave the review, if there is one, and tell whoever cares.
    ///
    /// Every way out goes through here — the Close Review row, entering a
    /// filter, aiming the panes elsewhere, and the console settling the
    /// environment — because the review's *tabs* have to leave with it, and
    /// a second way out would be the one that forgot to close them.
    fn leave_review(&self) -> bool {
        if self.review.borrow_mut().take().is_none() {
            return false;
        }
        if let Some(hook) = self.on_review_ended.borrow().as_ref() {
            hook();
        }
        true
    }

    /// Leave the review and repaint — the console's way out, for when
    /// merging or rejecting has settled the environment being reviewed.
    pub fn close_review(self: &Rc<Self>) {
        if !self.leave_review() {
            return;
        }
        self.close_intervention();
        if self.filters_active() {
            self.render_filter_view();
        } else {
            self.rebuild();
        }
    }

    /// Where the panel sends the panes. One hook for every destination —
    /// the primary included, because "back to yours" is the primary's row
    /// and not a second kind of action.
    pub fn set_on_open_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.strip.set_on_select(hook);
    }

    /// The panel header's + button, mirrored from the fleet view's own:
    /// the same call, so there is still one way an environment is made.
    pub fn set_on_new_environment(&self, hook: impl Fn(gtk::Button) + 'static) {
        self.strip.set_on_new_environment(hook);
    }

    /// Called on the panel's own tick, so a list that is always on screen
    /// says what is true now rather than what was true when something last
    /// moved.
    pub fn set_on_strip_refresh(&self, hook: impl Fn() + 'static) {
        self.strip.set_on_refresh(hook);
    }

    /// The assembled fleet: the panel's rows, their lights, their names and
    /// what each is working on — and the backlog's assignee lookup, which
    /// resolves a slug through these same rows so the queue's tooltip and
    /// the panel cannot disagree about what an environment is called.
    pub fn set_fleet(&self, rows: &[crate::fleet::FleetRow]) {
        self.strip.set_rows(rows);
        self.backlog.set_fleet(rows);
    }

    /// The workspace's issue queue, in the ref's own order. Read by the
    /// console (which is where the off-thread git passes live) and handed
    /// here, so there is one read of `refs/taste/issues` per change and not
    /// one per surface that renders it.
    pub fn set_issues(&self, issues: &[taste_git::Issue]) {
        self.backlog.set_issues(issues);
    }

    /// Asked for after the backlog writes to the issues ref: the write is
    /// optimistic on screen, and this is what makes it true.
    pub fn set_on_backlog_changed(&self, hook: impl Fn() + 'static) {
        self.backlog.set_on_refresh(hook);
    }

    /// How a refused issue write reaches the user — the window's own toast,
    /// like every other action outcome.
    pub fn set_on_backlog_error(&self, hook: impl Fn(String) + 'static) {
        self.backlog.set_on_toast(hook);
    }

    /// The subscription pool those rows all spend out of, for the gauge
    /// in the panel's header.
    pub fn set_quota(&self, snapshot: &taste_core::quota::QuotaSnapshot) {
        self.strip.set_quota(snapshot);
    }

    /// TASTE_PROBE_CHECK only: plant fabricated activity windows on the
    /// environment panel's rows, so a headless shot has sparklines in it.
    /// Paired with the console's `seed_fleet_for_probe`, which is what put
    /// those rows there.
    pub fn seed_activity_for_probe(&self, shapes: &[(&str, crate::envstrip::Shape)]) {
        for (slug, shape) in shapes {
            if let Ok(env) = taste_core::environment::EnvironmentId::parse(slug) {
                self.strip.seed_activity_for_probe(&env, *shape);
            }
        }
    }

    /// Hand the environment panel and the backlog to somebody else — the
    /// gadget, below the breakpoint — in the order they sit in here.
    ///
    /// Reparenting, not rebuilding. The panels keep their scroll position,
    /// their filter text, their sparkline history and their selection
    /// because the widgets are never taken apart; crossing the breakpoint
    /// costs two `remove` calls and nothing else. This is the same trick
    /// the editor uses to stow a tab set when the selection moves, for the
    /// same reason.
    pub fn stow_panels(&self) -> Vec<gtk::Widget> {
        let panels: Vec<gtk::Widget> = vec![
            self.strip.widget.clone().upcast(),
            self.backlog.widget.clone().upcast(),
        ];
        for panel in &panels {
            if panel.parent().as_ref() == Some(self.widget.upcast_ref::<gtk::Widget>()) {
                self.widget.remove(panel);
            }
        }
        panels
    }

    /// Take them back, at the bottom where they belong — the environment
    /// panel below everything this pane can open, and the backlog below
    /// that. The exact inverse of [`FileTree::stow_panels`], because
    /// "stretch back to the IDE, nothing rearranged" is a commitment.
    pub fn restore_panels(&self, panels: Vec<gtk::Widget>) {
        for panel in panels {
            if panel.parent().is_none() {
                self.widget.append(&panel);
            }
        }
    }

    /// TASTE_PROBE_CHECK only: fold the backlog away, for the shots that
    /// are about something above it.
    pub fn set_backlog_expanded(&self, expanded: bool) {
        self.backlog.set_expanded(expanded);
    }

    /// TASTE_PROBE_CHECK only: draw one backlog row's actions as if the
    /// pointer were on it, so a still frame can show what the rows do.
    pub fn seed_backlog_actions_for_probe(&self, id: &str) {
        self.backlog.seed_actions_for_probe(id);
    }

    /// Put the keyboard in the environment panel (Ctrl+Shift+E). Nothing
    /// opens — the list is already there — so this focuses the row the
    /// panes are aimed at, and walks the list on repeat presses.
    pub fn focus_environment_panel(&self) {
        self.strip.focus();
    }

    /// Everything that writes is disabled — never hidden — while watching.
    ///
    /// Disabled controls still say what the pane can do; hiding them would
    /// make a watched checkout look like a different, smaller application.
    fn apply_view_permissions(&self) {
        let read_only = self.read_only();
        if read_only {
            for button in [
                &self.push_button,
                &self.pull_button,
                &self.sync_button,
                &self.abort_button,
                &self.continue_button,
                &self.init_button,
            ] {
                button.set_sensitive(false);
            }
            self.branch_label.set_sensitive(false);
            self.branch_label
                .set_tooltip_text(Some("Read-only: this is another environment's checkout"));
            self.commit_box.set_sensitive(false);
        } else {
            self.branch_label.set_sensitive(true);
            self.branch_label.set_tooltip_text(None);
        }
    }

    /// The refusal a read-only view gives, naming the environment.
    fn refuse_read_only(&self) -> bool {
        let Some((env, _)) = self.watching.borrow().clone() else {
            return false;
        };
        self.workspace.events.publish(Event::Toast(format!(
            "Read-only: {env} is another environment's checkout. Review its work \
             with Open Review in the console, or take over its chat."
        )));
        true
    }

    /// (Re)build the search index in the background, progress shown over
    /// the search entry. Kept fresh by structural workspace changes.
    pub fn rebuild_index(self: &Rc<Self>) {
        if self.index_building.get() {
            return;
        }
        self.index_building.set(true);
        self.index_bar.set_fraction(0.0);
        // The words live in the entry's placeholder, which is the one
        // string slot this row has; the bar itself carries no text (see
        // where it is built). Search still works while it runs — the
        // uncached path — so the entry stays usable and only says what it
        // is busy with.
        self.search_entry.set_placeholder_text(Some("Indexing…"));
        self.index_bar.set_visible(true);
        let root = self.view_root();
        let (tx, rx) = async_channel::unbounded::<usize>();
        let handle = crate::runtime::runtime().spawn_blocking(move || {
            taste_core::search::collect_files(&root, |count| {
                let _ = tx.try_send(count);
            })
        });
        {
            let bar = self.index_bar.clone();
            let entry = self.search_entry.clone();
            glib::spawn_future_local(async move {
                while let Ok(count) = rx.recv().await {
                    bar.pulse();
                    entry.set_placeholder_text(Some(&format!("Indexing… {count} files")));
                }
                // Restored HERE, not where the index lands: the sender is
                // dropped when the walk returns, so this is the one point
                // that is ordered after the last count. Restoring from the
                // other task races a queued count and can leave the entry
                // saying "Indexing…" forever.
                entry.set_placeholder_text(Some(SEARCH_PLACEHOLDER));
            });
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let files = handle.await.unwrap_or_default();
            let Some(tree) = weak.upgrade() else { return };
            *tree.index.borrow_mut() = Some(std::sync::Arc::new(files));
            tree.index_bar.set_visible(false);
            tree.index_building.set(false);
        });
    }

    /// Focus find-in-project (Ctrl+F).
    pub fn focus_search(&self) {
        self.search_entry.grab_focus();
    }

    /// The background file index, for quick-open (None until built).
    pub fn index_files(&self) -> Option<std::sync::Arc<Vec<PathBuf>>> {
        self.index.borrow().clone()
    }

    pub fn set_on_open(&self, f: impl Fn(PathBuf, Option<u32>) + 'static) {
        *self.on_open.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_on_open_diff(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_open_diff.borrow_mut() = Some(Box::new(f));
    }

    /// Where a review row's diff goes. Separate from `set_on_open_diff`
    /// because it is a different diff, not the same one with an argument:
    /// there is no working-tree side and no file to open.
    pub fn set_on_open_review_diff(&self, f: impl Fn(PathBuf, String, String) + 'static) {
        *self.on_open_review_diff.borrow_mut() = Some(Box::new(f));
    }

    /// What to do when the pane leaves a review — closing the tabs it
    /// opened.
    pub fn set_on_review_ended(&self, f: impl Fn() + 'static) {
        *self.on_review_ended.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_commit_suggester(&self, f: impl Fn(String, Box<dyn FnOnce(String)>) + 'static) {
        *self.commit_suggester.borrow_mut() = Some(Box::new(f));
    }

    /// Ask the chat agent for a commit message describing the staged diff;
    /// the reply lands in the commit entry (editable before committing).
    fn suggest_commit_message(self: &Rc<Self>, button: gtk::Button) {
        let diff = self
            .git
            .borrow()
            .as_ref()
            .and_then(|git| git.staged_diff(48 * 1024).ok())
            .unwrap_or_default();
        if diff.trim().is_empty() {
            self.commit_entry
                .set_placeholder_text(Some("Stage changes first, then ask for a suggestion"));
            return;
        }
        let suggester = self.commit_suggester.borrow();
        let Some(suggester) = suggester.as_ref() else {
            return;
        };
        button.set_sensitive(false);
        let entry = self.commit_entry.downgrade();
        let button = button.downgrade();
        let prompt = format!(
            "Suggest a concise git commit message (imperative mood, one line, \
             no quotes or code fences) for these staged changes. Reply with \
             ONLY the commit message.\n\n{diff}"
        );
        suggester(
            prompt,
            Box::new(move |reply| {
                if let Some(button) = button.upgrade() {
                    button.set_sensitive(true);
                }
                let message = clean_commit_message(&reply);
                if message.is_empty() {
                    return;
                }
                if let Some(entry) = entry.upgrade() {
                    entry.set_text(&message);
                }
            }),
        );
    }

    fn open(&self, path: PathBuf, line: Option<u32>) {
        if let Some(on_open) = self.on_open.borrow().as_ref() {
            on_open(path, line);
        }
    }

    fn open_diff(&self, path: PathBuf) {
        if let Some(on_open_diff) = self.on_open_diff.borrow().as_ref() {
            on_open_diff(path);
        }
    }

    /// One reviewed file, as the branch left it against the merge target.
    fn open_review_diff(&self, rel: PathBuf, aim: &ReviewAim) {
        if let Some(hook) = self.on_open_review_diff.borrow().as_ref() {
            hook(rel, aim.branch.clone(), aim.target.clone());
        }
    }

    /// Open a conflicted file at its first conflict marker (top of the
    /// file when there is none — binary or delete/modify conflicts).
    fn open_conflict(self: &Rc<Self>, abs: PathBuf) {
        let weak = Rc::downgrade(self);
        let file = abs.clone();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let text = std::fs::read_to_string(&file).ok()?;
                Some(
                    text.lines()
                        .position(|line| line.starts_with("<<<<<<<"))
                        .map(|index| index as u32 + 1),
                )
            });
            let Ok(Some(line)) = handle.await else { return };
            if let Some(tree) = weak.upgrade() {
                tree.open(abs, line);
            }
        });
    }

    /// Structural change on disk (create/remove/rename): rebuild the tree.
    pub fn refresh_tree(self: &Rc<Self>) {
        self.refresh_status();
        // Keep whichever view is active current: re-run the search, or
        // rebuild the tree; the changed-files view refreshes with status.
        let query = self.search_entry.text().trim().to_string();
        if !query.is_empty() {
            self.run_search(query);
        } else if !self.filters_active() {
            self.rebuild();
        }
    }

    // --- find in project -------------------------------------------------

    fn run_search(self: &Rc<Self>, query: String) {
        let root = self.view_root();
        let index = self.index.borrow().clone();
        let weak = Rc::downgrade(self);
        let search_query = query.clone();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || match index {
                // Indexed: skip the walk entirely.
                Some(files) => taste_core::search::search_files(&files, &search_query, 200),
                None => taste_core::search::search(&root, &search_query, 200),
            });
            let Ok(hits) = handle.await else { return };
            let Some(tree) = weak.upgrade() else { return };
            // Render only if this exact query is still what's typed —
            // a slower, older search must not overwrite newer results.
            if tree.search_entry.text().trim() != query.trim() {
                return;
            }
            let root = tree.view_root();
            let mut grouped: HashMap<PathBuf, Vec<taste_core::search::SearchHit>> = HashMap::new();
            for hit in hits {
                grouped.entry(hit.path.clone()).or_default().push(hit);
            }
            let mut visible: HashSet<PathBuf> = HashSet::new();
            for path in grouped.keys() {
                visible.insert(path.clone());
                let mut current = path.as_path();
                while let Some(parent) = current.parent() {
                    if parent == root || !parent.starts_with(&root) {
                        break;
                    }
                    visible.insert(parent.to_path_buf());
                    current = parent;
                }
            }
            // The active editor file is always part of the result view —
            // its matches (even zero) are the "search here" subset.
            let pinned = tree
                .workspace
                .ide
                .open_files()
                .into_iter()
                .find(|f| f.active)
                .map(|f| f.path);
            if let Some(pinned) = &pinned {
                visible.insert(pinned.clone());
                let mut current = pinned.as_path();
                while let Some(parent) = current.parent() {
                    if parent == root || !parent.starts_with(&root) {
                        break;
                    }
                    visible.insert(parent.to_path_buf());
                    current = parent;
                }
            }
            let empty = grouped.is_empty() && pinned.is_none();
            *tree.search_view.borrow_mut() = Some(Rc::new(SearchView {
                hits: grouped,
                visible: Rc::new(visible),
                pinned: pinned.clone(),
            }));
            if empty && !tree.search_ghosts_toggle.is_active() {
                let status = adw::StatusPage::builder()
                    .icon_name("system-search-symbolic")
                    .title("No Matches")
                    .css_classes(["compact"])
                    .build();
                tree.list_holder.set_child(Some(&status));
            } else {
                tree.rebuild();
            }
            // Open (or refresh) the match panel: the user's chosen file if
            // one is up, otherwise the current file — "initially selected."
            let panel_target = tree
                .intervention_file
                .borrow()
                .clone()
                .filter(|_| tree.intervention.is_visible())
                .or(pinned);
            if let Some(target) = panel_target {
                tree.matches_intervention(target);
            }
        });
    }

    /// Bottom panel with one file's matches; activating a row jumps to
    /// that line. Same convention as the dirty-file workflows: closing it
    /// restores the full-height file list.
    fn matches_intervention(self: &Rc<Self>, path: PathBuf) {
        if self.search_view.borrow().is_none() {
            return;
        }
        let hits = self
            .search_view
            .borrow()
            .as_ref()
            .and_then(|view| view.hits.get(&path).cloned())
            .unwrap_or_default();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = self.open_intervention(&format!(
            "{name} — {} match{}",
            hits.len(),
            if hits.len() == 1 { "" } else { "es" }
        ));
        *self.intervention_file.borrow_mut() = Some(path.clone());
        if hits.is_empty() {
            content.append(
                &gtk::Label::builder()
                    .label("No matches in this file")
                    .css_classes(["dim-label", "caption"])
                    .xalign(0.0)
                    .build(),
            );
            return;
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for hit in &hits {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&hit.text))
                // One line per match, ellipsized: the row is a pointer to
                // the code, not a reproduction of it.
                .title_lines(1)
                .subtitle(format!("line {}", hit.line))
                .activatable(true)
                .build();
            let weak = Rc::downgrade(self);
            let path = path.clone();
            let line = hit.line;
            row.connect_activated(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.open(path.clone(), Some(line));
                }
            });
            list.append(&row);
        }
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .max_content_height(240)
            .propagate_natural_height(true)
            .build();
        content.append(&scroller);
    }

    // --- file operations ---------------------------------------------------

    fn file_op_allowed(&self, path: &Path) -> bool {
        if self.read_only() {
            return false;
        }
        let safe_mode = !self.workspace.exec.is_container();
        taste_core::policy::write_allowed(self.workspace.root(), safe_mode, path)
    }

    fn op_denied_dialog(&self) {
        if self.refuse_read_only() {
            return;
        }
        self.workspace.events.publish(Event::Toast(
            "Read-only until the project's environment is running — only devcontainer \
             setup and workspace dotfiles are editable"
                .into(),
        ));
    }

    fn prompt_name(
        self: Rc<Self>,
        heading: &str,
        affirm: &str,
        initial: &str,
        on_done: impl Fn(&Rc<Self>, String) + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), None);
        let entry = gtk::Entry::builder()
            .text(initial)
            .activates_default(true)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("ok", affirm)]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        let tree = self.clone();
        dialog.connect_response(Some("ok"), move |_, _| {
            let name = entry.text().to_string();
            let name = name.trim();
            if !name.is_empty() && !name.contains('/') && name != "." && name != ".." {
                on_done(&tree, name.to_string());
            }
        });
        dialog.present(Some(&self.widget));
    }

    fn confirm_destructive(
        self: Rc<Self>,
        heading: &str,
        body: &str,
        on_confirm: impl Fn(&Rc<Self>) + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("confirm", "Delete")]);
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let tree = self.clone();
        dialog.connect_response(Some("confirm"), move |_, _| on_confirm(&tree));
        dialog.present(Some(&self.widget));
    }

    fn create_file(self: &Rc<Self>, path: &Path, is_dir: bool) {
        if !self.file_op_allowed(path) {
            self.op_denied_dialog();
            return;
        }
        let result = if is_dir {
            std::fs::create_dir_all(path)
        } else {
            path.parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|_| {
                    if path.exists() {
                        Ok(())
                    } else {
                        std::fs::write(path, "")
                    }
                })
        };
        if let Err(e) = result {
            self.workspace.events.publish(Event::Toast(format!(
                "Could not create {}: {e}",
                path.display()
            )));
            return;
        }
        self.workspace.events.publish(Event::GitStatusChanged);
        self.refresh_status();
        self.rebuild();
        if !is_dir {
            self.open(path.to_path_buf(), None);
        }
    }

    fn rename(self: &Rc<Self>, from: &Path, to_name: &str) {
        let to = match from.parent() {
            Some(parent) => parent.join(to_name),
            None => return,
        };
        if !self.file_op_allowed(from) || !self.file_op_allowed(&to) {
            self.op_denied_dialog();
            return;
        }
        if let Err(e) = std::fs::rename(from, &to) {
            self.workspace
                .events
                .publish(Event::Toast(format!("Rename failed: {e}")));
            return;
        }
        self.workspace.events.publish(Event::GitStatusChanged);
        self.refresh_status();
        self.rebuild();
    }

    fn delete(self: &Rc<Self>, path: &Path, is_dir: bool) {
        if !self.file_op_allowed(path) {
            self.op_denied_dialog();
            return;
        }
        let result = if is_dir {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(e) = result {
            self.workspace
                .events
                .publish(Event::Toast(format!("Delete failed: {e}")));
            return;
        }
        self.workspace.events.publish(Event::GitStatusChanged);
        self.refresh_status();
        self.rebuild();
    }

    /// Right-click menu on a row (native GMenu/PopoverMenu): stage/unstage
    /// when applicable, then New File / New Folder / Rename / Delete.
    fn show_context_menu(self: &Rc<Self>, anchor: &gtk::Box, node: &FileNode) {
        use gtk::gio;

        let actions = gio::SimpleActionGroup::new();
        // Every file operation in this menu writes. In a watched
        // environment they are all present and all disabled — the menu
        // still says what this pane can do, it simply cannot do it here.
        let writable = !self.read_only();
        let add_action = |name: &str, callback: Box<dyn Fn() + 'static>| {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(writable);
            action.connect_activate(move |_, _| callback());
            actions.add_action(&action);
        };

        let menu = gio::Menu::new();

        // Git section: staging lives here, not as row chrome.
        if !node.is_dir && !node.ghost {
            let state = self.state_of(node);
            let git_section = gio::Menu::new();
            if state.stageable() {
                git_section.append(Some("Stage"), Some("row.stage"));
                let tree = self.clone();
                let path = node.path.clone();
                add_action("stage", Box::new(move || tree.toggle_stage(&path, false)));
            } else if state == FileState::Staged {
                git_section.append(Some("Unstage"), Some("row.unstage"));
                let tree = self.clone();
                let path = node.path.clone();
                add_action("unstage", Box::new(move || tree.toggle_stage(&path, true)));
            }
            if git_section.n_items() > 0 {
                menu.append_section(None, &git_section);
            }
        }

        let target_dir = if node.is_dir {
            node.path.clone()
        } else {
            node.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.view_root())
        };

        let create_section = gio::Menu::new();
        create_section.append(Some("New File…"), Some("row.new-file"));
        create_section.append(Some("New Folder…"), Some("row.new-folder"));
        menu.append_section(None, &create_section);
        {
            let tree = self.clone();
            let dir = target_dir.clone();
            add_action(
                "new-file",
                Box::new(move || {
                    let dir = dir.clone();
                    tree.clone()
                        .prompt_name("New File", "Create", "", move |tree, name| {
                            tree.create_file(&dir.join(name), false);
                        });
                }),
            );
        }
        {
            let tree = self.clone();
            let dir = target_dir;
            add_action(
                "new-folder",
                Box::new(move || {
                    let dir = dir.clone();
                    tree.clone()
                        .prompt_name("New Folder", "Create", "", move |tree, name| {
                            tree.create_file(&dir.join(name), true);
                        });
                }),
            );
        }

        if !node.ghost {
            let modify_section = gio::Menu::new();
            modify_section.append(Some("Rename…"), Some("row.rename"));
            modify_section.append(Some("Delete…"), Some("row.delete"));
            menu.append_section(None, &modify_section);
            {
                let tree = self.clone();
                let path = node.path.clone();
                let current = node
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                add_action(
                    "rename",
                    Box::new(move || {
                        let path = path.clone();
                        let current = current.clone();
                        tree.clone().prompt_name(
                            "Rename",
                            "Rename",
                            &current,
                            move |tree, name| {
                                tree.rename(&path, &name);
                            },
                        );
                    }),
                );
            }
            {
                let tree = self.clone();
                let path = node.path.clone();
                let is_dir = node.is_dir;
                let name = node
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                add_action(
                    "delete",
                    Box::new(move || {
                        let path = path.clone();
                        let name = name.clone();
                        tree.clone().confirm_destructive(
                            "Delete?",
                            &format!("“{name}” will be permanently deleted."),
                            move |tree| tree.delete(&path, is_dir),
                        );
                    }),
                );
            }
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("row", Some(&actions));
        popover.set_parent(anchor);
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        // Tracked so row rebinds (git status churn while an agent writes)
        // can close it before its anchor row is disposed under it.
        *self.open_menu.borrow_mut() = Some(popover.downgrade());
        popover.popup();
    }

    /// Close the context menu, if open, before its anchor may be disposed.
    fn close_open_menu(&self) {
        if let Some(weak) = self.open_menu.borrow_mut().take() {
            if let Some(popover) = weak.upgrade() {
                popover.popdown();
            }
        }
    }

    /// Re-query git status, the branch indicator, and the sync relation.
    ///
    /// Coalesced (`RefreshGate`): the watcher reports one event per changed
    /// path, and a busy checkout produces those in bursts. One query per
    /// burst, never two at once.
    pub fn refresh_status(self: &Rc<Self>) {
        if !self.refresh.request() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(REFRESH_COALESCE, move || {
            let Some(tree) = weak.upgrade() else { return };
            if tree.refresh.fire() {
                tree.query_status();
            }
        });
    }

    fn query_status(self: &Rc<Self>) {
        // Counts stay honest without anyone clicking: refreshes carry a
        // (throttled) background fetch.
        self.background_fetch();
        // Full-status computation runs off the main thread: with an agent
        // or build churning files, doing this synchronously would stutter
        // every interaction (window drags included). Results apply — and
        // rows restyle — when ready.
        let root = self.view_root();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                GitWorkspace::discover(&root).map(|git| StatusSnapshot {
                    status: git.status().unwrap_or_default(),
                    stashed: git.stashed_paths().unwrap_or_default(),
                    ignore_rules: std::fs::read_to_string(root.join(".gitignore"))
                        .map(|text| {
                            text.lines()
                                .filter(|l| {
                                    let l = l.trim();
                                    !l.is_empty() && !l.starts_with('#')
                                })
                                .count()
                        })
                        .unwrap_or(0),
                    branch: git.branch_name(),
                    sync: git.sync_status().ok(),
                    rebasing: git.rebase_in_progress(),
                    // Published work, against the branch the user is on.
                    // Cheap (libgit2 walks only the symmetric difference)
                    // and it rides the refresh every `.git` change already
                    // triggers, so publishing shows up without polling.
                })
            });
            let snapshot = handle.await;
            let Some(tree) = weak.upgrade() else { return };
            if let Ok(snapshot) = snapshot {
                tree.apply_status(snapshot);
            }
            // Every exit from a query has to release the gate, or the tree
            // stops refreshing for the life of the window.
            if tree.refresh.finish() {
                tree.refresh_status();
            }
        });
    }

    fn apply_status(self: &Rc<Self>, snapshot: Option<StatusSnapshot>) {
        let is_repo = snapshot.is_some();
        // A safe ↔ container flip restyles every row (the read-only locks)
        // even when git status is identical — starting the devcontainer
        // must not leave stale locks behind.
        let container_mode = self.workspace.exec.is_container();
        let mode_changed = self.container_mode.replace(container_mode) != container_mode;
        // Both arms assign it; no initializer, so a dead one can't hide.
        let unchanged;
        // Nothing but git state moved, so the rows that moved can be
        // restyled in place. A mode flip (locks) or a new ignore rule can
        // change what a row shows for reasons the status map doesn't
        // record, and those take the full rebind.
        let mut states_only = false;
        // Conflict transitions steer the view (applied at the end, once
        // the fresh maps are in place).
        let mut conflicts_appeared = false;
        let mut rebase_ended = false;
        match snapshot {
            Some(snapshot) => {
                self.rendered_non_repo.set(false);
                // Unchanged status must not churn row widgets: factory
                // resets during agent/build activity are what made hovering
                // feel laggy. ignore_rules participates: a new rule changes
                // no file state, but the Staged pane restore rides on this
                // refresh actually rendering.
                unchanged = !mode_changed
                    && *self.status.borrow() == snapshot.status
                    && *self.stashed.borrow() == snapshot.stashed
                    && self.ignore_rules.get() == snapshot.ignore_rules;
                states_only = !mode_changed && self.ignore_rules.get() == snapshot.ignore_rules;
                let conflict_count = |status: &HashMap<PathBuf, FileState>| {
                    status
                        .values()
                        .filter(|s| **s == FileState::Conflicted)
                        .count()
                };
                conflicts_appeared = conflict_count(&self.status.borrow()) == 0
                    && conflict_count(&snapshot.status) > 0;
                // The abort button's visibility IS the previous snapshot's
                // rebasing flag — it is set nowhere else.
                rebase_ended = self.abort_button.is_visible() && !snapshot.rebasing;
                *self.status.borrow_mut() = snapshot.status;
                *self.stashed.borrow_mut() = snapshot.stashed;
                self.ignore_rules.set(snapshot.ignore_rules);
                self.sync_filter_counts();
                self.set_branch_label(&snapshot.branch.unwrap_or_else(|| "(no branch)".into()));
                self.abort_button.set_visible(snapshot.rebasing);
                self.continue_button.set_visible(snapshot.rebasing);
                // Mid-operation: this snapshot predates the result, so
                // applying it would paint the pre-operation counts over
                // the spinner, then correct them a moment later.
                if !self.sync_busy.get() {
                    self.sync_button.set_icon_name("view-refresh-symbolic");
                    self.sync_button.set_width_request(-1);
                    self.sync_button.set_sensitive(!snapshot.rebasing);
                    if snapshot.rebasing {
                        self.sync_label
                            .set_label("rebase paused — resolve, mark, Continue");
                    } else {
                        match snapshot.sync {
                            Some(sync) => match sync.upstream {
                                Some(upstream) => {
                                    // The upstream name lives in the button
                                    // tooltips; the label is for exceptions.
                                    self.sync_label.set_label("");
                                    self.push_button.set_width_request(-1);
                                    self.push_button.set_label(&format!("↑ {}", sync.ahead));
                                    self.push_button.set_sensitive(sync.ahead > 0);
                                    self.push_button.set_tooltip_text(Some(&format!(
                                        "Push {} commit{} to {upstream}",
                                        sync.ahead,
                                        if sync.ahead == 1 { "" } else { "s" }
                                    )));
                                    self.pull_button.set_width_request(-1);
                                    self.pull_button.set_label(&format!("↓ {}", sync.behind));
                                    self.pull_button.set_sensitive(sync.behind > 0);
                                    self.pull_button.set_tooltip_text(Some(&format!(
                                        "Pull {} commit{} (fetch + rebase)",
                                        sync.behind,
                                        if sync.behind == 1 { "" } else { "s" }
                                    )));
                                }
                                None => {
                                    self.sync_label.set_label("no upstream");
                                    self.push_button.set_sensitive(false);
                                    self.pull_button.set_sensitive(false);
                                }
                            },
                            None => self.sync_label.set_label(""),
                        }
                    }
                }
            }
            None => {
                // Nothing to diff between non-repo refreshes: after the
                // first render, only a mode flip warrants row churn.
                unchanged = !mode_changed && self.rendered_non_repo.replace(true);
                self.branch_label.set_label("not a git repository");
                self.init_button.set_label("Initialize Repository");
                self.init_button.set_width_request(-1);
                self.init_button.set_sensitive(true);
            }
        }
        // Not a repo: the branch dropdown and sync tools mean nothing;
        // one Initialize button takes their place until init succeeds.
        self.init_button.set_visible(!is_repo);
        self.branch_label.set_visible(is_repo);
        self.pull_button.set_visible(is_repo);
        self.push_button.set_visible(is_repo);
        self.sync_button.set_visible(is_repo);
        // Disable, never hide: the git-state filters mean nothing without
        // a repository; All keeps working.
        self.stashed_toggle.set_sensitive(is_repo);
        self.dirty_toggle.set_sensitive(is_repo);
        self.staged_toggle.set_sensitive(is_repo);
        self.conflicts_toggle.set_sensitive(is_repo);
        // Landing in conflicts jumps straight to the Conflicts view — the
        // rebase can't move until they're dealt with; the rebase ending
        // (or aborting) leaves it again. set_active renders via the
        // toggled handler, so these paths skip the render below.
        if conflicts_appeared && !self.conflicts_toggle.is_active() {
            self.conflicts_toggle.set_visible(true);
            self.conflicts_toggle.set_active(true);
            return;
        }
        if rebase_ended && self.conflicts_toggle.is_active() {
            self.conflicts_toggle.set_visible(false);
            self.all_toggle.set_active(true);
            return;
        }
        // Whatever the git state said about what is possible, a watched
        // environment is read-only: this has the last word.
        self.apply_view_permissions();
        // Views refresh only now, with the fresh map in place — and only
        // if something actually changed.
        if unchanged {
            return;
        }
        if self.filters_active() && self.search_entry.text().trim().is_empty() {
            self.render_filter_view();
        } else if states_only {
            self.restyle_changed_rows();
        } else {
            self.rebuild_rows_in_place();
        }
    }

    /// Commit-building view: only changed files, each with an explicit
    /// stage checkbox — selection you can see.
    /// Refresh the counts carried by the filter buttons and the eye.
    fn sync_filter_counts(&self) {
        let ignore_rules = self.ignore_rules.get();
        let status = self.status.borrow();
        let dirty = status.values().filter(|s| s.stageable()).count();
        let staged = status.values().filter(|s| **s == FileState::Staged).count();
        let conflicts = status
            .values()
            .filter(|s| **s == FileState::Conflicted)
            .count();
        drop(status);
        let stashed = self.stashed.borrow().len();
        self.dirty_toggle.set_label(&format!("Dirty {dirty}"));
        self.staged_toggle.set_label(&format!("Staged {staged}"));
        if staged > 0 {
            self.staged_toggle.add_css_class("accent");
        } else {
            self.staged_toggle.remove_css_class("accent");
        }
        self.stashed_toggle.set_label(&format!("Stashed {stashed}"));
        self.conflicts_toggle
            .set_label(&format!("Conflicts {conflicts}"));
        // Present only while it means something — but never yanked out
        // from under its own active view.
        self.conflicts_toggle
            .set_visible(conflicts > 0 || self.conflicts_toggle.is_active());
        match self.index_files() {
            Some(files) => self.all_toggle.set_label(&format!("All {}", files.len())),
            None => self.all_toggle.set_label("All"),
        }
        self.ignored_toggle.set_tooltip_text(Some(&format!(
            "Show ignored files — {ignore_rules} ignore rule{} (the RULE \
             count; counting ignored files would walk everything)",
            if ignore_rules == 1 { "" } else { "s" }
        )));
        // Plain icon + caption count (ButtonContent bolds its label,
        // which reads as unearned emphasis next to the quiet filters).
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        content.append(&gtk::Image::from_icon_name("view-conceal-symbolic"));
        content.append(
            &gtk::Label::builder()
                .label(ignore_rules.to_string())
                .css_classes(["caption"])
                .build(),
        );
        self.ignored_toggle.set_child(Some(&content));
    }

    /// True when the list is showing something other than the tree: a git
    /// filter, or a branch under review.
    ///
    /// The review counts, because every caller of this is asking the same
    /// question — "is the tree what is on screen" — and a review that
    /// answered no would be refreshed away by the next status pass.
    fn filters_active(&self) -> bool {
        self.dirty_toggle.is_active()
            || self.staged_toggle.is_active()
            || self.stashed_toggle.is_active()
            || self.conflicts_toggle.is_active()
            || self.review.borrow().is_some()
    }

    /// Render whichever non-tree view is active.
    ///
    /// A branch under review outranks the filters: it was aimed here by an
    /// explicit action, and a status refresh that painted the Dirty list
    /// over it would take the user out of a review they did not leave.
    fn render_filter_view(self: &Rc<Self>) {
        if let Some(aim) = self.review.borrow().clone() {
            self.render_review_files(aim);
            return;
        }
        self.render_changed_list();
    }

    fn render_changed_list(self: &Rc<Self>) {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let dirty_on = self.dirty_toggle.is_active();
        let staged_on = self.staged_toggle.is_active();
        let stashed_on = self.stashed_toggle.is_active();
        let conflicts_on = self.conflicts_toggle.is_active();
        // OR of the active categories; a path in several shows once.
        let mut matched: std::collections::BTreeMap<PathBuf, (FileState, bool)> =
            std::collections::BTreeMap::new();
        {
            let stashed = self.stashed.borrow();
            for (path, state) in self.status.borrow().iter() {
                if *state == FileState::Ignored {
                    continue;
                }
                if (dirty_on && state.stageable())
                    || (staged_on && *state == FileState::Staged)
                    || (conflicts_on && *state == FileState::Conflicted)
                {
                    matched.insert(path.clone(), (*state, stashed.contains(path)));
                }
            }
            if stashed_on {
                let status = self.status.borrow();
                for path in stashed.iter() {
                    let state = status.get(path).copied().unwrap_or(FileState::Clean);
                    matched.insert(path.clone(), (state, true));
                }
            }
        }
        let entries: Vec<(PathBuf, FileState, bool)> = matched
            .into_iter()
            .map(|(path, (state, stashed))| (path, state, stashed))
            .collect();
        // Rebuilding rows resets every checkbox (the Staged view re-checks
        // them all), so the selection restarts from scratch with the rows —
        // keeping stale paths here made the ops pane act on files whose
        // checkboxes read unchecked.
        self.selection.borrow_mut().clear();
        if entries.is_empty() {
            if matches!(self.pane.get(), PaneKind::Selection | PaneKind::Staged) {
                self.close_intervention();
            }
            let empty = adw::StatusPage::builder()
                .icon_name("object-select-symbolic")
                .title("No Matching Files")
                .description(if conflicts_on {
                    if self.abort_button.is_visible() {
                        "All conflicts resolved — Continue the rebase above"
                    } else {
                        "No conflicts right now"
                    }
                } else if staged_on {
                    "Nothing is staged right now"
                } else if dirty_on {
                    "Nothing is dirty right now"
                } else {
                    "Nothing is stashed right now"
                })
                .css_classes(["compact"])
                .build();
            self.list_holder.set_child(Some(&empty));
            return;
        }
        let workdir = self
            .git
            .borrow()
            .as_ref()
            .map(|git| git.workdir().to_path_buf());
        self.syncing_selection.set(true);
        for (rel, state, in_stash) in entries {
            let check = gtk::CheckButton::new();
            check.set_tooltip_text(Some(if conflicts_on {
                "Select for conflict resolution"
            } else if staged_on {
                "A commit takes every staged file; unstage a file to leave it out"
            } else {
                "Select for stage/stash/unstage actions"
            }));
            {
                let weak = Rc::downgrade(self);
                let abs = workdir
                    .as_ref()
                    .map(|w| w.join(&rel))
                    .unwrap_or_else(|| rel.clone());
                check.connect_toggled(move |check| {
                    let Some(tree) = weak.upgrade() else { return };
                    if check.is_active() {
                        tree.selection.borrow_mut().insert(abs.clone());
                    } else {
                        tree.selection.borrow_mut().remove(&abs);
                    }
                    if !tree.syncing_selection.get() {
                        tree.selection_intervention();
                    }
                });
            }
            // Staged view: everything selected by default — the checkboxes
            // show what the commit takes (which is all of it).
            if staged_on {
                check.set_active(true);
            }
            let row = adw::ActionRow::builder()
                .title(
                    rel.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
                .subtitle(rel.display().to_string())
                .activatable(true)
                .tooltip_text(if conflicts_on {
                    "Opens the file at its first conflict marker"
                } else {
                    "Opens the diff — the tab's Changes view"
                })
                .build();
            row.add_prefix(&check);
            let badge = gtk::Label::builder()
                .label(match state {
                    FileState::Staged => "S",
                    FileState::Modified => "M",
                    FileState::Untracked => "U",
                    FileState::Conflicted => "!",
                    _ => "",
                })
                .css_classes(["caption", "dim-label"])
                .build();
            row.add_suffix(&badge);
            if in_stash {
                let stash_badge = gtk::Label::builder()
                    .label("stashed")
                    .css_classes(["caption", "dim-label"])
                    .build();
                row.add_suffix(&stash_badge);
            }
            // Row actions: discard, and a … menu for the rest (stash,
            // ignore). Staging is the checkbox. Inapplicable = disabled.
            let abs = workdir
                .as_ref()
                .map(|w| w.join(&rel))
                .unwrap_or_else(|| rel.clone());
            let discard = gtk::Button::builder()
                .icon_name("edit-undo-symbolic")
                .tooltip_text(if state == FileState::Untracked {
                    "Untracked: no committed state to restore"
                } else {
                    "Discard changes (restore the committed state)"
                })
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .sensitive(
                    matches!(state, FileState::Modified | FileState::Conflicted)
                        && !self.read_only(),
                )
                .build();
            {
                let weak = Rc::downgrade(self);
                let abs = abs.clone();
                discard.connect_clicked(move |_| {
                    if let Some(tree) = weak.upgrade() {
                        tree.discard_intervention(abs.clone());
                    }
                });
            }
            row.add_suffix(&discard);
            let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let popover = gtk::Popover::builder().child(&menu_box).build();
            let more = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .sensitive(!self.read_only())
                .popover(&popover)
                .build();
            for (label, icon, ignore_flow) in [
                ("Stash…", "document-save-symbolic", false),
                ("Add to Ignores…", "action-unavailable-symbolic", true),
            ] {
                // Equal-width rows, icon first — the hit target is the
                // whole row, not the word.
                let row_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                row_content.set_halign(gtk::Align::Start);
                row_content.append(&gtk::Image::from_icon_name(icon));
                row_content.append(&gtk::Label::new(Some(label)));
                let item = gtk::Button::builder()
                    .child(&row_content)
                    .css_classes(["flat"])
                    .width_request(180)
                    .build();
                let weak = Rc::downgrade(self);
                let abs = abs.clone();
                let popover = popover.clone();
                item.connect_clicked(move |_| {
                    popover.popdown();
                    let Some(tree) = weak.upgrade() else { return };
                    if ignore_flow {
                        tree.ignore_intervention(abs.clone());
                    } else {
                        tree.stash_intervention(abs.clone());
                    }
                });
                menu_box.append(&item);
            }
            row.add_suffix(&more);
            {
                let weak = Rc::downgrade(self);
                let abs = workdir
                    .as_ref()
                    .map(|w| w.join(&rel))
                    .unwrap_or_else(|| rel.clone());
                row.connect_activated(move |_| {
                    if let Some(tree) = weak.upgrade() {
                        if conflicts_on {
                            // Resolution happens in the buffer: land on the
                            // first conflict marker, not a diff of the mess.
                            tree.open_conflict(abs.clone());
                        } else {
                            tree.open_diff(abs.clone());
                        }
                    }
                });
            }
            list.append(&row);
        }
        self.syncing_selection.set(false);
        self.list_holder.set_child(Some(&list));
        // The Staged view carries its ops-and-commit pane at the bottom of
        // the list; elsewhere a selection-derived pane is now stale (the
        // selection was just reset with the rows). Ad-hoc flows survive.
        if staged_on {
            self.selection_intervention();
        } else if matches!(self.pane.get(), PaneKind::Selection | PaneKind::Staged) {
            self.close_intervention();
        }
    }

    /// One environment branch's changed files, against its merge base
    /// with the current branch. Rows open as diffs — the editor's Changes
    /// face, the same plumbing the Dirty and Staged lists use.
    fn render_review_files(self: &Rc<Self>, aim: ReviewAim) {
        if self.git.borrow().is_none() {
            return;
        }
        let Some(workdir) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        let weak = Rc::downgrade(self);
        let root = workdir.clone();
        let wanted = aim.clone();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                GitWorkspace::discover(&root)
                    .and_then(|git| git.changed_since_base(&wanted.branch, &wanted.target).ok())
                    .unwrap_or_default()
            });
            let Ok(changed) = handle.await else { return };
            let Some(tree) = weak.upgrade() else { return };
            // The user may have left the review (or aimed it at another
            // environment) while git worked.
            if tree.review.borrow().as_ref() != Some(&aim) {
                return;
            }
            tree.build_review_file_list(&aim, &changed);
        });
    }

    fn build_review_file_list(
        self: &Rc<Self>,
        aim: &ReviewAim,
        changed: &[taste_git::ChangedFile],
    ) {
        let branch = aim.branch.as_str();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        // The way out. There is no list to go back to any more — the
        // fleet is the list, and it is in the panel below — so this leaves
        // the review rather than ascending one level of it.
        let back = adw::ActionRow::builder()
            .title("Close Review")
            .subtitle(glib::markup_escape_text(&format!(
                "{branch} → {}",
                aim.target
            )))
            .subtitle_lines(1)
            .activatable(true)
            .tooltip_text(
                "Back to the file tree, and the diffs this review opened close with it. \
                 The environment's review state is unchanged — merging and rejecting \
                 happen in the console, where the branch's mergedness is.",
            )
            .build();
        back.add_prefix(&gtk::Image::from_icon_name("go-previous-symbolic"));
        {
            let weak = Rc::downgrade(self);
            back.connect_activated(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.close_review();
                }
            });
        }
        list.append(&back);
        if changed.is_empty() {
            let row = adw::ActionRow::builder()
                .title("Nothing new on this branch")
                .subtitle("Everything on it is already in the current branch")
                .build();
            row.add_css_class("dim-label");
            list.append(&row);
        }
        let tooltip = format!(
            "Opens this file as {branch} left it, against {} — read-only, and not \
             your working copy",
            aim.target
        );
        for file in changed {
            let row = adw::ActionRow::builder()
                .title(
                    file.path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
                .subtitle(file.path.display().to_string())
                .activatable(true)
                .tooltip_text(&tooltip)
                .build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(file.kind.badge())
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            let weak = Rc::downgrade(self);
            let rel = file.path.clone();
            let aim = aim.clone();
            row.connect_activated(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.open_review_diff(rel.clone(), &aim);
                }
            });
            list.append(&row);
        }
        self.list_holder.set_child(Some(&list));
    }

    /// Fill the branch dropdown: every local branch (current checked,
    /// activate to switch) plus a create-and-switch entry.
    fn populate_branch_menu(self: &Rc<Self>) {
        let Some(git) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        let current = self.git.borrow().as_ref().and_then(|g| g.branch_name());
        let branches = self
            .git
            .borrow()
            .as_ref()
            .and_then(|g| g.local_branches().ok())
            .unwrap_or_default();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        for branch in &branches {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(branch))
                // One line, ellipsized — never hyphen-wrap a branch name.
                .title_lines(1)
                .activatable(true)
                .build();
            if Some(branch) == current.as_ref() {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            let weak = Rc::downgrade(self);
            let name = branch.clone();
            let root = git.clone();
            row.connect_activated(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                tree.branch_popover.popdown();
                tree.run_branch_op(root.clone(), name.clone(), false);
            });
            list.append(&row);
        }

        let entry = gtk::Entry::builder()
            .placeholder_text("new-branch-name")
            .hexpand(true)
            .build();
        entry.set_cursor_from_name(Some("text"));
        let create = gtk::Button::builder()
            .label("Create")
            .css_classes(["suggested-action"])
            .build();
        {
            let weak = Rc::downgrade(self);
            let entry = entry.clone();
            let root = git.clone();
            create.connect_clicked(move |_| {
                let name = entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                let Some(tree) = weak.upgrade() else { return };
                tree.branch_popover.popdown();
                tree.run_branch_op(root.clone(), name, true);
            });
        }
        let create_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        create_row.set_margin_top(6);
        create_row.append(&entry);
        create_row.append(&create);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        // Real menu width: branch names and the create entry need room.
        content.set_width_request(260);
        content.append(&list);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&create_row);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .propagate_natural_height(true)
            .max_content_height(360)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        self.branch_popover.set_child(Some(&scroller));
    }

    /// Switch to (or create-and-switch to) a branch, off the main thread;
    /// failures (e.g. conflicting working-tree changes) surface as toasts.
    fn run_branch_op(self: &Rc<Self>, root: PathBuf, name: String, create: bool) {
        if self.refuse_read_only() {
            return;
        }
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let branch = name.clone();
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = GitWorkspace::discover(&root)
                    .ok_or_else(|| "not a git repository".to_string())?;
                if create {
                    git.create_branch(&branch).map_err(|e| e.to_string())
                } else {
                    git.switch_branch(&branch).map_err(|e| e.to_string())
                }
            });
            let Ok(result) = handle.await else { return };
            match result {
                Ok(()) => events.publish(Event::Toast(format!(
                    "{} {name}",
                    if create {
                        "Created and switched to"
                    } else {
                        "Switched to"
                    }
                ))),
                Err(e) => events.publish(Event::Toast(format!("Branch: {e}"))),
            }
            if let Some(tree) = weak.upgrade() {
                // New HEAD: statuses, rows, and open buffers all changed.
                tree.refresh_status();
                tree.rebuild();
                tree.workspace.events.publish(Event::FileTreeChanged);
            }
        });
    }

    /// Bottom-anchored bulk actions for the checked files: what applies
    /// depends on each file's state (stage/unstage/stash/unstash).
    fn selection_intervention(self: &Rc<Self>) {
        let staged_view = self.staged_toggle.is_active();
        let selected: Vec<PathBuf> = self.selection.borrow().iter().cloned().collect();
        // The Staged view keeps its pane even with nothing selected — it is
        // the view's resting state (ops disabled, composer grayed).
        if selected.is_empty() && !staged_view {
            self.close_intervention();
            return;
        }
        let status = self.status.borrow();
        let stashed = self.stashed.borrow();
        let workdir = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf());
        let rel_of = |abs: &PathBuf| {
            workdir
                .as_ref()
                .and_then(|w| abs.strip_prefix(w).ok())
                .map(|r| r.to_path_buf())
                .unwrap_or_else(|| abs.clone())
        };
        let stageable = selected
            .iter()
            .filter(|p| status.get(&rel_of(p)).is_some_and(|s| s.stageable()))
            .count();
        let staged = selected
            .iter()
            .filter(|p| status.get(&rel_of(p)) == Some(&FileState::Staged))
            .count();
        let in_stash = selected
            .iter()
            .filter(|p| stashed.contains(&rel_of(p)))
            .count();
        let conflicted = selected
            .iter()
            .filter(|p| status.get(&rel_of(p)) == Some(&FileState::Conflicted))
            .count();
        drop(status);
        drop(stashed);

        let title = format!(
            "{} file{} selected",
            selected.len(),
            if selected.len() == 1 { "" } else { "s" }
        );
        // The Staged view's pane is its resting state: not closable.
        let content = if staged_view {
            self.open_pane(&title, false)
        } else {
            self.open_intervention(&title)
        };
        // Bulk ops are directional. The views sit on a pipeline —
        //   Stashed ← Dirty ↔ Staged (→ commit)
        // farther left is farther from the commit — and with one view
        // active at a time, each offers exactly the moves out of it.
        // Left buttons push the checked files away from the commit and the
        // view stays put; right buttons move them toward it and the view
        // follows the files (see run_selection_op).
        type Op = (&'static str, usize, &'static str, &'static str);
        let (left_ops, right_ops): (Vec<Op>, Vec<Op>) = if self.conflicts_toggle.is_active() {
            // Off the pipeline: every resolution ends staged. The two
            // wholesale choices sit left; hand-fixed files get marked on
            // the right, the guided path.
            (
                vec![
                    (
                        "Keep Yours",
                        conflicted,
                        "keep-yours",
                        "Resolve the checked files with your version",
                    ),
                    (
                        "Take Remote",
                        conflicted,
                        "take-remote",
                        "Resolve the checked files with the remote tip's version",
                    ),
                ],
                vec![(
                    "Mark Resolved →",
                    conflicted,
                    "mark-resolved",
                    "The checked files are fixed by hand — mark them \
                     resolved (they join the staged set)",
                )],
            )
        } else if staged_view {
            (
                vec![
                    (
                        "← Stash",
                        stageable + staged,
                        "stash",
                        "Set the checked files aside — out of the index \
                         and the working tree",
                    ),
                    (
                        "← Unstage",
                        staged,
                        "unstage",
                        "Take the checked files out of the next commit, \
                         back to dirty",
                    ),
                ],
                vec![],
            )
        } else if self.stashed_toggle.is_active() {
            (
                vec![],
                vec![(
                    "Unstash →",
                    in_stash,
                    "unstash",
                    "Restore the checked files to the working tree — one \
                     step closer to a commit",
                )],
            )
        } else {
            (
                vec![(
                    "← Stash",
                    stageable + staged,
                    "stash",
                    "Set the checked files aside — out of the working tree",
                )],
                vec![(
                    "Stage →",
                    stageable,
                    "stage",
                    "Put the checked files in the next commit",
                )],
            )
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The header already counts the selection; buttons just name
        // their move (eligibility still drives sensitivity).
        let writable = !self.read_only();
        let build = |(label, count, op, tip): &Op, toward_commit: bool| {
            let button = gtk::Button::builder()
                .label(*label)
                .sensitive(*count > 0 && writable)
                .tooltip_text(*tip)
                .build();
            if toward_commit {
                button.add_css_class("suggested-action");
            }
            let weak = Rc::downgrade(self);
            let op: &'static str = op;
            button.connect_clicked(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.run_selection_op(op);
                }
            });
            button
        };
        for op in &left_ops {
            row.append(&build(op, false));
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        row.append(&spacer);
        for op in &right_ops {
            row.append(&build(op, true));
        }
        content.append(&row);
        // Committing joins the bulk ops in the Staged view: the composer,
        // enabled only while the selection is the whole index (a commit
        // takes everything staged; the blocker banner explains).
        if staged_view {
            if let Some(parent) = self.commit_overlay.parent() {
                if let Ok(parent) = parent.downcast::<gtk::Box>() {
                    parent.remove(&self.commit_overlay);
                }
            }
            content.append(&self.commit_overlay);
            self.pane.set(PaneKind::Staged);
            self.refresh_commit_pane_state();
        } else {
            self.pane.set(PaneKind::Selection);
        }
    }

    /// Apply one bulk op to the eligible selected files, off-thread.
    fn run_selection_op(self: &Rc<Self>, op: &'static str) {
        if self.refuse_read_only() {
            return;
        }
        let selected: Vec<PathBuf> = self.selection.borrow().iter().cloned().collect();
        let Some(root) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let op_root = root.clone();
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = GitWorkspace::discover(&op_root)
                    .ok_or_else(|| "not a git repository".to_string())?;
                let rels: Vec<PathBuf> = selected
                    .iter()
                    .filter_map(|p| p.strip_prefix(git.workdir()).ok().map(|r| r.to_path_buf()))
                    .collect();
                match op {
                    "stage" => {
                        for rel in &rels {
                            git.stage(rel).map_err(|e| e.to_string())?;
                        }
                    }
                    "unstage" => {
                        for rel in &rels {
                            git.unstage(rel).map_err(|e| e.to_string())?;
                        }
                    }
                    "stash" => {
                        let mut args: Vec<String> = vec![
                            "-C".into(),
                            git.workdir().display().to_string(),
                            "stash".into(),
                            "push".into(),
                            "--include-untracked".into(),
                            "-m".into(),
                            "taste-ide selection".into(),
                            "--".into(),
                        ];
                        args.extend(rels.iter().map(|r| r.display().to_string()));
                        let out = std::process::Command::new("git")
                            .args(&args)
                            .output()
                            .map_err(|e| e.to_string())?;
                        if !out.status.success() {
                            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
                        }
                    }
                    "keep-yours" | "take-remote" => {
                        // A rebase inverts git's ours/theirs: replaying
                        // YOUR commits onto the remote tip makes --ours
                        // the remote side. The buttons speak meaning;
                        // this maps meaning back to git's flag.
                        let keep_yours = op == "keep-yours";
                        let side = match (git.rebase_in_progress(), keep_yours) {
                            (true, true) | (false, false) => "--theirs",
                            (true, false) | (false, true) => "--ours",
                        };
                        let mut args: Vec<String> = vec![
                            "-C".into(),
                            git.workdir().display().to_string(),
                            "checkout".into(),
                            side.into(),
                            "--".into(),
                        ];
                        args.extend(rels.iter().map(|r| r.display().to_string()));
                        let out = std::process::Command::new("git")
                            .args(&args)
                            .output()
                            .map_err(|e| e.to_string())?;
                        if !out.status.success() {
                            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
                        }
                        // Taking a side resolves: mark it so.
                        for rel in &rels {
                            git.stage(rel).map_err(|e| e.to_string())?;
                        }
                    }
                    "mark-resolved" => {
                        for rel in &rels {
                            git.stage(rel).map_err(|e| e.to_string())?;
                        }
                    }
                    "unstash" => {
                        let entries = git.stash_entries().map_err(|e| e.to_string())?;
                        for rel in &rels {
                            let Some(index) = entries.iter().position(|paths| paths.contains(rel))
                            else {
                                continue;
                            };
                            let (program, args) = git.unstash_file_command(index, rel);
                            let out = std::process::Command::new(&program)
                                .args(&args)
                                .output()
                                .map_err(|e| e.to_string())?;
                            if !out.status.success() {
                                return Err(String::from_utf8_lossy(&out.stderr)
                                    .trim()
                                    .to_string());
                            }
                        }
                    }
                    _ => {}
                }
                Ok::<_, String>(())
            });
            let Ok(result) = handle.await else { return };
            let failed = result.is_err();
            match result {
                Ok(()) => events.publish(Event::Toast(match op {
                    "stage" => "Staged".to_string(),
                    "unstage" => "Unstaged".to_string(),
                    "stash" => "Stashed".to_string(),
                    "unstash" => "Unstashed — back in the working tree".to_string(),
                    "keep-yours" => "Resolved with your version".to_string(),
                    "take-remote" => "Resolved with the remote version".to_string(),
                    "mark-resolved" => "Marked resolved".to_string(),
                    _ => format!("{op}: done"),
                })),
                Err(e) => events.publish(Event::Toast(format!("{op} failed: {e}"))),
            }
            if let Some(tree) = weak.upgrade() {
                if failed {
                    // Nothing moved: the selection and its pane stay honest.
                    return;
                }
                tree.selection.borrow_mut().clear();
                tree.close_intervention();
                match op {
                    // Right-moves (toward the commit) follow the files to
                    // the view where they landed; left-moves stay put.
                    "stage" => tree.staged_toggle.set_active(true),
                    "unstash" => tree.dirty_toggle.set_active(true),
                    _ => {}
                }
                // A successful op always changed status, so the refresh
                // re-renders the list and rebuilds the pane.
                tree.refresh_status();
            }
        });
    }

    /// Gray the composer (and show the blocker banner) unless every staged
    /// file is selected.
    fn refresh_commit_pane_state(&self) {
        if self.pane.get() != PaneKind::Staged {
            return;
        }
        let workdir = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf());
        let selection = self.selection.borrow();
        let all_selected = self
            .status
            .borrow()
            .iter()
            .filter(|(_, state)| **state == FileState::Staged)
            .all(|(rel, _)| {
                let abs = workdir
                    .as_ref()
                    .map(|w| w.join(rel))
                    .unwrap_or_else(|| rel.clone());
                selection.contains(&abs)
            });
        drop(selection);
        let committable = all_selected && !self.read_only();
        self.commit_box.set_sensitive(committable);
        self.commit_box
            .set_opacity(if committable { 1.0 } else { 0.4 });
        self.commit_blocker.set_visible(!all_selected);
    }

    /// Open the intervention panel with a title; returns the content box.
    /// Replaces any previous intervention. Closing cancels.
    fn open_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
        self.open_pane(title, true)
    }

    fn open_pane(self: &Rc<Self>, title: &str, closable: bool) -> gtk::Box {
        // Ad-hoc until a selection pane claims otherwise after building.
        self.pane.set(PaneKind::Adhoc);
        while let Some(child) = self.intervention.first_child() {
            self.intervention.remove(&child);
        }
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_start(10);
        header.set_margin_end(6);
        let label = gtk::Label::builder()
            .label(title)
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        header.append(&label);
        if closable {
            let close = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Cancel")
                .css_classes(["flat", "circular"])
                .build();
            let weak = Rc::downgrade(self);
            close.connect_clicked(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.dismiss_intervention();
                }
            });
            header.append(&close);
        }
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
        self.pane.set(PaneKind::None);
        self.intervention_file.borrow_mut().take();
        self.intervention.set_visible(false);
        while let Some(child) = self.intervention.first_child() {
            self.intervention.remove(&child);
        }
    }

    /// An ad-hoc pane ending (closed, or its flow completed): the filter
    /// views get their selection-derived pane back — the Staged view's
    /// resting pane always, the others' ops pane if files are still
    /// checked. Callers that just changed git state still refresh_status;
    /// this covers the refreshes the unchanged-guard swallows.
    fn dismiss_intervention(self: &Rc<Self>) {
        self.close_intervention();
        if self.filters_active() {
            self.selection_intervention();
        }
    }

    /// Discard = destructive, so it confirms — in the panel, not a dialog.
    fn discard_intervention(self: &Rc<Self>, abs: PathBuf) {
        if self.refuse_read_only() {
            return;
        }
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = self.open_intervention(&format!("Discard — {name}"));
        content.append(
            &gtk::Label::builder()
                .label(format!(
                    "Restore “{name}” to its last committed state? \
                     Unstaged changes are lost."
                ))
                .css_classes(["caption"])
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        let button = gtk::Button::builder()
            .label("Discard Changes")
            .css_classes(["destructive-action"])
            .halign(gtk::Align::End)
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            let Some(tree) = weak.upgrade() else { return };
            let Some(workdir) = tree
                .git
                .borrow()
                .as_ref()
                .map(|g| g.workdir().to_path_buf())
            else {
                return;
            };
            let rel = abs.strip_prefix(&workdir).unwrap_or(&abs).to_path_buf();
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            let name = rel.display().to_string();
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    GitWorkspace::discover(&workdir)
                        .ok_or_else(|| "not a git repository".to_string())
                        .and_then(|git| git.restore_file(&rel).map_err(|e| e.to_string()))
                });
                let Ok(result) = handle.await else { return };
                match result {
                    Ok(()) => events.publish(Event::Toast(format!("Discarded — {name}"))),
                    Err(e) => events.publish(Event::Toast(format!("Discard failed: {e}"))),
                }
                if let Some(tree) = weak.upgrade() {
                    tree.dismiss_intervention();
                    tree.refresh_status();
                }
            });
        });
        content.append(&button);
    }

    /// Stash one file, with an editable stash message.
    fn stash_intervention(self: &Rc<Self>, abs: PathBuf) {
        if self.refuse_read_only() {
            return;
        }
        let Some(workdir) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        let rel = abs.strip_prefix(&workdir).unwrap_or(&abs).to_path_buf();
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = self.open_intervention(&format!("Stash — {name}"));
        let entry = gtk::Entry::builder()
            .text(format!("taste-ide: {}", rel.display()))
            .hexpand(true)
            .build();
        let confirm = gtk::Button::builder()
            .label("Stash")
            .css_classes(["suggested-action"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&confirm);
        content.append(&row);
        let weak = Rc::downgrade(self);
        let run = move |message: String| {
            let Some(tree) = weak.upgrade() else { return };
            let Some((program, args)) = tree
                .git
                .borrow()
                .as_ref()
                .map(|git| git.stash_file_command(&rel, &message))
            else {
                return;
            };
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    std::process::Command::new(&program)
                        .args(&args)
                        .output()
                        .map_err(|e| e.to_string())
                        .and_then(|out| {
                            if out.status.success() {
                                Ok(())
                            } else {
                                Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
                            }
                        })
                });
                let Ok(result) = handle.await else { return };
                match result {
                    Ok(()) => events.publish(Event::Toast("Stashed".into())),
                    Err(e) => events.publish(Event::Toast(format!("Stash failed: {e}"))),
                }
                if let Some(tree) = weak.upgrade() {
                    tree.dismiss_intervention();
                    tree.refresh_status();
                }
            });
        };
        {
            let run = run.clone();
            let entry2 = entry.clone();
            confirm.connect_clicked(move |_| run(entry2.text().to_string()));
        }
        entry.connect_activate(move |entry| run(entry.text().to_string()));
    }

    /// Add-to-ignores: an editable expression with live feedback (match
    /// count, and a warning when it misses the file it started from).
    fn ignore_intervention(self: &Rc<Self>, abs: PathBuf) {
        if self.refuse_read_only() {
            return;
        }
        let Some(workdir) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        let rel = abs.strip_prefix(&workdir).unwrap_or(&abs).to_path_buf();
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = self.open_intervention(&format!("Add to Ignores — {name}"));
        let entry = gtk::Entry::builder()
            .text(format!("/{}", rel.display()))
            .hexpand(true)
            .build();
        let confirm = gtk::Button::builder()
            .label("Add")
            .css_classes(["suggested-action"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&confirm);
        let status = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .wrap(true)
            .build();
        content.append(&row);
        content.append(&status);

        let generation = Rc::new(std::cell::Cell::new(0u64));
        {
            let weak = Rc::downgrade(self);
            let status = status.clone();
            let workdir = workdir.clone();
            let target = rel.clone();
            let generation = generation.clone();
            let evaluate = move |entry: &gtk::Entry| {
                let Some(tree) = weak.upgrade() else { return };
                let expr = entry.text().trim().to_string();
                let current = generation.get() + 1;
                generation.set(current);
                let files = tree.index.borrow().clone();
                let workdir = workdir.clone();
                let target = target.clone();
                let target_label = target.display().to_string();
                let status = status.clone();
                let generation = generation.clone();
                glib::spawn_future_local(async move {
                    let handle = crate::runtime::runtime().spawn_blocking(move || {
                        let mut builder = ignore::gitignore::GitignoreBuilder::new(&workdir);
                        builder
                            .add_line(None, &expr)
                            .map_err(|e| format!("invalid pattern: {e}"))?;
                        let matcher = builder.build().map_err(|e| e.to_string())?;
                        let target_hit = matcher.matched(&target, false).is_ignore();
                        let count = files
                            .map(|files| {
                                files
                                    .iter()
                                    .filter(|path| {
                                        path.strip_prefix(&workdir)
                                            .map(|rel| matcher.matched(rel, false).is_ignore())
                                            .unwrap_or(false)
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        Ok::<_, String>((target_hit, count))
                    });
                    let Ok(result) = handle.await else { return };
                    if generation.get() != current {
                        return; // superseded by a newer keystroke
                    }
                    match result {
                        Ok((true, count)) => {
                            status.remove_css_class("warning");
                            status.set_label(&format!(
                                "Matches {count} file{} (including this one)",
                                if count == 1 { "" } else { "s" }
                            ));
                        }
                        Ok((false, count)) => {
                            status.add_css_class("warning");
                            status.set_label(&format!(
                                "Does not match {target_label} — matches {count} file{}",
                                if count == 1 { "" } else { "s" }
                            ));
                        }
                        Err(e) => {
                            status.add_css_class("warning");
                            status.set_label(&e);
                        }
                    }
                });
            };
            let eval2 = evaluate.clone();
            entry.connect_changed(move |entry| eval2(entry));
            evaluate(&entry);
        }

        let weak = Rc::downgrade(self);
        let save = move |entry: &gtk::Entry| {
            let Some(tree) = weak.upgrade() else { return };
            let expr = entry.text().trim().to_string();
            if expr.is_empty() {
                return;
            }
            let gitignore = workdir.join(".gitignore");
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    let mut content = std::fs::read_to_string(&gitignore).unwrap_or_default();
                    if !content.lines().any(|line| line.trim() == expr) {
                        if !content.is_empty() && !content.ends_with('\n') {
                            content.push('\n');
                        }
                        content.push_str(&expr);
                        content.push('\n');
                        std::fs::write(&gitignore, content).map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(())
                });
                let Ok(result) = handle.await else { return };
                match result {
                    Ok(()) => events.publish(Event::Toast("Added to .gitignore".into())),
                    Err(e) => events.publish(Event::Toast(format!(".gitignore: {e}"))),
                }
                // Reprocess: status, rows, and the search index all see
                // the new ignore rule.
                if let Some(tree) = weak.upgrade() {
                    tree.dismiss_intervention();
                    tree.refresh_status();
                    tree.rebuild_index();
                }
            });
        };
        {
            let save = save.clone();
            let entry2 = entry.clone();
            confirm.connect_clicked(move |_| save(&entry2));
        }
        entry.connect_activate(move |entry| save(entry));
    }

    /// Fetch, then rebase onto the remote tip — the full sync, conflicts
    /// and all (a paused rebase gets the Conflicts view and the
    /// Continue/Abort pair). Push stays a separate, deliberate action;
    /// count freshness is the background fetch's job, not a button's.
    fn sync(self: &Rc<Self>) {
        if self.refuse_read_only() {
            return;
        }
        let Some((fetch, fetch_issues, rebase_command)) = self.git.borrow().as_ref().map(|git| {
            (
                git.fetch_command(),
                git.fetch_issues_command(),
                git.rebase_command(),
            )
        }) else {
            return;
        };
        let root = self.workspace.root().to_path_buf();
        self.sync_button.set_sensitive(false);
        self.sync_label.set_label("syncing…");
        self.last_fetch.set(Some(std::time::Instant::now()));
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            for (label, (program, args)) in [("fetch", fetch), ("rebase", rebase_command)] {
                let handle = crate::runtime::runtime().spawn(run_git_step(program, args));
                let failure = match handle.await {
                    Ok(Ok(_)) => None,
                    Ok(Err(reason)) => Some(reason),
                    Err(_) => Some("interrupted".to_string()),
                };
                if let Some(reason) = failure {
                    events.publish(Event::Toast(format!("{label} failed: {reason}")));
                    break;
                }
            }
            // The issues ref, second and quietly. A remote that has never
            // seen an issue makes this fail, which is the normal case
            // before the first push, so its failure is silence rather than
            // a toast about something the user did not ask for.
            let fetched = crate::runtime::runtime()
                .spawn(run_git_step(fetch_issues.0, fetch_issues.1))
                .await;
            if matches!(fetched, Ok(Ok(_))) {
                // Fast-forward or nothing: two machines that both moved the
                // ref get a sentence, not a merge UI.
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    taste_git::GitWorkspace::discover(&root)
                        .map(|git| git.reconcile_issues())
                        .transpose()
                        .ok()
                        .flatten()
                });
                if let Ok(Some(sync)) = handle.await {
                    if let Some(warning) = sync.warning() {
                        events.publish(Event::Toast(warning));
                    }
                }
            }
            if let Some(tree) = weak.upgrade() {
                tree.end_sync_op();
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    fn abort_rebase(self: &Rc<Self>) {
        if self.refuse_read_only() {
            return;
        }
        let Some((program, args)) = self
            .git
            .borrow()
            .as_ref()
            .map(|git| git.rebase_abort_command())
        else {
            return;
        };
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let _ = tokio::process::Command::new(&program)
                .args(&args)
                .output()
                .await;
            events.publish(Event::GitStatusChanged);
        });
    }

    /// `git rebase --continue`: git itself checks the preconditions
    /// (everything resolved and marked), and its refusal — usually
    /// "unmerged files" — comes through as the toast.
    fn continue_rebase(self: &Rc<Self>) {
        if self.refuse_read_only() {
            return;
        }
        let Some((program, args)) = self
            .git
            .borrow()
            .as_ref()
            .map(|git| git.rebase_continue_command())
        else {
            return;
        };
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let output = tokio::process::Command::new(&program)
                .args(&args)
                .output()
                .await;
            match output {
                Ok(out) if out.status.success() => {
                    events.publish(Event::Toast("Rebase complete".into()));
                }
                // Non-zero also means "stopped at the NEXT conflict":
                // either way git's first line says what's up, and the
                // refreshed Conflicts view shows where.
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let line = stderr
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("stopped again");
                    events.publish(Event::Toast(format!("Rebase: {line}")));
                }
                Err(e) => events.publish(Event::Toast(format!("Rebase continue failed: {e}"))),
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    /// Ahead/behind counts only tell the truth if someone consults the
    /// remote: status refreshes piggyback a fetch, throttled hard (they
    /// fire on every save; the network must not), and quiet on failure —
    /// offline means stale counts, not toast spam.
    fn background_fetch(self: &Rc<Self>) {
        // Never for a watched environment: a clone's `origin` is a host
        // path no container has mounted, and fetching another
        // environment's repository on its behalf is not watching.
        if self.read_only() {
            return;
        }
        const FETCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
        if self
            .last_fetch
            .get()
            .is_some_and(|at| at.elapsed() < FETCH_INTERVAL)
        {
            return;
        }
        let Some((program, args)) = self.git.borrow().as_ref().map(|g| g.fetch_command()) else {
            return;
        };
        self.last_fetch.set(Some(std::time::Instant::now()));
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            match tokio::process::Command::new(&program)
                .args(&args)
                .output()
                .await
            {
                // The refresh this triggers is throttled by last_fetch, so
                // fetch → refresh → fetch can't loop.
                Ok(out) if out.status.success() => events.publish(Event::GitStatusChanged),
                Ok(out) => tracing::debug!(
                    "background fetch: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => tracing::debug!("background fetch failed: {e}"),
            }
        });
    }

    fn repo_relative(&self, path: &Path) -> Option<PathBuf> {
        let git = self.git.borrow();
        let workdir = git.as_ref()?.workdir().to_path_buf();
        path.strip_prefix(workdir).ok().map(Path::to_path_buf)
    }

    fn state_of(&self, node: &FileNode) -> FileState {
        let Some(rel) = self.repo_relative(&node.path) else {
            return FileState::Clean;
        };
        state_for(&self.status.borrow(), &rel, node.is_dir)
    }

    /// Build the (fresh) tree model. Called on construction and when the
    /// ignored-files toggle flips. Git-status changes only restyle rows.
    fn rebuild(self: &Rc<Self>) {
        self.close_open_menu();
        // A new list means new rows; the handles for the old one address
        // widgets that are about to be dropped (and, after an `aim_at`,
        // paths relative to a different repository).
        self.rows.borrow_mut().clear();
        let show_ignored = *self.show_ignored.borrow();
        // Search shapes the tree: matches-only filters to matching files
        // (autoexpanded); ghost mode keeps every file and dims the rest.
        let search = self.search_view.borrow().clone();
        let ghost_mode = self.search_ghosts_toggle.is_active();
        let filter: Option<Rc<HashSet<PathBuf>>> = match (&search, ghost_mode) {
            (Some(view), false) => Some(view.visible.clone()),
            _ => None,
        };
        let ghosts = if search.is_some() || self.read_only() {
            // Template suggestions are noise in search results — and an
            // offer to create a file is a lie in a read-only view.
            Vec::new()
        } else {
            ghost_candidates(self.workspace.root())
        };
        let view_root = self.view_root();
        let root_store = build_dir_store(&view_root, show_ignored, &ghosts, filter.as_ref());
        let autoexpand = filter.is_some();
        let child_filter = filter.clone();
        let tree_model = gtk::TreeListModel::new(root_store, false, autoexpand, move |item| {
            let node = item.downcast_ref::<BoxedAnyObject>()?.borrow::<FileNode>();
            if node.is_dir {
                Some(build_dir_store(&node.path, show_ignored, &[], child_filter.as_ref()).upcast())
            } else {
                None
            }
        });
        let selection = gtk::SingleSelection::new(Some(tree_model));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&gtk::TreeExpander::new()));
        });
        let tree = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(tree) = tree.upgrade() else { return };
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
            let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
            expander.set_list_row(Some(&row));
            let node = row
                .item()
                .and_downcast::<BoxedAnyObject>()
                .unwrap()
                .borrow::<FileNode>()
                .clone();
            expander.set_child(Some(&tree.build_row(&node)));
        });

        let list = gtk::ListView::new(Some(selection), Some(factory));
        // Single click opens files / toggles folders (Builder-style);
        // double-click-only activation reads as broken.
        list.set_single_click_activate(true);
        let tree = Rc::downgrade(self);
        list.connect_activate(move |list, position| {
            let Some(tree) = tree.upgrade() else { return };
            let model = list.model().unwrap();
            let row = model
                .item(position)
                .and_downcast::<gtk::TreeListRow>()
                .unwrap();
            let node = row
                .item()
                .and_downcast::<BoxedAnyObject>()
                .unwrap()
                .borrow::<FileNode>()
                .clone();
            if node.ghost {
                tree.create_ghost(&node.path);
            } else if node.is_dir {
                row.set_expanded(!row.is_expanded());
            } else {
                let matched = tree
                    .search_view
                    .borrow()
                    .as_ref()
                    .is_some_and(|view| view.hits.contains_key(&node.path));
                if matched {
                    // Picking a matching file opens the match list below.
                    tree.matches_intervention(node.path.clone());
                } else {
                    tree.open(node.path.clone(), None);
                }
            }
        });

        self.list_holder.set_child(Some(&list));
    }

    /// One row's content: icon, name, git badge, stage/unstage toggle.
    fn build_row(self: &Rc<Self>, node: &FileNode) -> gtk::Box {
        if node.ghost {
            return self.build_ghost_row(node);
        }
        let state = self.state_of(node);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // Directories keep the folder glyph; files get their GNOME
        // content-type icon (name-based guess — no IO).
        let icon = if node.is_dir {
            gtk::Image::from_icon_name("folder-symbolic")
        } else {
            gtk::Image::from_gicon(&crate::editor::file_type_icon(&node.path))
        };
        let name = node
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let label = gtk::Label::builder()
            .label(&name)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();

        let (badge, css) = state_style(state);
        let badge_label = gtk::Label::builder().label(badge).build();
        if let Some(css) = css {
            badge_label.add_css_class(css);
            label.add_css_class(css);
        }

        row.append(&icon);
        row.append(&label);
        row.append(&badge_label);

        // Search annotations: per-file match count; in ghost mode the
        // non-matching rows stay but fade.
        if let Some(view) = self.search_view.borrow().as_ref() {
            if !node.is_dir {
                if let Some(hits) = view.hits.get(&node.path) {
                    let count = gtk::Label::builder()
                        .label(hits.len().to_string())
                        .css_classes(["caption", "accent"])
                        .build();
                    row.append(&count);
                } else if view.pinned.as_deref() == Some(node.path.as_path()) {
                    // The current file rides along with zero hits: full
                    // opacity, an honest zero.
                    let count = gtk::Label::builder()
                        .label("0")
                        .css_classes(["caption", "dim-label"])
                        .build();
                    row.append(&count);
                } else {
                    row.set_opacity(0.4);
                }
            } else if !view.visible.contains(&node.path) {
                row.set_opacity(0.4);
            }
        }

        // The lock, for its two reasons — the same affordance, because it
        // means the same thing to the user: you are looking, not editing.
        //
        // Watching wins where both apply: "this is calm-1's file" is the
        // more useful answer than "the devcontainer is down".
        let watched = self.watching.borrow().as_ref().map(|(env, _)| env.clone());
        let safe_mode = !self.workspace.exec.is_container();
        let lock_reason = match watched {
            Some(env) => Some(format!(
                "Read-only: {env}'s checkout. Its agent is working here — review \
                 what it publishes, or take over its chat."
            )),
            None if safe_mode
                && !taste_core::policy::write_allowed(self.workspace.root(), true, &node.path) =>
            {
                Some(
                    "Read-only until the project's environment is running — only \
                     devcontainer setup and workspace dotfiles are editable"
                        .to_string(),
                )
            }
            None => None,
        };
        let locked = lock_reason.is_some();
        if let Some(reason) = lock_reason {
            let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
            lock.add_css_class("dim-label");
            lock.set_tooltip_text(Some(&reason));
            row.append(&lock);
            label.add_css_class("dim-label");
        }

        // Addressable from here on: a later status tick restyles this row
        // rather than building another one over it.
        {
            let mut rows = self.rows.borrow_mut();
            // Rows the list has recycled away leave dead handles behind.
            // Pruned in bulk rather than per bind, and by the restyle pass
            // itself; this is only the bound on a long scroll between ticks.
            if rows.len() >= 512 {
                rows.retain(|_, handle| handle.badge.upgrade().is_some());
            }
            rows.insert(
                node.path.clone(),
                RowHandle {
                    is_dir: node.is_dir,
                    state: std::cell::Cell::new(state),
                    locked,
                    label: label.downgrade(),
                    badge: badge_label.downgrade(),
                },
            );
        }

        // Right-click: file operations + stage/unstage. Kept out of the row
        // itself so every row is uniform icon+label (native, even spacing).
        {
            let context = gtk::GestureClick::builder().button(3).build();
            let tree = Rc::downgrade(self);
            let node = node.clone();
            let row_weak = row.downgrade();
            context.connect_released(move |_, _, _, _| {
                if let (Some(tree), Some(row)) = (tree.upgrade(), row_weak.upgrade()) {
                    tree.show_context_menu(&row, &node);
                }
            });
            row.add_controller(context);
        }
        row
    }

    /// A ghost: config the workspace could have, drawn faint, one activation
    /// away from existing.
    fn build_ghost_row(self: &Rc<Self>, node: &FileNode) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::from_icon_name("list-add-symbolic");
        icon.add_css_class("dim-label");
        let rel = node
            .path
            .strip_prefix(self.view_root())
            .unwrap_or(&node.path)
            .display()
            .to_string();
        let label = gtk::Label::builder()
            .use_markup(true)
            .label(format!("<i>{}</i>", glib::markup_escape_text(&rel)))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label"])
            .build();
        row.set_tooltip_text(Some(&format!("Create {rel}")));
        row.append(&icon);
        row.append(&label);
        row
    }

    /// Materialize a ghost, then open it. If the user keeps templates for
    /// this file name (`~/.config/taste-ide/templates/<file-name>/…`), offer
    /// them alongside the built-in default; otherwise create silently.
    pub fn create_ghost(self: &Rc<Self>, path: &Path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let templates = taste_core::templates::templates_for(&name);
        if templates.is_empty() {
            self.materialize_ghost(path, ghost_template(Some(&name)).to_string());
            return;
        }

        let dialog = adw::AlertDialog::new(
            Some(&format!("Create {name}")),
            Some("Choose a template (from ~/.config/taste-ide/templates)."),
        );
        let mut responses: Vec<(String, String)> = vec![
            ("cancel".into(), "Cancel".into()),
            ("builtin".into(), "Built-in".into()),
        ];
        for (index, template) in templates.iter().enumerate() {
            responses.push((format!("t{index}"), template.name.clone()));
        }
        let response_refs: Vec<(&str, &str)> = responses
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        dialog.add_responses(&response_refs);
        dialog.set_close_response("cancel");
        let tree = Rc::downgrade(self);
        let path = path.to_path_buf();
        dialog.connect_response(None, move |_, response| {
            let Some(tree) = tree.upgrade() else { return };
            let content = if response == "builtin" {
                Some(ghost_template(path.file_name().and_then(|n| n.to_str())).to_string())
            } else if let Some(index) = response.strip_prefix('t') {
                index
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| templates.get(i))
                    .and_then(|t| std::fs::read_to_string(&t.path).ok())
            } else {
                None // cancel
            };
            if let Some(content) = content {
                tree.materialize_ghost(&path, content);
            }
        });
        dialog.present(Some(&self.widget));
    }

    fn materialize_ghost(self: &Rc<Self>, path: &Path, content: String) {
        // Nothing touches disk here: the file opens as an unsaved buffer
        // and pops into existence in the tree when the user saves it.
        self.workspace.events.publish(Event::CreateFileRequested {
            path: path.to_path_buf(),
            content,
        });
    }

    fn toggle_stage(self: &Rc<Self>, path: &Path, currently_staged: bool) {
        if self.refuse_read_only() {
            return;
        }
        let Some(rel) = self.repo_relative(path) else {
            return;
        };
        let Some(root) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        // Index writes are IO: off the main thread like every other git op.
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let toggle_rel = rel.clone();
            let result = tokio::task::spawn_blocking(move || {
                let git = GitWorkspace::discover(&root)
                    .ok_or_else(|| "not a git repository".to_string())?;
                if currently_staged {
                    git.unstage(&toggle_rel).map_err(|e| e.to_string())
                } else {
                    git.stage(&toggle_rel).map_err(|e| e.to_string())
                }
            })
            .await;
            if let Ok(Err(e)) = result {
                events.publish(Event::Toast(format!(
                    "Staging {} failed: {e}",
                    rel.display()
                )));
                return;
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    fn commit(self: &Rc<Self>) {
        let message = self.commit_entry.text().to_string();
        if message.trim().is_empty() {
            // Not a silent no-op, and not a casual yes: confirm in the
            // panel (never a modal), with writing a message — or asking
            // the sparkle — as the easy path.
            let content = self.open_intervention("Commit without a message?");
            content.append(
                &gtk::Label::builder()
                    .label(
                        "The message is empty. Blank history is miserable to \
                         dig through later — the star button beside the \
                         message field drafts one.",
                    )
                    .css_classes(["caption"])
                    .xalign(0.0)
                    .wrap(true)
                    .build(),
            );
            let anyway = gtk::Button::builder()
                .label("Commit Anyway")
                .css_classes(["destructive-action"])
                .halign(gtk::Align::End)
                .build();
            let weak = Rc::downgrade(self);
            anyway.connect_clicked(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                tree.dismiss_intervention();
                tree.commit_with("(no message)");
            });
            content.append(&anyway);
            self.commit_entry.grab_focus();
            return;
        }
        self.commit_with(&message);
    }

    fn commit_with(self: &Rc<Self>, message: &str) {
        if self.refuse_read_only() {
            return;
        }
        let Some(root) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| g.workdir().to_path_buf())
        else {
            return;
        };
        // Writing the commit is IO: off the main thread; the entry clears
        // only once the commit actually exists.
        let events = self.workspace.events.clone();
        let message = message.to_string();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = GitWorkspace::discover(&root)
                    .ok_or_else(|| "not a git repository".to_string())?;
                git.commit(&message).map(|_| ()).map_err(|e| e.to_string())
            });
            let Ok(result) = handle.await else { return };
            match result {
                Ok(()) => {
                    if let Some(tree) = weak.upgrade() {
                        tree.commit_entry.set_text("");
                    }
                    events.publish(Event::GitStatusChanged);
                }
                Err(e) => events.publish(Event::Toast(format!("Commit failed: {e}"))),
            }
        });
    }

    /// Hand the sync row to `button`'s spinner until the operation ends.
    fn begin_sync_op(&self, button: &gtk::Button) {
        self.sync_busy.set(true);
        button_busy(button);
        // The others are meaningless until this one lands.
        for other in [&self.push_button, &self.pull_button, &self.sync_button] {
            if other != button {
                other.set_sensitive(false);
            }
        }
    }

    /// Give it back. Cleared BEFORE the refresh is asked for, so the row
    /// rebuilds straight to the state the operation produced.
    fn end_sync_op(&self) {
        self.sync_busy.set(false);
    }

    fn push(self: &Rc<Self>) {
        // Push stays user-only and host-side, and it pushes the USER's
        // checkout. There is no reading of an agent's clone that turns
        // into publishing it to the world.
        if self.refuse_read_only() {
            return;
        }
        // The issues ref rides along, and only here. It is byte-identical
        // to the plain push until the ref exists, so a workspace that has
        // never filed an issue pushes exactly what it always did — and the
        // queue reaches a remote on the USER's action, never an agent's.
        let Some((program, args)) = self
            .git
            .borrow()
            .as_ref()
            .map(|git| git.push_command_including_issues())
        else {
            return;
        };
        let events = self.workspace.events.clone();
        // Push runs on the host with the user's own credential helpers.
        let spec = {
            let exec = taste_core::ExecContext::host();
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            exec.resolve(&program, &arg_refs, false)
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn(run_git_step(spec.program, spec.args));
            match handle.await {
                Ok(Ok(_)) => events.publish(Event::Toast("Pushed".into())),
                Ok(Err(reason)) => {
                    events.publish(Event::Toast(format!("Push failed: {reason}")));
                }
                Err(_) => events.publish(Event::Toast("Push failed: interrupted".into())),
            }
            if let Some(tree) = weak.upgrade() {
                tree.end_sync_op();
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    /// Restyle rows after a status change without rebuilding the tree
    /// (expansion state is preserved; rows re-bind lazily on redraw).
    /// TASTE_PROBE_CHECK only: aim the git views at a branch, so a
    /// headless screenshot shows the review face of this pane rather than
    /// the tree everything else already covers.
    pub fn seed_review_for_probe(self: &Rc<Self>, branch: &str, target: &str) {
        self.clone()
            .open_review(branch.to_string(), target.to_string());
    }

    /// TASTE_PROBE_CHECK only: aim the tree at an "environment" so a
    /// headless screenshot shows watching — the panel's tint and lock, the
    /// locks on every row, the disabled git controls.
    ///
    /// The rendering's whole input is the target pair, so pointing it at
    /// the workspace's own path shows exactly what a real clone would; the
    /// clone would only cost a `git clone`. What is fabricated here is the
    /// binding, not the drawing.
    pub fn seed_watching_for_probe(self: &Rc<Self>, env: &str) {
        let Ok(env) = taste_core::environment::EnvironmentId::parse(env) else {
            return;
        };
        let root = self.workspace.root().to_path_buf();
        self.aim_at(Some((env, root)));
    }

    pub fn on_git_status_changed(self: &Rc<Self>) {
        // apply_status restyles the rows once the fresh map lands.
        self.refresh_status();
    }

    /// Repaint only the rows whose git state actually moved.
    ///
    /// This is the ordinary status tick, and on a watched checkout it is
    /// nearly every tick: the agent writes one file, one badge flips, and
    /// every other row — the one under the pointer included — keeps its
    /// widgets, its prelight, its tooltip and its open context menu.
    ///
    /// Row MEMBERSHIP never depends on git status in the tree view (the
    /// list comes off disk, and files appearing or vanishing arrive as
    /// `FileTreeChanged` → `rebuild`), so restyling is the whole of the
    /// update. Anything that can change what a row shows for another
    /// reason — a mode flip's locks, a new ignore rule — takes the full
    /// rebind instead.
    fn restyle_changed_rows(&self) {
        let mut rows = self.rows.borrow_mut();
        rows.retain(|_, handle| handle.badge.upgrade().is_some());
        // Resolved against the repository open NOW, not the one that was
        // open when the row was built. Both git reassignments rebuild the
        // list anyway, but a row that quietly kept painting itself against
        // a stale workdir would be a silent wrong answer, and this costs
        // one `strip_prefix` per visible row.
        let resolved: Vec<(Option<PathBuf>, &RowHandle)> = rows
            .iter()
            .map(|(path, handle)| (self.repo_relative(path), handle))
            .collect();
        let status = self.status.borrow();
        let folders = aggregate_dir_states(
            &status,
            resolved
                .iter()
                .filter(|(_, handle)| handle.is_dir)
                .filter_map(|(rel, _)| rel.as_deref()),
        );
        for (rel, handle) in &resolved {
            let state = match rel {
                None => FileState::Clean,
                Some(rel) if handle.is_dir => folders
                    .get(rel.as_path())
                    .copied()
                    .unwrap_or(FileState::Clean),
                Some(rel) => status.get(rel).copied().unwrap_or(FileState::Clean),
            };
            let painted = handle.state.get();
            if state == painted {
                continue;
            }
            let (Some(label), Some(badge)) = (handle.label.upgrade(), handle.badge.upgrade())
            else {
                continue;
            };
            if let (_, Some(old)) = state_style(painted) {
                badge.remove_css_class(old);
                // The lock painted dim-label for its own reason; leaving
                // Ignored must not un-dim a locked row.
                if !(handle.locked && old == "dim-label") {
                    label.remove_css_class(old);
                }
            }
            let (text, css) = state_style(state);
            badge.set_label(text);
            if let Some(css) = css {
                badge.add_css_class(css);
                label.add_css_class(css);
            }
            handle.state.set(state);
        }
    }

    fn rebuild_rows_in_place(self: &Rc<Self>) {
        self.close_open_menu();
        // Rebinding every visible row cheaply: toggle the factory. The
        // ListView re-runs bind for visible rows when the factory is reset.
        if let Some(list) = self.list_holder.child().and_downcast::<gtk::ListView>() {
            let factory = list.factory();
            list.set_factory(None::<&gtk::ListItemFactory>);
            list.set_factory(factory.as_ref());
        }
    }
}

/// The badge glyph and CSS class a git state paints. One source of truth
/// for the initial build and for the in-place restyle, so a row built fresh
/// and a row updated under the pointer can never diverge.
fn state_style(state: FileState) -> (&'static str, Option<&'static str>) {
    match state {
        FileState::Clean => ("", None),
        FileState::Modified => ("M", Some("warning")),
        FileState::Staged => ("S", Some("success")),
        FileState::Untracked => ("U", Some("accent")),
        FileState::Conflicted => ("!", Some("error")),
        FileState::Ignored => ("·", Some("dim-label")),
    }
}

/// Directory rows aggregate their subtree: the most interesting child state
/// wins, by this priority. Clean and Ignored contribute nothing — a folder
/// of ignored files is not itself interesting.
fn dir_rank(state: FileState) -> u8 {
    match state {
        FileState::Conflicted => 3,
        FileState::Staged => 2,
        FileState::Modified | FileState::Untracked => 1,
        FileState::Clean | FileState::Ignored => 0,
    }
}

fn rank_state(rank: u8) -> FileState {
    match rank {
        3 => FileState::Conflicted,
        2 => FileState::Staged,
        1 => FileState::Modified,
        _ => FileState::Clean,
    }
}

/// One directory's aggregate, scanning the whole status map.
fn dir_state(status: &HashMap<PathBuf, FileState>, rel: &Path) -> FileState {
    let mut rank = 0;
    for (path, state) in status {
        if path.starts_with(rel) {
            rank = rank.max(dir_rank(*state));
            if rank == 3 {
                break;
            }
        }
    }
    rank_state(rank)
}

/// The state a row paints: a file's own, or a directory's aggregate.
fn state_for(status: &HashMap<PathBuf, FileState>, rel: &Path, is_dir: bool) -> FileState {
    if is_dir {
        dir_state(status, rel)
    } else {
        status.get(rel).copied().unwrap_or(FileState::Clean)
    }
}

/// Aggregate states for MANY directories in one pass over the status map,
/// by walking each changed path's ancestors instead of rescanning the map
/// per directory. The restyle pass wants every visible folder at once, and
/// on a checkout an agent is churning `dirs × status` per tick is real
/// main-thread time; `status × depth` is not.
///
/// Agrees with [`dir_state`] by construction — and by test.
fn aggregate_dir_states<'a>(
    status: &HashMap<PathBuf, FileState>,
    dirs: impl IntoIterator<Item = &'a Path>,
) -> HashMap<&'a Path, FileState> {
    let mut ranks: HashMap<&Path, u8> = dirs.into_iter().map(|dir| (dir, 0)).collect();
    if !ranks.is_empty() {
        for (path, state) in status {
            let rank = dir_rank(*state);
            if rank == 0 {
                continue;
            }
            // `starts_with` is component-wise and includes equality, so the
            // directories a path contributes to are exactly its ancestors.
            for ancestor in path.ancestors() {
                if let Some(slot) = ranks.get_mut(ancestor) {
                    *slot = (*slot).max(rank);
                }
            }
        }
    }
    ranks
        .into_iter()
        .map(|(dir, rank)| (dir, rank_state(rank)))
        .collect()
}

/// List one directory as a ListStore of `FileNode`s, honoring .gitignore
/// (unless `show_ignored`), directories first, then case-insensitive alpha.
/// `ghosts` (workspace root only) are appended last as creation suggestions.
/// Active search results shaped for the tree: per-file hits, and the set
/// of paths (matching files + their ancestor directories) that stay
/// visible in matches-only mode.
struct SearchView {
    hits: HashMap<PathBuf, Vec<taste_core::search::SearchHit>>,
    visible: Rc<HashSet<PathBuf>>,
    /// The editor's active file: always listed (even with zero hits), so
    /// project search doubles as search-within-the-current-file.
    pinned: Option<PathBuf>,
}

fn build_dir_store(
    dir: &Path,
    show_ignored: bool,
    ghosts: &[PathBuf],
    filter: Option<&Rc<HashSet<PathBuf>>>,
) -> gtk::gio::ListStore {
    let store = gtk::gio::ListStore::new::<BoxedAnyObject>();
    let mut walk = ignore::WalkBuilder::new(dir);
    walk.max_depth(Some(1)).hidden(false);
    if show_ignored {
        walk.git_ignore(false).git_exclude(false).parents(false);
    }
    let mut nodes: Vec<FileNode> = walk
        .build()
        .flatten()
        .filter(|entry| entry.path() != dir)
        .filter(|entry| entry.file_name() != ".git")
        .map(|entry| FileNode {
            is_dir: entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
            path: entry.into_path(),
            ghost: false,
        })
        .filter(|node| filter.map(|f| f.contains(&node.path)).unwrap_or(true))
        .collect();
    nodes.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            a.path
                .file_name()
                .map(|n| n.to_ascii_lowercase())
                .cmp(&b.path.file_name().map(|n| n.to_ascii_lowercase()))
        })
    });
    nodes.extend(ghosts.iter().map(|path| FileNode {
        path: path.clone(),
        is_dir: false,
        ghost: true,
    }));
    for node in nodes {
        store.append(&BoxedAnyObject::new(node));
    }
    store
}

/// Allowlisted config files the workspace doesn't have yet — shown as
/// ghosts. All of them are within the safe-mode writable scope, so creation
/// is legitimate in either mode.
fn ghost_candidates(root: &Path) -> Vec<PathBuf> {
    // Shared with the MCP `ide_conventions` tool: one source of truth for
    // the conventional locations.
    taste_core::conventions::conventions(root)
        .into_iter()
        .filter(|c| c.ghost && !c.exists)
        .map(|c| c.path)
        .collect()
}

/// Distill an agent reply into a single-line commit message: first
/// non-empty, non-fence line, unquoted.
fn clean_commit_message(reply: &str) -> String {
    reply
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .map(|line| line.trim_matches(['"', '`', '\'']).to_string())
        .unwrap_or_default()
}

/// Starter content for a ghost, so creation lands somewhere useful.
fn ghost_template(file_name: Option<&str>) -> &'static str {
    match file_name {
        Some("devcontainer.json") => {
            "{\n    // Define the project's devcontainer. taste-ide validates this\n    \
             // config (no privileged flags, mounts stay in the workspace).\n    \
             \"name\": \"dev\",\n    \
             \"image\": \"registry.fedoraproject.org/fedora:44\",\n    \
             \"runArgs\": [\"--userns=keep-id\"],\n    \
             // Services: forwardPorts publishes on localhost (ports >= 1024).\n    \
             // \"forwardPorts\": [8080],\n    \
             // Caches: named volumes only.\n    \
             // \"mounts\": [\"source=build-cache,target=/cache,type=volume\"],\n    \
             // Background services: a systemd-capable image plus\n    \
             // \"runArgs\": [\"--userns=keep-id\", \"--systemd=always\"] and\n    \
             // \"overrideCommand\": false\n}\n"
        }
        Some(".editorconfig") => {
            "root = true\n\n[*]\ncharset = utf-8\nend_of_line = lf\n\
             insert_final_newline = true\ntrim_trailing_whitespace = true\n\
             indent_style = space\nindent_size = 4\n"
        }
        Some(".gitignore") => "# Build artifacts\n",
        Some(".gitattributes") => "* text=auto\n",
        _ => "\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path in a checkout laid out like a Rust workspace.
    fn file_path(files: usize, changed: usize, i: usize) -> PathBuf {
        PathBuf::from(format!(
            "crates/c{}/src/m{}/f{}.rs",
            i % 8,
            i % 40,
            i * files / changed.max(1)
        ))
    }

    /// A checkout an agent is working in: `changed` files in flight.
    fn busy_status(files: usize, changed: usize) -> HashMap<PathBuf, FileState> {
        (0..changed)
            .map(|i| {
                let state = match i % 4 {
                    0 => FileState::Modified,
                    1 => FileState::Staged,
                    2 => FileState::Untracked,
                    _ => FileState::Ignored,
                };
                (file_path(files, changed, i), state)
            })
            .collect()
    }

    /// A viewport's worth of rows: the folders down one expanded branch,
    /// plus the files in it.
    fn visible_rows() -> Vec<(PathBuf, bool)> {
        let mut rows: Vec<(PathBuf, bool)> = (0..8)
            .map(|c| (PathBuf::from(format!("crates/c{c}")), true))
            .chain((0..4).map(|m| (PathBuf::from(format!("crates/c0/src/m{m}")), true)))
            .collect();
        rows.extend((0..28).map(|i| {
            (
                PathBuf::from(format!("crates/c0/src/m0/f{}.rs", i * 7)),
                false,
            )
        }));
        rows
    }

    /// The batch aggregate and the single-directory scan are two
    /// implementations of one rule — the restyle path uses the first and
    /// `build_row` the second, so they must never disagree.
    #[test]
    fn directory_aggregates_agree() {
        let plain = busy_status(2000, 400);
        let mut conflicted = plain.clone();
        conflicted.insert(
            PathBuf::from("crates/c0/src/m1/boom.rs"),
            FileState::Conflicted,
        );
        let rows = visible_rows();
        let dirs: Vec<&Path> = rows
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(path, _)| path.as_path())
            .collect();
        for status in [&plain, &conflicted] {
            let batch = aggregate_dir_states(status, dirs.iter().copied());
            for dir in &dirs {
                assert_eq!(
                    batch.get(dir).copied().unwrap_or(FileState::Clean),
                    dir_state(status, dir),
                    "{}",
                    dir.display()
                );
            }
        }
    }

    /// Conflicts beat staged beats modified; clean and ignored children
    /// leave a folder clean.
    #[test]
    fn directory_aggregate_priority() {
        let dir = Path::new("d");
        let of = |states: &[(&str, FileState)]| {
            let status: HashMap<PathBuf, FileState> = states
                .iter()
                .map(|(name, state)| (PathBuf::from(format!("d/{name}")), *state))
                .collect();
            let batch = aggregate_dir_states(&status, [dir]);
            let single = dir_state(&status, dir);
            assert_eq!(batch.get(dir).copied().unwrap(), single);
            single
        };
        assert_eq!(of(&[("a", FileState::Ignored)]), FileState::Clean);
        assert_eq!(of(&[("a", FileState::Untracked)]), FileState::Modified);
        assert_eq!(
            of(&[("a", FileState::Modified), ("b", FileState::Staged)]),
            FileState::Staged
        );
        assert_eq!(
            of(&[("a", FileState::Staged), ("b", FileState::Conflicted)]),
            FileState::Conflicted
        );
        // Sibling directories do not bleed: "d2" is not under "d".
        let status = HashMap::from([(PathBuf::from("d2/a"), FileState::Conflicted)]);
        assert_eq!(dir_state(&status, dir), FileState::Clean);
        assert_eq!(
            aggregate_dir_states(&status, [dir]).get(dir).copied(),
            Some(FileState::Clean)
        );
    }

    /// A burst of per-path watcher events becomes ONE query.
    #[test]
    fn refresh_gate_folds_a_burst() {
        let gate = RefreshGate::default();
        assert!(gate.request(), "the first request arms the timer");
        for _ in 0..50 {
            assert!(!gate.request(), "the rest fold into it");
        }
        assert!(gate.fire());
        assert!(!gate.finish(), "nothing asked for more while it ran");
    }

    /// Requests arriving during a query are neither dropped (the status
    /// would go stale) nor stacked (a queue that never drains) — they
    /// become exactly one re-run.
    #[test]
    fn refresh_gate_holds_one_trailing_run() {
        let gate = RefreshGate::default();
        assert!(gate.request());
        assert!(gate.fire());
        for _ in 0..20 {
            assert!(!gate.request(), "no second query while one is in flight");
        }
        assert!(gate.finish(), "one re-run is owed");
        assert!(gate.request(), "and it arms the timer again");
        assert!(gate.fire());
        assert!(!gate.finish(), "exactly one, not twenty");
    }

    /// A timer firing while a query is in flight defers rather than
    /// starting a second one.
    #[test]
    fn refresh_gate_never_runs_two_queries() {
        let gate = RefreshGate::default();
        assert!(gate.request());
        assert!(gate.fire());
        assert!(!gate.fire(), "a stray timer must not start a second query");
        assert!(gate.finish());
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-app perf_ -- --ignored --nocapture`
    ///
    /// The git-status tick on a checkout an agent is writing to. The number
    /// that matters is rows TOUCHED: before the differential path existed
    /// every tick reset the factory, so every visible row's widgets were
    /// destroyed and rebuilt — the one under the pointer included, which is
    /// what made hovering a watched environment lag.
    #[test]
    #[ignore]
    fn perf_status_tick_on_a_busy_checkout() {
        const TICKS: usize = 300;
        let rows = visible_rows();
        let dirs: Vec<&Path> = rows
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(path, _)| path.as_path())
            .collect();
        for (files, changed) in [(1000, 120), (3000, 400), (10_000, 1500)] {
            let mut status = busy_status(files, changed);
            let mut painted: Vec<FileState> = rows
                .iter()
                .map(|(path, is_dir)| state_for(&status, path, *is_dir))
                .collect();

            let mut restyled = 0usize;
            let mut differential = std::time::Duration::ZERO;
            let mut naive = std::time::Duration::ZERO;
            for tick in 0..TICKS {
                let flip = if tick % 2 == 0 {
                    FileState::Modified
                } else {
                    FileState::Staged
                };
                // The delta one edit round of an agent actually produces:
                // a few files anywhere in the checkout…
                for d in 0..3 {
                    status.insert(file_path(files, changed, (tick * 3 + d) % changed), flip);
                }
                // …and, for the case that actually costs something, one of
                // them on screen. Every tick, which is pessimistic: most of
                // what an agent writes is not in the viewport at all.
                status.insert(
                    PathBuf::from(format!("crates/c0/src/m0/f{}.rs", (tick % 28) * 7)),
                    // Period coprime with the row count, so the row really
                    // does land on a different state each time round.
                    match tick % 3 {
                        0 => FileState::Modified,
                        1 => FileState::Staged,
                        _ => FileState::Untracked,
                    },
                );

                // What the tick costs now: one pass for every folder, a
                // lookup per file, and widget work only where it moved.
                let start = std::time::Instant::now();
                let folders = aggregate_dir_states(&status, dirs.iter().copied());
                for (index, (path, is_dir)) in rows.iter().enumerate() {
                    let state = if *is_dir {
                        folders
                            .get(path.as_path())
                            .copied()
                            .unwrap_or(FileState::Clean)
                    } else {
                        status.get(path).copied().unwrap_or(FileState::Clean)
                    };
                    if state != painted[index] {
                        painted[index] = state;
                        restyled += 1;
                    }
                }
                differential += start.elapsed();

                // What it cost before: every row rebuilt, and every folder
                // row rescanning the whole status map to do it.
                let start = std::time::Instant::now();
                for (path, is_dir) in &rows {
                    std::hint::black_box(state_for(&status, path, *is_dir));
                }
                naive += start.elapsed();
            }

            println!(
                "status tick: {files:>6} files ({changed:>4} changed), {} rows visible → \
                 before: {:>5} rows rebuilt, {:>8.1?}/tick of state lookup alone; \
                 after: {:>5.2} rows restyled, {:>8.1?}/tick",
                rows.len(),
                rows.len(),
                naive / TICKS as u32,
                restyled as f64 / TICKS as f64,
                differential / TICKS as u32,
            );
            assert!(
                restyled < rows.len() * TICKS / 4,
                "a small delta must not touch most rows"
            );
        }
    }

    /// Profiling harness, widget half (needs a display, so it skips when
    /// there isn't one):
    /// `Xvfb :9 -screen 0 1440x900x24 & DISPLAY=:9 \
    ///  cargo test -p taste-app perf_ -- --ignored --nocapture`
    ///
    /// The hover lag itself, in microseconds: what a factory reset costs
    /// per tick (every visible row's widgets torn down and built again)
    /// against what the differential restyle costs (a badge and two CSS
    /// classes on the rows that moved).
    #[test]
    #[ignore]
    fn perf_row_widget_churn() {
        if gtk::init().is_err() {
            println!("row widget churn: no display — skipped");
            return;
        }
        const ROWS: usize = 40;
        const TICKS: usize = 200;

        // The shape `build_row` produces: content icon, name, badge, and
        // the right-click gesture.
        let build = |path: &Path| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::from_gicon(&crate::editor::file_type_icon(path));
            let label = gtk::Label::builder()
                .label(path.file_name().unwrap().to_string_lossy())
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let (badge, css) = state_style(FileState::Modified);
            let badge = gtk::Label::builder().label(badge).build();
            if let Some(css) = css {
                badge.add_css_class(css);
                label.add_css_class(css);
            }
            row.append(&icon);
            row.append(&label);
            row.append(&badge);
            row.add_controller(gtk::GestureClick::builder().button(3).build());
            (row, label, badge)
        };

        let paths: Vec<PathBuf> = (0..ROWS)
            .map(|i| PathBuf::from(format!("crates/taste-app/src/module{i}.rs")))
            .collect();
        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let expanders: Vec<gtk::TreeExpander> = (0..ROWS)
            .map(|_| {
                let expander = gtk::TreeExpander::new();
                holder.append(&expander);
                expander
            })
            .collect();

        // Before: a status tick reset the factory, so every bound row was
        // rebuilt — the one under the pointer with the rest.
        let mut painted: Vec<(gtk::Label, gtk::Label)> = Vec::new();
        let start = std::time::Instant::now();
        for _ in 0..TICKS {
            painted.clear();
            for (expander, path) in expanders.iter().zip(&paths) {
                let (row, label, badge) = build(path);
                expander.set_child(Some(&row));
                painted.push((label, badge));
            }
        }
        let rebuilt = start.elapsed();

        // After: one row's badge and CSS.
        let start = std::time::Instant::now();
        for tick in 0..TICKS {
            let (label, badge) = &painted[tick % ROWS];
            let (old, new) = if tick % 2 == 0 {
                (FileState::Modified, FileState::Staged)
            } else {
                (FileState::Staged, FileState::Modified)
            };
            if let (_, Some(css)) = state_style(old) {
                badge.remove_css_class(css);
                label.remove_css_class(css);
            }
            let (text, css) = state_style(new);
            badge.set_label(text);
            if let Some(css) = css {
                badge.add_css_class(css);
                label.add_css_class(css);
            }
        }
        let restyled = start.elapsed();

        println!(
            "row widget churn: {ROWS} rows → rebuild-all {:>8.1?}/tick, \
             restyle-one {:>8.1?}/tick",
            rebuilt / TICKS as u32,
            restyled / TICKS as u32,
        );
    }
}
