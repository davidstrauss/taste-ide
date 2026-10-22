//! Left pane: the file tree, which *is* the git interface.
//!
//! Rows show git state; hovering a changed file exposes stage/unstage; the
//! pane header carries branch, commit message entry, commit and push. There
//! is deliberately no other git UI.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::hover::FullTextOnHover;
use adw::prelude::*;
use gtk::glib;
use gtk::glib::BoxedAnyObject;
use taste_core::files::Files;
use taste_core::{Event, Workspace};
use taste_devcontainer::config::PortSpec;
use taste_devcontainer::Worktree;
use taste_git::{FileState, GitWorkspace};

/// One row of the Ports section: a forwarded port and whether anything
/// answers on it (`None` until probed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    pub spec: PortSpec,
    pub state: Option<crate::portview::PortState>,
}

type OpenLogCallback = Box<dyn Fn(taste_core::environment::EnvironmentId, crate::logview::LogKind)>;
type TaskCallback = Box<dyn Fn(taste_core::environment::EnvironmentId, String)>;

/// One row of the Tasks section: the task, and how its last run went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub info: crate::tasks::TaskInfo,
    pub state: crate::tasks::RunState,
}
type OpenPortCallback = Box<dyn Fn(taste_core::environment::EnvironmentId, u16)>;

/// The hits of an outline line and everything under it.
fn subtree_hits(outline: &[crate::tasks::OutlineEntry], at: usize, hits: &[usize]) -> usize {
    let depth = outline[at].depth;
    outline[at..]
        .iter()
        .enumerate()
        .take_while(|(offset, e)| *offset == 0 || e.depth > depth)
        .filter_map(|(_, e)| e.task.map(|i| hits[i]))
        .sum()
}

/// The last runs under a heading — every task beneath it, the heading's own
/// run aside — as a folded heading's arrow badges them: failed, running,
/// and succeeded, how many of each, in a traffic light's order.
fn subtree_states(
    outline: &[crate::tasks::OutlineEntry],
    at: usize,
    states: &[crate::tasks::RunState],
) -> FoldStates {
    use crate::tasks::RunState;
    let depth = outline[at].depth;
    let mut counts = FoldStates::default();
    for entry in outline[at + 1..].iter().take_while(|e| e.depth > depth) {
        match entry.task.map(|i| states[i]) {
            Some(RunState::Failed) => counts.failed += 1,
            Some(RunState::Running) => counts.running += 1,
            Some(RunState::Succeeded) => counts.succeeded += 1,
            _ => {}
        }
    }
    counts
}

/// How many tasks under a folded heading last failed, are running, and
/// last succeeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FoldStates {
    failed: usize,
    running: usize,
    succeeded: usize,
}

impl FoldStates {
    /// The lights, top to bottom, with their words: red, amber, and green,
    /// each only when some task under the heading is in that state.
    fn lights(self) -> Vec<(&'static str, String)> {
        [
            ("red", self.failed, "failed"),
            ("amber", self.running, "running"),
            ("green", self.succeeded, "succeeded"),
        ]
        .into_iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(class, n, words)| (class, format!("{n} {words}")))
        .collect()
    }
}

/// A heading's fold arrow, and — folded — the lights of what it hides
/// beside the arrow's tip, a column of badges at the slot's end, each
/// overlapping the one above it (David, 2026-10-22: "badge the collapsing
/// arrow with the combination of statuses within … it could theoretically
/// show a whole traffic light"; "It's okay to stack the traffic light
/// indicators a bit. We just need to represent the existence of each").
/// Running is amber here, where a task's own row spins: a badge is too
/// small to.
fn fold_arrow(open: bool, states: FoldStates) -> gtk::Widget {
    let arrow = fold_glyph(open);
    let lights = states.lights();
    if open || lights.is_empty() {
        return arrow.upcast();
    }
    arrow.set_hexpand(true);
    arrow.set_halign(gtk::Align::Center);
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["fold-lights"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .can_target(false)
        .build();
    for (class, _) in &lights {
        column.append(
            &gtk::Box::builder()
                .css_classes(["env-badge", class])
                .valign(gtk::Align::Center)
                .build(),
        );
    }
    let overlay = gtk::Overlay::builder().child(&arrow).build();
    overlay.add_overlay(&column);
    overlay.upcast()
}

/// What a fold's tooltip says of the lights on its arrow: ", 1 failed, 2
/// succeeded", or nothing when the heading is open or hides no runs.
fn fold_words(open: bool, states: FoldStates) -> String {
    if open {
        return String::new();
    }
    states
        .lights()
        .into_iter()
        .map(|(_, words)| format!(" · {words}"))
        .collect()
}

/// The Tasks list's height before it scrolls: five of its rows.
const TASKS_MAX_HEIGHT: i32 = 5 * 40;

/// The least of the Tasks list a short column keeps: two of its rows.
const TASKS_MIN_HEIGHT: i32 = 2 * 40;

/// How far a task sits in under its namespace's heading, per level.
const TASK_INDENT: i32 = 16;

/// The header every section of the flank wears — `[glyph] Title` — in
/// the project-folder row's own insets: Logs, Ports, and the backlog
/// (David, 2026-09-06: "Logs, Ports, and Backlog should all use the same
/// design"). No disclosure arrow: the header folds its section when
/// clicked, and the row reads better without a control announcing it
/// (David, later that day). A section with more to say on its header (the
/// backlog's count and actions) appends it after the title.
pub(crate) fn section_header(icon: &str, title: &str) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    // The pane's own column, which is the column of the rows this header
    // titles: a two-pixel difference is the kind an eye catches without
    // knowing what it caught (near-miss.py).
    header.set_margin_top(4);
    header.set_margin_bottom(4);
    header.set_margin_start(CHROME_INSET);
    header.set_margin_end(CHROME_INSET);
    header.append(&gtk::Image::from_icon_name(icon));
    // The title does not ellipsize: it is one short word, and the header's
    // slack is the count's to give up (the backlog's five actions left
    // "Bac…" beside a whole "4 · 3 active" once).
    header.append(
        &gtk::Label::builder()
            .label(title)
            .css_classes(["heading"])
            .xalign(0.0)
            .build(),
    );
    header
}

/// A click on a section's header folds its body. Visibility, not a
/// `GtkRevealer`: a revealer around the backlog's list allocated it at its
/// natural width in the gadget (rows ran 250px past a 400px window), and a
/// fold that hides is a fold.
pub(crate) fn wire_collapse(header: &gtk::Box, body: &impl IsA<gtk::Widget>) {
    let click = gtk::GestureClick::new();
    {
        let body = body.clone().upcast::<gtk::Widget>();
        click.connect_released(move |_, _, _, _| {
            body.set_visible(!body.is_visible());
        });
    }
    header.add_controller(click);
}

/// A section under the file list — `[icon] Logs`, `[icon] Ports` — in the
/// project-folder row's own shape, collapsible from its header. The body
/// holds the section's list and whatever it says when the list is empty.
fn section(icon: &str, title: &str) -> (gtk::Box, gtk::ListBox, gtk::Box) {
    let header = section_header(icon, title);
    // The title takes the slack here; nothing else is on the row.
    if let Some(title) = header.last_child() {
        title.set_hexpand(true);
    }
    let list = gtk::ListBox::builder()
        // Single, so the row whose tab is in front can be shown selected
        // (`select_for_editor`); a click both selects and activates.
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar", "section-list"])
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&list);
    wire_collapse(&header, &body);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    container.append(&header);
    container.append(&body);
    (container, list, body)
}

/// The column's row geometry, so a Ports dot, a Logs glyph, a backlog
/// row's light and a Dirty row's checkbox stand on one vertical line and
/// their titles start on another (David, 2026-09-06: "The checkboxes for
/// dirty items … the icons for the logs, and the status indicators for the
/// issues/environments should all be aligned. The text for all of them
/// should also start aligned across that column"). The numbers are
/// AdwActionRow's — the Dirty rows are action rows, and the theme's metrics
/// are the ones to meet, not to argue with: a 26px prefix box 14px in from
/// the list's edge (the row's 4px margin plus 10 here), a 12px gap, the
/// title at 52.
/// How long the writing has to stop before a standing query is asked
/// again. Long enough to swallow a build's burst of writes, short enough
/// that a file the user just created shows up while they are still looking
/// for it.
const RESEARCH_IDLE: std::time::Duration = std::time::Duration::from_millis(350);

pub(crate) const ROW_INSET: i32 = 10;
pub(crate) const ROW_GAP: i32 = 12;
pub(crate) const LEAD_WIDTH: i32 = 26;

/// The margin every list in this pane insets its rows by.
///
/// **Ours, not the theme's.** libadwaita's `navigation-sidebar` row sits
/// at 6; `main.rs` states 4 for `.section-list`, `.change-list` and the
/// tree alike, because one pane with two insets four pixels apart is the
/// near-miss this column keeps being caught at. Counted here because it
/// is half of where a row's content starts, and anything meant to stand
/// in that column has to add it too.
pub(crate) const SIDEBAR_ROW_MARGIN: i32 = 4;

/// The column the pane's own chrome stands in: the branch bar, the
/// filters, the project-folder row, and every section header.
///
/// Stated as the section rows' column rather than as a second number.
/// The two had drifted to 12 against the rows' 14, so every header sat
/// two pixels inside the list it titles — the near-miss this pane has
/// been caught at more than once, invisible in the source (the numbers
/// live in different functions, and one of them is a theme default) and
/// nearly invisible in a screenshot (`near-miss.py`).
pub(crate) const CHROME_INSET: i32 = SIDEBAR_ROW_MARGIN + ROW_INSET;

/// Where the Ports ghost row says ports are listed — the conventional
/// place (docs/ARCHITECTURE.md → Conventions); the click opens whichever
/// config actually exists.
const PORTS_CONFIG_HINT_PATH: &str = ".devcontainer/devcontainer.json";

/// The leading slot of a row: fixed width, whatever is in it centred, so
/// an 8px dot, a 14px glyph and a checkbox share a centre line.
pub(crate) fn leading_slot(child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let slot = gtk::Box::builder()
        .width_request(LEAD_WIDTH)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .build();
    // A box centres a child only when the child expands into it; and a
    // child that expands would make the slot expand too and take the row's
    // slack from the title — unless the slot's own expand is SET (to
    // false), which is what stops GTK computing it from the children.
    slot.set_hexpand(false);
    child.set_hexpand(true);
    child.set_halign(gtk::Align::Center);
    slot.append(child);
    slot
}

/// `.gitignore`'s text with a line for each of `paths` it does not have:
/// anchored to the root with a leading `/`, so it ignores that file and not
/// every file of the same name, and with gitignore's own characters
/// escaped, so a name is matched as written.
fn with_ignored(current: &str, paths: &[&PathBuf]) -> String {
    let have: HashSet<&str> = current.lines().map(str::trim_end).collect();
    let mut out = current.to_string();
    let mut added = false;
    for path in paths {
        let rel = path.to_string_lossy().replace('\\', "/");
        let mut line = String::from("/");
        for c in rel.chars() {
            if matches!(c, '*' | '?' | '[' | ']' | '!' | '#') {
                line.push('\\');
            }
            line.push(c);
        }
        if line.ends_with(' ') {
            line.insert(line.len() - 1, '\\');
        }
        if have.contains(line.as_str()) {
            continue;
        }
        if !added && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&line);
        out.push('\n');
        added = true;
    }
    out
}

/// A heading's disclosure arrow, open or folded.
fn fold_glyph(open: bool) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(if open {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        })
        .pixel_size(14)
        .css_classes(["dim-label"])
        .build()
}

/// A section's row: a dot or a glyph, a title, a caption under it — the
/// backlog row's geometry, because the two lists share a column.
fn section_row(
    dot: Option<&str>,
    icon: Option<&str>,
    title: &str,
    subtitle: &str,
    trailing: Option<&gtk::Widget>,
) -> gtk::ListBoxRow {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
    box_.set_margin_top(2);
    box_.set_margin_bottom(2);
    box_.set_margin_start(ROW_INSET);
    box_.set_margin_end(ROW_INSET);
    if let Some(class) = dot {
        box_.append(&leading_slot(
            &gtk::Box::builder()
                .css_classes(["env-dot", class])
                .valign(gtk::Align::Center)
                .build(),
        ));
    } else if let Some(icon) = icon {
        box_.append(&leading_slot(
            &gtk::Image::builder()
                .icon_name(icon)
                .css_classes(["dim-label"])
                .pixel_size(14)
                .valign(gtk::Align::Center)
                .build(),
        ));
    }
    let lines = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lines.set_hexpand(true);
    lines.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .build()
            .full_text_on_hover(),
    );
    // No caption, no caption line: an empty one still took its height, so
    // a task with no description had its name at the top of the row and
    // its light centred on the blank line below it. The lines centre as
    // one, so the title alone sits level with the light.
    lines.set_valign(gtk::Align::Center);
    lines.append(
        &gtk::Label::builder()
            .label(subtitle)
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .visible(!subtitle.is_empty())
            .build()
            .full_text_on_hover(),
    );
    box_.append(&lines);
    if let Some(trailing) = trailing {
        box_.append(trailing);
    }
    // No tooltip: the two lines say what the row is, and what happens on
    // activation is what happens to every row in this pane — it opens.
    gtk::ListBoxRow::builder().child(&box_).build()
}

#[derive(Clone)]
struct FileNode {
    path: PathBuf,
    is_dir: bool,
    /// A suggested-but-not-yet-created file (allowlisted config the
    /// workspace could have). Activating it creates the real file.
    ghost: bool,
    /// The New file… and New folder… rows at the end of every folder's
    /// listing (a ghost, never on disk): activating one asks for a name and
    /// creates it there (David, 2026-10-20: "I think all directories should
    /// get ghost entries for new subfolders and files for consistency").
    create: Option<Create>,
}

/// What a create row makes ([`FileNode::create`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Create {
    File,
    Folder,
}

impl Create {
    /// The name a create row stands under in its folder: never a real
    /// file's, and never written.
    fn placeholder(self) -> &'static str {
        match self {
            Create::File => ".taste-new-file",
            Create::Folder => ".taste-new-folder",
        }
    }
}

/// How many of a file's matching lines a search keeps for the match panel.
/// The COUNT is always complete (see `run_search`); this bounds the text.
const MATCH_LINES_PER_FILE: usize = 200;

pub struct FileTree {
    pub widget: gtk::Box,
    workspace: Workspace,
    /// The environment whose checkout the tree and the git views are aimed
    /// at — `None` is the user's own. Watching (ENVIRONMENTS.md → "Watching
    /// an environment") points these panes at another environment's clone,
    /// **read, never edit**, by explicit action only: no chat-tab switch and
    /// no event ever moves it, and it is not persisted — a fresh IDE opens
    /// on the user's checkout.
    watching: RefCell<Option<Aim>>,
    /// The environment panel, pinned to the bottom of this pane: the one
    /// indicator of where the panes are aimed, and the way to aim them
    /// somewhere else — one row per environment, always visible (see
    /// `backlog.rs`).
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
    /// What the primary's checkout in the VM does after it moves a ref —
    /// commit, switch, rebase — so the folder on this host (its peer)
    /// follows at once rather than at the next snapshot. The window wires
    /// it to the supervisor.
    after_ref_change: RefCell<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// The branch the last status pass found checked out in the working
    /// tree; what the branch menu marks.
    status_branch: RefCell<Option<String>>,
    /// How the checkout in the VM is given the peer's freshly fetched
    /// remote-tracking refs before it rebases onto one.
    share_remotes: RefCell<Option<std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>>>,
    /// How the primary's agent is told, with its next prompt, what the
    /// user's Push and Pull did to its branch (`Supervisor::note_for_agent`).
    note_agent: RefCell<Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>>,
    status: Rc<RefCell<HashMap<PathBuf, FileState>>>,
    list_holder: gtk::ScrolledWindow,
    branch_label: gtk::MenuButton,
    /// The label inside it: ellipsizing, so a long branch name cannot
    /// widen this pane (see the construction site).
    branch_child: gtk::Label,
    init_button: gtk::Button,
    /// The push and pull buttons' faces — their counts — behind the
    /// spinner pages (`sync_button_with_label`).
    push_face: gtk::Label,
    pull_face: gtk::Label,
    /// The ⇊: rebase onto the canonical branch ([`PullFrom::Canonical`]).
    canon_face: gtk::Label,
    canon_button: gtk::Button,
    _canon_hold: Rc<crate::holdbutton::HoldButton>,
    branch_popover: gtk::Popover,
    sync_label: gtk::Label,
    sync_button: gtk::Button,
    pull_button: gtk::Button,
    /// Pull's hold: a click fast-forwards, a hold rebases ([`Pull`]). Kept
    /// here because the button holds itself only weakly.
    _pull_hold: Rc<crate::holdbutton::HoldButton>,
    /// Push's hold: a click pushes, and a push that replaces commits on
    /// the remote is held for ([`FileTree::push`]).
    push_hold: Rc<crate::holdbutton::HoldButton>,
    /// What ⇊, ↓, and ↑ show on hover (`crate::sync_card`).
    push_card: Rc<RefCell<Option<crate::sync_card::SyncCard>>>,
    pull_card: Rc<RefCell<Option<crate::sync_card::SyncCard>>>,
    canon_card: Rc<RefCell<Option<crate::sync_card::SyncCard>>>,
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
    /// Why the background fetch is held, when it is: the remote is over
    /// SSH and the key would ask for a touch (`taste_git::presence`). Said
    /// in the Pull button's tooltip, where the counts it keeps honest live.
    fetch_hold: RefCell<Option<String>>,
    /// The Staged view's facts for the universal composer's Commit: how
    /// many files, and whether an unchecked one blocks a commit.
    staged_count: Cell<usize>,
    commit_blocked: Cell<bool>,
    on_commit_state: RefCell<Option<Box<dyn Fn(usize, bool)>>>,
    /// Where a drafted commit message goes: the universal composer.
    on_suggestion: RefCell<Option<Box<dyn Fn(String)>>>,
    /// The one query (search.rs), as last broadcast. The tree has no box
    /// of its own.
    query: RefCell<crate::search::Query>,
    search: RefCell<Option<Rc<crate::search::Search>>>,
    branch_count: gtk::Label,
    /// Definitions, indexed with the file index (`taste_core::search::symbols`).
    /// The editor's listing takes the content hits, definitions and commits.
    /// While searching: show all files with non-matches ghosted, instead
    /// of matches only.
    search_view: RefCell<Option<Rc<SearchView>>>,
    /// What the semantic index found for the query (window.rs asks it,
    /// debounced, and hands the answer here): files the word is not in but
    /// the idea is. They join the visible set with a ≈ badge.
    meaning_hits: RefCell<Vec<crate::search::MeaningHit>>,
    /// Which coalesced re-search is the live one ([`RESEARCH_IDLE`]).
    research: Cell<u64>,
    /// The files the current query reached by the WORD — content matches
    /// and name matches together. Kept because the Files lozenge's number
    /// is these unioned with the ones reached by meaning, and the two
    /// answers land at different moments.
    files_matched: RefCell<HashSet<PathBuf>>,
    /// Which file the bottom match panel is currently showing.
    intervention_file: RefCell<Option<PathBuf>>,
    /// Background search index: the workspace's searchable file list.
    index: RefCell<Option<std::sync::Arc<Vec<PathBuf>>>>,
    /// The running search's stop flag. A search reads every indexed file
    /// (counts have to be complete — see `run_search`), so the one a new
    /// keystroke supersedes is told to stop rather than left to finish
    /// reading for a result nobody will render.
    search_cancel: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    index_building: std::cell::Cell<bool>,
    /// The files' intervention panel (`intervention.rs`): the non-modal
    /// input surface for dirty-file workflows, opening from the bottom of
    /// the file list; closing it cancels and gives the list its height
    /// back. The backlog has its own, under its list.
    intervention: Rc<crate::intervention::Panel>,
    all_toggle: gtk::ToggleButton,
    /// The count All wears beside its infinity sign.
    all_face: gtk::Label,
    commit_box: gtk::Box,
    ignore_rules: std::cell::Cell<usize>,
    /// Which files the tree shows ([`Showing`]): ignored ones, off by
    /// default; synced ones, on.
    ignored_toggle: gtk::ToggleButton,
    synced_toggle: gtk::ToggleButton,
    dirty_toggle: gtk::ToggleButton,
    staged_toggle: gtk::ToggleButton,
    stashed_toggle: gtk::ToggleButton,
    /// The counts the git filters wear beside their icons
    /// ([`icon_and_count`]).
    stashed_face: gtk::Label,
    dirty_face: gtk::Label,
    staged_face: gtk::Label,
    conflicts_face: gtk::Label,
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
    /// The filter views' list (Dirty, Staged, Stashed, Conflicts) and the
    /// flags it was built for, kept across status ticks and reconciled
    /// row by row; `changed_rows` are its rows by path.
    changed_list: RefCell<Option<(gtk::ListBox, ChangedFlags)>>,
    changed_rows: RefCell<HashMap<PathBuf, ChangedRow>>,
    /// The row a shift-click reaches from: the last checkbox toggled by
    /// hand without Shift.
    check_anchor: RefCell<Option<PathBuf>>,
    /// Whether the checkbox being toggled was pressed with Shift held — a
    /// toggle does not say, so its press does.
    shift_click: std::cell::Cell<bool>,
    /// The Stashed view's discard, held to confirm: kept here because the
    /// button holds itself only weakly, for as long as its pane stands.
    stash_discard: RefCell<Option<Rc<crate::holdbutton::HoldButton>>>,
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
    showing: Rc<Cell<Showing>>,
    /// Each submodule of the checkout on screen, by its path in it, and
    /// where it is: its row reads "name @ main" under a folder with git's
    /// mark (David, 2026-10-06).
    submodule_heads: RefCell<HashMap<PathBuf, taste_devcontainer::worktree::SubmoduleHead>>,
    /// A sync of the folder with Personal is pending or under way: a
    /// submodule caught halfway through one — its parent's commit there,
    /// its own not yet — is not warned about until the pass has ended.
    folder_syncing: Cell<bool>,
    /// The folder's submodules that are not yet the commit the project
    /// records and can be made so, and the banner whose one button does it
    /// (`crate::submodule_bar`); `submodule_busy` while it runs.
    submodule_setup: RefCell<Vec<taste_git::submodule_setup::SubmoduleSetup>>,
    submodule_bar: crate::submodule_bar::SubmoduleBar,
    /// The banner for a branch already in the default branch, with the
    /// switch back (`crate::merged_bar`).
    merged_bar: Rc<crate::merged_bar::MergedBar>,
    submodule_busy: Cell<bool>,
    on_open: RefCell<Option<OpenCallback>>,
    /// Changed-list rows open as diffs (the editor's Changes face).
    on_open_diff: RefCell<Option<OpenDiffCallback>>,
    /// What syncs though git ignores it (`Worktree::sync_ignored`): the
    /// rows that wear the synced mark, and the Sync Ignored toggle's state.
    sync_ignored: RefCell<taste_devcontainer::worktree::SyncIgnored>,
    /// A REVIEW row opens a different diff: the branch's blob against the
    /// merge target's, which has no working-tree side and no file on disk.
    on_open_review_diff: RefCell<Option<OpenReviewDiffCallback>>,
    /// Fired when the pane leaves a review, so the tabs that review opened
    /// can go with it.
    on_review_ended: RefCell<Option<Box<dyn Fn()>>>,
    /// The Ports section's list and its empty-state line, under the file
    /// list beside the Logs section (David, 2026-09-06: logs and the
    /// devcontainer's ports listed in the left bar, opening as tabs). The
    /// rows are the selected environment's `forwardPorts`, handed in by
    /// the window (`set_ports`), which is what knows the environments.
    logs_list: gtk::ListBox,
    /// One sparkline per Logs row (`LogKind::ALL`'s order): how much each
    /// log has been saying in the last five minutes, the backlog rows'
    /// own drawing (`set_log_activity`).
    log_sparklines: Vec<crate::sparkline::Sparkline>,
    ports_list: gtk::ListBox,
    /// The Ports section's ghost row: a slashed port and the words, shown
    /// when the devcontainer forwards nothing.
    ports_empty: gtk::Box,
    ports: RefCell<Vec<PortRow>>,
    /// The Tasks section (taskfile.dev, tasks.rs): its rows, the note
    /// that says why they cannot be run when they cannot, the ghost row
    /// that opens the Taskfile or offers to create one, what is listed,
    /// and the task whose tab is in front — kept so a redraw of the rows
    /// keeps it lit.
    tasks_list: gtk::ListBox,
    /// The list's scroller: as tall as its rows up to a cap, and scrolling
    /// past it (David, 2026-09-23: "a maximum height that isn't too tall").
    tasks_scroller: gtk::ScrolledWindow,
    /// What each row of the list is: the task it runs, or `None` for a
    /// namespace's heading (`tasks::outline`).
    task_row_names: RefCell<Vec<Option<String>>>,
    /// The namespaces opened in the outline (`dev`, `db:migrate`); the
    /// rest are closed, their tasks folded under their headings.
    tasks_open: RefCell<HashSet<String>>,
    tasks_note: gtk::Label,
    tasks_empty: gtk::Box,
    tasks_ghost_label: gtk::Label,
    tasks: RefCell<(Vec<TaskRow>, Option<String>, bool)>,
    /// The aimed environment's services (`taste_devcontainer::config::
    /// ServiceSpec`): which tasks run something long-lived, and the port
    /// each serves — a service's row says so, and its port's row runs and
    /// stops it.
    services: RefCell<std::collections::BTreeMap<String, taste_devcontainer::config::ServiceSpec>>,
    task_in_front: RefCell<Option<String>>,
    /// The file whose tab is in front, when the tab in front is a file's:
    /// the one row the tree may light (`select_for_editor`).
    file_in_front: RefCell<Option<PathBuf>>,
    /// The query's hits inside each task's output, by name, as the window
    /// last counted them (`set_task_hits`); and the section's count banner.
    task_hits: RefCell<HashMap<String, usize>>,
    tasks_results: Rc<crate::results::ResultsPanel>,
    on_open_task: RefCell<Option<TaskCallback>>,
    on_run_task: RefCell<Option<TaskCallback>>,
    /// Per published host port, the row's in and out sparklines, for the
    /// tick to feed; and the last readings, so a redrawn row starts from
    /// them (David, 2026-09-21: "proper ingress/egress sparklines for
    /// ports, even for UDP").
    port_sparklines:
        RefCell<HashMap<u16, (crate::sparkline::Sparkline, crate::sparkline::Sparkline)>>,
    port_traffic_seed: RefCell<HashMap<u16, taste_devcontainer::PortTraffic>>,
    /// The count banners (results.rs, title-only): one at the foot of the
    /// files, one under Ports, one under Logs. These panels answer a query
    /// by hiding rows, so they have nothing to list — but a panel that
    /// answered and a panel that did not look the same without a word, and
    /// zero is an answer (David, 2026-09-06: "show the banner at the bottom
    /// of each of those panels with the count of results, whether zero or
    /// more").
    files_results: Rc<crate::results::ResultsPanel>,
    ports_results: Rc<crate::results::ResultsPanel>,
    logs_results: Rc<crate::results::ResultsPanel>,
    /// The files' literal hit count as last reported, so the banner can be
    /// redrawn with the meaning note when the index answers.
    files_hits: Cell<usize>,
    /// The Logs counts as last reported (`set_log_hits`): the window may
    /// report them before this pane has seen the query, and the banner has
    /// to be right whichever arrives second.
    log_hits: RefCell<Vec<usize>>,
    /// A file the editor put in front whose row is not in the model yet:
    /// its folder was just expanded and the rows land asynchronously.
    /// Retried as they arrive; cleared by the next focus change.
    pending_select: RefCell<Option<PathBuf>>,
    /// A selection that is not the file in front, on purpose: the New
    /// file… row while its name is asked for.
    selection_held: Cell<bool>,
    /// The file in front lights its row only where it shows, opening no
    /// folder ([`FileTree::hold_reveal`]): while a launch reopens its tabs.
    reveal_held: Cell<bool>,
    /// A check of the selection against the file in front is queued.
    selection_check_queued: Cell<bool>,
    on_open_log: RefCell<Option<OpenLogCallback>>,
    on_open_port: RefCell<Option<OpenPortCallback>>,
    /// A click on a file that has hits and is already on screen: step to
    /// the next one. Returns whether it did (else the click opens).
    on_step_file: RefCell<Option<Box<dyn Fn(PathBuf) -> bool>>>,
    /// One badge per Logs row (`LogKind::ALL`'s order), hidden at zero.
    /// One slot per Logs row for its pill (`search::hit_badge`), filled
    /// when the counts arrive and emptied when they are zero.
    log_badges: Vec<gtk::Box>,
    /// Routes a staged diff to the chat agent, reply → commit entry.
    commit_suggester: RefCell<Option<SuggestCallback>>,
    /// The open context menu, closed before row rebinds dispose its anchor.
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
    /// The rows currently bound in the list, keyed by absolute path, so an
    /// ordinary git-status tick can restyle the ones that moved instead of
    /// resetting the factory. See [`FileTree::restyle_changed_rows`].
    rows: RefCell<HashMap<PathBuf, RowHandle>>,
    /// The files on their way between the folder and Personal's checkout,
    /// by path from the root (`taste_core::Event::SyncFiles`).
    moving: RefCell<std::sync::Arc<std::collections::BTreeMap<PathBuf, taste_core::SyncFile>>>,
    /// The folders the user has open. A `TreeListModel` is rebuilt whole
    /// whenever the tree changes under it — a save, a git refresh, an
    /// agent's edit — and a rebuilt model starts collapsed, which closed
    /// every folder the user had opened each time a file changed. This is
    /// read off the live model before it is replaced and replayed onto the
    /// new one as its rows arrive (`rebuild`).
    expanded_dirs: RefCell<HashSet<PathBuf>>,
    /// Collapses the watcher's per-path event fan-out into one status query.
    refresh: RefreshGate,
    /// The last status found the checkout unreachable (its VM down, its
    /// files service gone). While set, refresh requests — which arrive in
    /// a stream, since every failed query ends with something asking for
    /// the next — collapse to one retry every few seconds, so neither the
    /// keeper nor the log is hammered (David, 2026-09-21: "Rate-limit
    /// messages like this"). Cleared by the next status that answers.
    unreachable: Cell<bool>,
    /// Since when git beside a checkout in a VM has refused the status, for
    /// [`FileTree::apply_git_refused`]; cleared by the next status that
    /// works.
    git_refused_since: RefCell<Option<std::time::Instant>>,
    unreachable_retry_armed: Cell<bool>,
}

/// How long an unreachable checkout waits between status attempts.
const UNREACHABLE_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Whether the row syncs by a rule of its own, which its badge says
    /// with ⇅ (`SyncIgnored::own_rule`).
    own: std::cell::Cell<bool>,
    /// The sync status, left of the badge (`crate::sync_mark`).
    mark: crate::sync_mark::SyncMark,
}

impl RowHandle {
    /// The status for what is moving now, or `at_rest` when it syncs by
    /// policy and is in step. A folder's files carry their own.
    fn show_mark(&self, moving: Option<&taste_core::SyncFile>, at_rest: Option<&str>) {
        if self.is_dir {
            return;
        }
        // Ignored and not synced: a change there goes nowhere, whatever a
        // watcher made of it.
        let moving = moving.filter(|_| self.state.get() != FileState::Ignored);
        self.mark.show(moving, at_rest);
    }
}

/// How long a refresh request waits for company.
///
/// A burst of agent writes reaches the tree as one `FileChanged` per path —
/// dozens of them for a single edit round — and each used to run its own
/// `git status` and repaint the list. Short enough that staging a file
/// still feels instant.
pub(crate) const REFRESH_COALESCE: std::time::Duration = std::time::Duration::from_millis(120);

/// The refresh coalescer: at most one armed timer, at most one query in
/// flight, and at most one trailing re-run behind it.
///
/// Queries can outlive their own window on a big checkout, so requests
/// arriving during one are not dropped (the status would go stale) and not
/// stacked either (they would queue behind each other for ever) — they
/// collapse into a single re-run when it lands.
///
/// Shared with the editor, which paints the same status onto its tabs off
/// the same events: two panes coalescing the same burst differently is how
/// one of them ends up running a `git status` per changed path.
/// Where the panes are aimed when it is not home: one environment, its
/// checkout wherever it is, the files service that reads it, and the
/// repository on this host holding its refs (its peer, or the clone
/// itself while a checkout is local).
#[derive(Clone, PartialEq)]
pub struct Aim {
    pub env: taste_core::environment::EnvironmentId,
    pub checkout: taste_core::environment::Checkout,
    pub files: taste_core::files::Files,
    pub peer: PathBuf,
}

#[derive(Default)]
pub(crate) struct RefreshGate {
    armed: std::cell::Cell<bool>,
    inflight: std::cell::Cell<bool>,
    trailing: std::cell::Cell<bool>,
}

impl RefreshGate {
    /// A refresh was asked for. `true` means arm the timer.
    pub(crate) fn request(&self) -> bool {
        if self.inflight.get() {
            self.trailing.set(true);
            return false;
        }
        !self.armed.replace(true)
    }

    /// The timer fired. `true` means run the query now.
    pub(crate) fn fire(&self) -> bool {
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
    pub(crate) fn finish(&self) -> bool {
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
    /// Each submodule's path and where it is (`Worktree::submodule_heads`).
    submodules: HashMap<PathBuf, taste_devcontainer::worktree::SubmoduleHead>,
    /// The folder's submodules to set up (`GitWorkspace::submodules_to_set_up`):
    /// the folder's, not the checkout's, since the folder is where they are
    /// cloned and fetched, with the user's keys.
    submodule_setup: Vec<taste_git::submodule_setup::SubmoduleSetup>,
    /// The branch checked out, when it is already in the default branch
    /// (`GitWorkspace::merged_into_default`), asked of the folder's refs
    /// as the sync status is.
    merged: Option<taste_git::merged::MergedInto>,
    /// What syncs though git ignores it (`Worktree::sync_ignored`).
    sync_ignored: taste_devcontainer::worktree::SyncIgnored,
    /// What the sync counts' cards say of submodules.
    submodule_sync: SubmoduleSync,
}

/// The submodules in a push or a pull of the branch, for the ↑, ↓, and ⇊
/// cards (David, 2026-10-20: "The tooltip for pull and push should also
/// include submodule info when relevant").
#[derive(Debug, Clone, Default)]
struct SubmoduleSync {
    /// Push's submodule pushes first (`GitWorkspace::submodule_pushes`):
    /// each submodule, its branch, and the commits it sends — or why the
    /// push would refuse.
    pushes: Option<Result<Vec<(PathBuf, String, usize)>, String>>,
    /// The submodules ↓'s commits move.
    own_moves: Vec<PathBuf>,
    /// The submodules ⇊'s commits move.
    canon_moves: Vec<PathBuf>,
}

impl SubmoduleSync {
    fn read(git: &GitWorkspace, branch: &str) -> Self {
        let head = format!("refs/heads/{branch}");
        let moves = |target: Option<String>| {
            target
                .map(|target| git.submodule_moves(&head, &target))
                .unwrap_or_default()
        };
        Self {
            pushes: Some(git.submodule_pushes(branch).map(|pushes| {
                pushes
                    .into_iter()
                    .map(|push| (push.path, push.branch, push.commits))
                    .collect()
            })),
            own_moves: moves(git.own_target_of(branch)),
            canon_moves: moves(git.canonical_ref_of(branch)),
        }
    }
}

impl SubmoduleSync {
    /// ↑'s card, told what goes first: each submodule branch and what it
    /// sends, or why the push would refuse.
    fn tell_push(&self, card: &mut SyncCard) {
        match &self.pushes {
            Some(Ok(pushes)) => {
                // The name in the value, which wraps, not the label, which
                // sets the column: a long submodule path there squeezed
                // every value on the card to a word a line.
                for (path, branch, commits) in pushes {
                    card.facts.push((
                        "Submodule".to_string(),
                        format!(
                            "{}: {} on {branch}, pushed first",
                            path.display(),
                            count(*commits, "commit")
                        ),
                    ));
                }
            }
            Some(Err(reason)) => add_warning(card, reason),
            None => {}
        }
    }

    /// A pull's card, told which submodules its commits move.
    fn tell_moves(moves: &[PathBuf], card: &mut SyncCard) {
        if moves.is_empty() {
            return;
        }
        card.facts.push((
            if moves.len() == 1 {
                "Submodule".to_string()
            } else {
                "Submodules".to_string()
            },
            format!(
                "{} {} to a new commit",
                submodule_names(moves),
                if moves.len() == 1 { "moves" } else { "move" }
            ),
        ));
    }
}

/// `warning` beside whatever the card already warns of.
fn add_warning(card: &mut SyncCard, warning: &str) {
    card.warning = Some(match card.warning.take() {
        Some(said) => format!("{said}\n{warning}"),
        None => warning.to_string(),
    });
}

/// The paths of `moves`, as a card's value: "tpl", "tpl and ext/lib".
fn submodule_names(moves: &[PathBuf]) -> String {
    let names: Vec<String> = moves.iter().map(|p| p.display().to_string()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

use crate::sync_card::{count, SyncCard};

/// What a sync does once it has fetched: Fetch (⟳) nothing more; Pull's
/// click moves the branch forward only, and says so when that was not
/// enough; Pull's hold rebases (David, 2026-10-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pull {
    Fetch,
    FastForward,
    Rebase,
}

/// Which files the tree lists: what git tracks; that and the ignored files
/// that sync with Personal anyway, which are part of both working copies
/// (the default); or everything, ignored and unsynced included. Two
/// toggles at the end of the filter row choose it, Show Ignored Files (off)
/// and Show Synced Files (on): ignored shown is everything, synced included
/// (David, 2026-10-20: "I actually just want this to be two separate,
/// adjacent buttons").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Showing {
    Tracked,
    #[default]
    TrackedAndSynced,
    All,
}

impl Showing {
    fn of(ignored: bool, synced: bool) -> Self {
        match (ignored, synced) {
            (true, _) => Showing::All,
            (false, true) => Showing::TrackedAndSynced,
            (false, false) => Showing::Tracked,
        }
    }
}

/// A git filter's face: `icon` and its count beside it, the count the label
/// returned. Replaces the button's own label, whose words are its tooltip.
fn icon_and_count(button: &gtk::ToggleButton, icon: &str) -> gtk::Label {
    // Six, as the folder's and the branch's rows part their icon and
    // name, so the counts start where those names do.
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    face.append(&gtk::Image::from_icon_name(icon));
    let count = gtk::Label::new(Some("0"));
    face.append(&count);
    button.set_child(Some(&face));
    count
}

/// Which branch a Pull takes from (David, 2026-10-19): the branch's own
/// copy on the remote — the header's ↓, for commits someone else pushed to
/// it — or the branch a pull request's branch is rebased onto — the ⇊, the
/// remote's default branch or the branch's base. Two targets, two buttons:
/// one Pull for both rebased a branch onto its own pushed copy, which is
/// how an agent's rebase onto main was undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullFrom {
    Own,
    Canonical,
}

/// How long a fetch, rebase or push may run before it is called failed. A
/// hung network step must not leave the sync row spinning for ever.
const GIT_NETWORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The fetch of each submodule cloned in the folder at `root`, by its
/// path: every remote, as the parent's own fetch is, and on the parent's
/// host in the parent's scheme (`taste_git::transport`).
pub(crate) fn submodule_fetch_commands(root: &Path) -> Vec<(String, (String, Vec<String>))> {
    let Some(git) = taste_git::GitWorkspace::discover(root) else {
        return Vec::new();
    };
    git.submodules()
        .into_iter()
        .filter_map(|(_, sub)| {
            if !root.join(&sub).join(".git").exists() {
                return None;
            }
            Some((
                sub.display().to_string(),
                git.submodule_fetch_command(&sub)?,
            ))
        })
        .collect()
}

/// Run one git step, bounded. `Err` carries something worth putting in a
/// toast.
pub(crate) async fn run_git_step(
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
) -> Result<std::process::Output, String> {
    run_git_step_within(program, args, envs, GIT_NETWORK_TIMEOUT).await
}

/// How long a submodule's setup may run: a clone is a whole repository,
/// which a fetch's bound would call failed halfway through a big one.
const SUBMODULE_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// [`run_git_step`], bounded by `limit`.
async fn run_git_step_within(
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    limit: std::time::Duration,
) -> Result<std::process::Output, String> {
    let run = tokio::process::Command::new(&program)
        .envs(envs)
        .args(&args)
        .output();
    match tokio::time::timeout(limit, run).await {
        Ok(Ok(out)) if out.status.success() => Ok(out),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("failed")
            .to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
    }
}

/// For a branch that pushes to a URL — what `gh pr checkout` leaves — ask
/// the remote for the branch's tip there, and fetch that tip when it is not
/// here yet, so ↑ is counted against what the fork actually has rather
/// than against no remote at all. Nothing is written into the repository
/// but objects (`taste_git::relation`). Quiet on failure: the count stays
/// a ceiling, and its tooltip says so.
async fn observe_push_tip(root: PathBuf, branch: Option<String>, envs: Vec<(String, String)>) {
    let Some(branch) = branch else { return };
    let ask_root = root.clone();
    let asked = tokio::task::spawn_blocking(move || {
        taste_git::GitWorkspace::discover(&ask_root)
            .and_then(|git| git.ask_push_tip_command(&branch))
    })
    .await
    .ok()
    .flatten();
    let Some((program, args, target)) = asked else {
        return;
    };
    let out = match run_git_step(program, args, envs.clone()).await {
        Ok(out) => out,
        Err(reason) => {
            tracing::debug!("asking {} for its tip: {reason}", target.display());
            return;
        }
    };
    taste_git::record_remote_tip(
        &target.remote,
        &target.branch,
        &String::from_utf8_lossy(&out.stdout),
    );
    let fetch = tokio::task::spawn_blocking(move || {
        taste_git::GitWorkspace::discover(&root).and_then(|git| git.fetch_push_tip_command(&target))
    })
    .await
    .ok()
    .flatten();
    if let Some((program, args)) = fetch {
        if let Err(reason) = run_git_step(program, args, envs).await {
            tracing::debug!("fetching the push target's tip: {reason}");
        }
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

/// A sync-row button whose content is a stack of its face and a spinner,
/// homogeneous both ways, so the two pages are one size and swapping them
/// moves nothing (David, 2026-09-21: "The spinner should simply swap in
/// for the button content"). The earlier scheme replaced the child and
/// froze the button's size request, which held the button but let the
/// row settle around it.
fn sync_button_stack(face: &impl IsA<gtk::Widget>) -> gtk::Stack {
    let stack = gtk::Stack::builder()
        .hhomogeneous(true)
        .vhomogeneous(true)
        .transition_type(gtk::StackTransitionType::None)
        .build();
    stack.add_named(face, Some("face"));
    let spinner = gtk::Spinner::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    stack.add_named(&spinner, Some("busy"));
    stack.set_visible_child_name("face");
    stack
}

/// The class a sync count wears when it warns: its arrow's badge
/// ([`arrow_button`]), not a colour of its own.
const WARNED: &str = "sync-warned";

fn sync_button_with_label(text: &str, tooltip: &str) -> (gtk::Button, gtk::Label) {
    let face = gtk::Label::new(Some(text));
    let button = gtk::Button::builder()
        .child(&sync_button_stack(&face))
        .tooltip_text(tooltip)
        .css_classes(["flat", "sync-count"])
        .build();
    (button, face)
}

fn sync_button_with_icon(icon: &str, tooltip: &str) -> gtk::Button {
    let face = gtk::Image::from_icon_name(icon);
    gtk::Button::builder()
        .child(&sync_button_stack(&face))
        .tooltip_text(tooltip)
        .css_classes(["flat"])
        .build()
}

/// Show a button's spinner page until the operation ends.
fn button_busy(button: &gtk::Button) {
    if let Some(stack) = button.child().and_downcast::<gtk::Stack>() {
        if let Some(spinner) = stack.child_by_name("busy").and_downcast::<gtk::Spinner>() {
            spinner.start();
        }
        stack.set_visible_child_name("busy");
    }
    button.set_sensitive(false);
}

/// How long Push is held for. On the default branch's checkout every push
/// is held, and one that replaces history there for [`LONG_HOLD`]
/// (`crate::holdbutton`); anywhere else, replacing — a pull request's
/// branch after a rebase — is the ordinary hold, and a plain push a click
/// (David, 2026-10-19: "I don't want to have to hold 10s to force push to
/// my own, non-canonical branch").
fn push_hold_for(on_canonical: bool, replacing: bool) -> std::time::Duration {
    if on_canonical && replacing {
        crate::holdbutton::LONG_HOLD
    } else {
        crate::holdbutton::HOLD
    }
}

/// A sync count's button: its drawn arrow (`crate::sync_arrow`) and its
/// count — glyph, space, number — in a sync button's stack. The count is
/// the label returned; the arrow is drawn in its colour.
///
/// A count that warns — a push that replaces, a pull that rebases — says
/// so with an amber badge on the arrow's shoulder, raised by giving the
/// count [`WARNED`]. The whole count in amber read as inviting where it
/// meant caution (David, 2026-10-22: "I'd prefer to see a yellow badge on
/// the arrow instead of making the whole arrow + number combo golden").
fn arrow_button(arrow: crate::sync_arrow::Arrow, tooltip: &str) -> (gtk::Button, gtk::Label) {
    let count = gtk::Label::new(Some(" 0"));
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let drawn = crate::sync_arrow::SyncArrow::new(arrow);
    let badge = gtk::Box::builder()
        .css_classes(["env-badge", "amber", "arrow-badge"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .can_target(false)
        .visible(false)
        .build();
    let marked = gtk::Overlay::builder().child(&drawn).build();
    marked.add_overlay(&badge);
    face.append(&marked);
    face.append(&count);
    {
        let badge = badge.downgrade();
        count.connect_css_classes_notify(move |count| {
            if let Some(badge) = badge.upgrade() {
                badge.set_visible(count.has_css_class(WARNED));
            }
        });
    }
    // The arrow is drawn in the count's colour, so it is drawn again when
    // that changes: the warning class Push wears when it replaces, and the
    // dimming of a disabled button, which reach the count and not it.
    {
        let drawn = drawn.downgrade();
        count.connect_css_classes_notify(move |_| {
            if let Some(drawn) = drawn.upgrade() {
                drawn.queue_draw();
            }
        });
    }
    {
        let drawn = drawn.downgrade();
        count.connect_state_flags_changed(move |_, _| {
            if let Some(drawn) = drawn.upgrade() {
                drawn.queue_draw();
            }
        });
    }
    let button = gtk::Button::builder()
        .child(&sync_button_stack(&face))
        .tooltip_text(tooltip)
        .css_classes(["flat", "sync-count"])
        .build();
    (button, count)
}

/// What a push of `sync`'s branch replaces on its push target, when it
/// replaces anything: the tip there it is leased on and how many commits
/// there it drops from the branch's history. A fork's branch rebased onto
/// its base, or a branch rewritten here (`SyncStatus::rewritten`); any
/// other divergence is the push target having moved, which is Pull's.
fn replacing_push(sync: &taste_git::SyncStatus) -> Option<(String, usize)> {
    (sync.base.is_some() || sync.rewritten.is_some())
        .then(|| sync.replaces.clone())
        .flatten()
}

/// What a push replacing a branch rewritten here replaces, as the IDE
/// found it (`SyncStatus::rewritten`): the `replaced` commits on the
/// remote are the branch's own old copies, and either every change in them
/// is in the branch — compared change by change, so the push loses nothing
/// — or `dropped` of them make a change it no longer has, which the push
/// drops (David, 2026-10-19: "make this tooltip more clear if we've been
/// able to verify that our local commits include the same work, just
/// rebased").
fn rewrite_verdict(replaced: usize, dropped: usize, upstream: &str) -> String {
    let (commits, are) = if replaced == 1 {
        ("commit", "is")
    } else {
        ("commits", "are")
    };
    match dropped {
        0 => format!(
            "the {replaced} {commits} on {upstream} {are} this branch's own from before it \
             was rebased or reworded here, and every change in {} is in this branch, so \
             nothing is lost",
            if replaced == 1 { "it" } else { "them" }
        ),
        1 => format!(
            "the {replaced} {commits} on {upstream} {are} this branch's own from before it \
             was rewritten here, and 1 of them makes a change this branch no longer has, \
             which the push drops"
        ),
        n => format!(
            "the {replaced} {commits} on {upstream} {are} this branch's own from before it \
             was rewritten here, and {n} of them make changes this branch no longer has, \
             which the push drops"
        ),
    }
}

/// Show a button's face again, in place of its spinner. Only the spinner:
/// every status refresh calls this, and those land several times a second
/// on a checkout in a VM, so a hold's ring (`holdbutton`) swapped back to
/// the face as soon as it was up — the hold still ran to its end, with
/// nothing on screen to say so (David, 2026-10-19: "when I held to pull,
/// it didn't show the animating circle").
fn button_idle(button: &gtk::Button) {
    if let Some(stack) = button.child().and_downcast::<gtk::Stack>() {
        if stack.visible_child_name().as_deref() == Some("busy") {
            stack.set_visible_child_name("face");
        }
        if let Some(spinner) = stack.child_by_name("busy").and_downcast::<gtk::Spinner>() {
            spinner.stop();
        }
    }
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
        // giving up all its width and collapsing to "w…49". The minimum
        // is a floor for a squeeze, not a width: at 14 it held the menu
        // arrow fourteen characters away from a four-letter name, and an
        // arrow that far from its word is not that word's.
        let branch_child = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .width_chars(6)
            .max_width_chars(22)
            .xalign(0.0)
            .build()
            .full_text_on_hover();
        // One control that reads as a menu: the branch glyph, the name, and
        // the platform's own dropdown arrow, inside one button. The glyph
        // used to sit beside the button and the arrow was hidden (a
        // MenuButton with a custom child shows none), so the row read as a
        // heading with a bold word in it (David: "The branch listing
        // should be more obviously a menu").
        let git_glyph = gtk::Image::from_icon_name("taste-branch-symbolic");
        git_glyph.add_css_class("dim-label");
        // Branches that match the query, on the button that is the way to
        // them (reachability, SEARCH.md rule 3).
        let branch_count = gtk::Label::builder()
            .css_classes(["caption", "accent"])
            .visible(false)
            .build();
        let branch_face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        branch_face.append(&git_glyph);
        branch_face.append(&branch_child);
        branch_face.append(&branch_count);
        // No arrow. The glyph and the branch name already read as a
        // button, and a disclosure triangle beside them is a second thing
        // saying the same (David, 2026-09-08: "remove the dropdown
        // indicator on the branch. As long as the icon + branch name is
        // clearly a button, that's enough") — the same argument the
        // section headers were given when theirs went.
        let branch_label = gtk::MenuButton::builder()
            .css_classes(["flat"])
            .direction(gtk::ArrowType::Down)
            .child(&branch_face)
            .build();
        // Each sync button's content is a two-page stack, its face and a
        // spinner, sized to the larger of the two: a running operation
        // swaps the page and nothing around it moves (David, 2026-09-21:
        // "When I press to push or pull and it turns into a spinner, no
        // positions of anything should change").
        let (push_button, push_face) =
            arrow_button(crate::sync_arrow::Arrow::Up, "Push commits to the remote");
        push_button.set_sensitive(false);
        let push_hold = crate::holdbutton::HoldButton::around(
            &push_button,
            &push_button
                .child()
                .and_downcast::<gtk::Stack>()
                .expect("a sync button's child is its stack"),
            "face",
        );
        let (pull_button, pull_face) = arrow_button(
            crate::sync_arrow::Arrow::Down,
            "Pull: a click fetches and fast-forwards; hold to rebase onto the remote tip",
        );
        pull_button.set_sensitive(false);
        let pull_hold = crate::holdbutton::HoldButton::around(
            &pull_button,
            &pull_button
                .child()
                .and_downcast::<gtk::Stack>()
                .expect("a sync button's child is its stack"),
            "face",
        );
        let (canon_button, canon_face) = arrow_button(
            crate::sync_arrow::Arrow::DoubleDown,
            "Rebase onto the remote's default branch: hold to rebase",
        );
        canon_button.set_sensitive(false);
        canon_button.set_visible(false);
        let canon_hold = crate::holdbutton::HoldButton::around(
            &canon_button,
            &canon_button
                .child()
                .and_downcast::<gtk::Stack>()
                .expect("a sync button's child is its stack"),
            "face",
        );
        // Which files the tree lists: two toggles, the eye for the ignored
        // files and the sync arrows for the synced ones (David, 2026-10-20).
        let ignored_toggle = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .tooltip_text("Show ignored files")
            .css_classes(["flat"])
            .build();
        let synced_toggle = gtk::ToggleButton::builder()
            .icon_name("taste-sync-symbolic")
            .tooltip_text("Show ignored files that sync with Personal")
            .css_classes(["flat"])
            .active(true)
            .build();
        // Filters in change-flow order: Stashed ↔ Dirty ↔ Staged, with
        // All (no git filter) leading. Counts live on the buttons.
        let all_toggle = gtk::ToggleButton::builder()
            .label("All")
            .tooltip_text("No git filter: every file the tree lists")
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
        // The git filters as icons with their counts (David, 2026-10-20:
        // "Are there icons we could use for stashed/dirty/staged?"): a box
        // for what is put away, a pencil for what is edited, a ticked box
        // for what is ready to commit, the warning for what is in conflict.
        // Their tooltips say the words.
        let all_face = icon_and_count(&all_toggle, "taste-all-files-symbolic");
        let stashed_face = icon_and_count(&stashed_toggle, "package-x-generic-symbolic");
        let dirty_face = icon_and_count(&dirty_toggle, "document-edit-symbolic");
        let staged_face = icon_and_count(&staged_toggle, "checkbox-checked-symbolic");
        let conflicts_face = icon_and_count(&conflicts_toggle, "dialog-warning-symbolic");
        // search_delay debounces keystrokes; run_search additionally drops
        // stale results, so typing never staggers the UI.

        let suggest_button = gtk::Button::builder()
            .icon_name("starred-symbolic")
            .tooltip_text("Ask the AI to suggest a commit message for the staged changes")
            .css_classes(["flat", "circular"])
            .build();

        // Where a commit is written: the universal composer under the chat,
        // and this row says so, in the words and glyph the backlog's ghost
        // row uses (David, 2026-09-07: "use the same iconography/language
        // to say that commits get created through the universal
        // composer"). The sparkle stays: it drafts a message into that box.
        let commit_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        commit_row.set_margin_start(10);
        commit_row.set_margin_end(10);
        commit_row.set_margin_top(4);
        commit_row.set_margin_bottom(4);
        commit_row.append(&suggest_button);
        {
            let glyph = gtk::Image::from_icon_name("taste-compose-symbolic");
            glyph.add_css_class("dim-label");
            commit_row.append(&glyph);
        }
        commit_row.append(
            &gtk::Label::builder()
                .label("Commits are written in the Dispatch box under the chat")
                .xalign(0.0)
                .hexpand(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
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
            // Ellipsize only — no `width_chars` floor, unlike the branch
            // label beside it. This carries the paused-rebase notice
            // ("rebase paused — resolve, mark, Continue"), which reads
            // fully whenever there is room (an ellipsized label's NATURAL
            // width is still its full text without `max-width-chars`) but
            // used to be counted at that full width when there is not —
            // the sync row's single biggest contributor to the pane's
            // minimum, ahead of even the branch dropdown. The full text
            // rides along in the tooltip regardless (`set_sync_label`).
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build()
            .full_text_on_hover();
        let sync_button = sync_button_with_icon(
            "view-refresh-symbolic",
            "Fetch the remote (refreshes the counts)",
        );
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
        sync_row.append(&branch_label);
        sync_row.append(&sync_label);
        sync_row.append(&abort_button);
        sync_row.append(&continue_button);
        sync_row.append(&init_button);
        // The three counts as one group, parted by their own padding rather
        // than the row's spacing: they are one instrument — where this
        // branch stands against its remote copy and the canonical branch —
        // and the room is the branch name's.
        let counts = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        counts.append(&canon_button);
        counts.append(&pull_button);
        counts.append(&push_button);
        sync_row.append(&counts);
        // Fetch belongs to the folder, not the branch: it is on the
        // project row above (David, 2026-10-20).

        // Which environment the panes are aimed at is said once, by the
        // panel at the bottom of this pane — the backlog, whose first row
        // is the user's own checkout and whose other rows are the issues,
        // each started one with its environment's marks on it. Workspace-
        // scoped: it reads the main checkout's root rather than whatever
        // the panes are aimed at, because the queue lives on one ref for
        // the whole workspace, and watching an environment does not change
        // whose backlog this is.
        let backlog = crate::backlog::BacklogPanel::new(
            workspace.root().to_path_buf(),
            workspace.activity.clone(),
            &workspace,
        );

        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(CHROME_INSET);
        header.set_margin_end(CHROME_INSET);
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
        // The ignored-files eye sits at the end of the filter row: the
        // search row it shared with the tree's own entry is gone, the query
        // having moved to the title bar.
        let showing_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        showing_box.set_halign(gtk::Align::End);
        showing_box.set_hexpand(true);
        showing_box.append(&ignored_toggle);
        showing_box.append(&synced_toggle);
        branch_row.append(&showing_box);
        // The git row leads: the branch menu and what follows from it —
        // behind/ahead as Pull and Push, Sync, a paused rebase's Abort and
        // Continue, Initialize Repository where there is no repo. It was
        // built and never attached from the title-bar search commit
        // (e1eb8cf) until 2026-09-06 — the header was rebuilt around the
        // search row's departure and this line went with it — so for a day
        // the IDE had no way to push or pull (David: "What happened to the
        // interface for me to actually commit and push/pull?"). The
        // filters, a listing choice, sit under it.
        header.append(&sync_row);
        header.append(&branch_row);

        let list_holder = gtk::ScrolledWindow::builder().vexpand(true).build();

        // Logs and Ports: two sections in the project-folder row's shape,
        // under the files. Their rows open in the editor's strip the way a
        // file does (`logview`, `portview`); what they list is the selected
        // environment's, so they follow the aim the way the tree does.
        let (logs_section, logs_list, logs_body) = section(crate::logview::LOG_ICON, "Logs");
        logs_list.add_css_class("log-list");
        let mut log_sparklines = Vec::new();
        let mut log_badges = Vec::new();
        for kind in crate::logview::LogKind::ALL {
            let sparkline = crate::sparkline::Sparkline::new();
            let badge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            badge.set_valign(gtk::Align::Center);
            let trailing = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            trailing.append(&badge);
            trailing.append(&sparkline.widget);
            // The icon, the name, and the activity: a short set, the
            // description left to the log's tab (David, 2026-10-20: "Drop
            // the descriptions from the 'Logs' items").
            logs_list.append(&section_row(
                None,
                Some(kind.icon()),
                kind.title(),
                "",
                Some(trailing.upcast_ref()),
            ));
            log_sparklines.push(sparkline);
            log_badges.push(badge);
        }
        // The count banners, at each panel's foot, up only under a query.
        let files_results = crate::results::ResultsPanel::new();
        files_results.widget.set_widget_name("files-results");
        let logs_results = crate::results::ResultsPanel::new();
        logs_results.widget.set_widget_name("logs-results");
        logs_body.append(&logs_results.widget);
        let ports_results = crate::results::ResultsPanel::new();
        ports_results.widget.set_widget_name("ports-results");
        let (ports_section, ports_list, ports_body) = section(crate::portview::PORT_ICON, "Ports");
        // A ghost row like the other panels' (David, 2026-09-08: "For
        // ports … show a 'ghost row' just like for other panels. Except,
        // have a port with a slash through it as the icon"; then: "To add
        // ports, list them in <filename>. Clicking it should open that file
        // path. Like for backlog, keep the ghost row at the bottom always"):
        // the rows' own geometry, the server stack with a plus in the leading slot (it was a slashed
        // port until David, 2026-09-23: "a server stack with a plus (like for
        // adding tasks), not a slash"),
        // under the list whatever it holds, and a click opens the config —
        // or offers to create it, as the tree's own ghost does.
        let ports_empty = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
        // Plus the 4 a list row pads itself by: this box is in the body,
        // not the list, and its glyph and words must stand on the rows'
        // columns (the near-miss check reads a 4 as a defect).
        ports_empty.set_margin_start(ROW_INSET + 4);
        ports_empty.set_margin_end(ROW_INSET + 4);
        ports_empty.set_margin_top(2);
        ports_empty.set_margin_bottom(6);
        ports_empty.add_css_class("backlog-ghost");
        ports_empty.append(&leading_slot(
            &gtk::Image::builder()
                .icon_name("taste-port-add-symbolic")
                .pixel_size(13)
                .css_classes(["dim-label"])
                .build(),
        ));
        ports_empty.append(
            &gtk::Label::builder()
                // The key, not only the file (David, 2026-09-08: "name the
                // actual section that needs to exist").
                .label("Add forwardPorts in devcontainer.json.")
                .css_classes(["caption", "dim-label", "ghost-text"])
                .xalign(0.0)
                // The row's width, not thirty characters': unexpanded it
                // wrapped a one-line sentence wherever the flank had the
                // height to give it a second line.
                .hexpand(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(30)
                .build(),
        );
        ports_empty.set_cursor_from_name(Some("pointer"));
        ports_body.append(&ports_empty);
        ports_body.append(&ports_results.widget);

        // Tasks (taskfile.dev, tasks.rs), below Ports (David, 2026-09-23):
        // one row per task with its last run's light and a run button, a
        // note when they cannot be run, and the ghost row under them that
        // opens the Taskfile — or offers to create one — as Ports' does
        // the devcontainer.
        let (tasks_section, tasks_list, tasks_body) = section(crate::tasks::TASK_ICON, "Tasks");
        tasks_list.set_widget_name("tasks-list");
        // Five rows' worth before it scrolls: the flank is shared, and a
        // project with forty tasks must not push the Logs off it.
        tasks_body.remove(&tasks_list);
        let tasks_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(TASKS_MAX_HEIGHT)
            .child(&tasks_list)
            // Hidden until a row is kept, as `render_tasks` leaves it: a
            // project with no Taskfile is the state the tree starts in, so
            // `set_tasks` finds nothing changed, never renders, and the
            // empty scroller stood between the header and the ghost row.
            .visible(false)
            .build();
        tasks_body.prepend(&tasks_scroller);
        let tasks_note = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(30)
            .visible(false)
            .build();
        tasks_note.set_margin_start(ROW_INSET + 4 + LEAD_WIDTH + ROW_GAP);
        tasks_note.set_margin_end(ROW_INSET + 4);
        tasks_note.set_margin_top(2);
        let tasks_empty = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
        tasks_empty.set_margin_start(ROW_INSET + 4);
        tasks_empty.set_margin_end(ROW_INSET + 4);
        tasks_empty.set_margin_top(2);
        tasks_empty.set_margin_bottom(6);
        tasks_empty.add_css_class("backlog-ghost");
        tasks_empty.append(&leading_slot(
            &gtk::Image::builder()
                .icon_name("document-new-symbolic")
                .pixel_size(13)
                .css_classes(["dim-label"])
                .build(),
        ));
        let tasks_ghost_label = gtk::Label::builder()
            .label("Add tasks in a Taskfile.yml.")
            .css_classes(["caption", "dim-label", "ghost-text"])
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(30)
            .build();
        tasks_empty.append(&tasks_ghost_label);
        tasks_empty.set_cursor_from_name(Some("pointer"));
        tasks_body.append(&tasks_note);
        tasks_body.append(&tasks_empty);
        let tasks_results = crate::results::ResultsPanel::new();
        tasks_results.widget.set_widget_name("tasks-results");
        tasks_body.append(&tasks_results.widget);

        // Dirty-file workflows that need input steal space from the
        // bottom of the file list — never a modal dialog.
        let intervention = crate::intervention::Panel::new();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_width_request(180);
        widget.append(&header);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let submodule_bar = crate::submodule_bar::SubmoduleBar::new(
            CHROME_INSET,
            sync_button_with_label("Set Up", "").0,
        );
        widget.append(&submodule_bar.widget);
        let merged_bar = crate::merged_bar::MergedBar::new(CHROME_INSET);
        widget.append(&merged_bar.widget);
        // The project folder itself: always the first row, never
        // collapsible; its context menu creates top-level items.
        let root_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The folder is a button, as the branch under it is, and a click
        // opens what can be done with it (David, 2026-10-20: "Interactions
        // with the project folder should be a left-click pop-up, just like
        // the branch switcher"). Being a button also stands its icon and
        // name in the column the branch's and the filters' do: as a plain
        // row they stood ten pixels left of them (TASTE_PROBE_ALIGN).
        let root_face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root_face.append(&gtk::Image::from_icon_name("folder-open-symbolic"));
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
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build()
            .full_text_on_hover();
        root_face.append(&root_label);
        let root_button = gtk::Button::builder()
            .child(&root_face)
            .css_classes(["flat"])
            .halign(gtk::Align::Start)
            .hexpand(true)
            .tooltip_text("New file, new folder, and more")
            .build();
        root_row.append(&root_button);
        // The pane's first row (David, 2026-10-20): the folder, and the
        // fetch that freshens everything under it — then the branch with
        // its counts, then the filters.
        root_row.append(&sync_button);
        header.prepend(&root_row);
        widget.append(&list_holder);
        widget.append(&files_results.widget);
        // The files' intervention panel opens from the bottom of the file
        // list — directly under the rows it asks about, not under the
        // sections below them (David, 2026-09-06: "For staging dirty files
        // … it should pop up from the bottom of the files subpanel").
        widget.append(&intervention.widget);
        // Ports first, then Logs (David, 2026-09-06): what is running is
        // read before what it wrote.
        widget.append(&ports_section);
        widget.append(&tasks_section);
        widget.append(&logs_section);
        // Last, and permanent: the backlog sits below everything else this
        // pane opens, so the context it names is never the thing that gets
        // displaced. Its own interventions — a new issue, an edit, an
        // environment's rename or destruction — open inside it, under its
        // list.
        widget.append(&backlog.widget);

        let tree = Rc::new(Self {
            widget,
            workspace: workspace.clone(),
            watching: RefCell::new(None),
            backlog: backlog.clone(),
            root_label,
            git: RefCell::new(GitWorkspace::discover(workspace.root())),
            after_ref_change: RefCell::new(None),
            status_branch: RefCell::new(None),
            share_remotes: RefCell::new(None),
            note_agent: RefCell::new(None),
            status: Rc::new(RefCell::new(HashMap::new())),
            list_holder,
            intervention: intervention.clone(),
            branch_label,
            branch_child,
            init_button: init_button.clone(),
            push_face,
            pull_face,
            canon_face,
            canon_button: canon_button.clone(),
            _canon_hold: canon_hold.clone(),
            branch_popover: branch_popover.clone(),
            sync_label,
            sync_button: sync_button.clone(),
            pull_button: pull_button.clone(),
            _pull_hold: pull_hold.clone(),
            push_hold: push_hold.clone(),
            push_card: crate::sync_card::install(&push_button),
            pull_card: crate::sync_card::install(&pull_button),
            canon_card: crate::sync_card::install(&canon_button),
            push_button: push_button.clone(),
            sync_busy: std::cell::Cell::new(false),
            abort_button: abort_button.clone(),
            continue_button: continue_button.clone(),
            last_fetch: std::cell::Cell::new(None),
            fetch_hold: RefCell::new(None),
            conflicts_toggle: conflicts_toggle.clone(),
            staged_count: Cell::new(0),
            commit_blocked: Cell::new(false),
            on_commit_state: RefCell::new(None),
            on_suggestion: RefCell::new(None),
            query: RefCell::new(crate::search::Query::default()),
            search: RefCell::new(None),
            branch_count: branch_count.clone(),
            search_view: RefCell::new(None),
            meaning_hits: RefCell::new(Vec::new()),
            research: Cell::new(0),
            files_matched: RefCell::new(HashSet::new()),
            intervention_file: RefCell::new(None),
            index: RefCell::new(None),
            search_cancel: RefCell::new(None),
            index_building: std::cell::Cell::new(false),
            all_toggle: all_toggle.clone(),
            all_face,
            commit_box: commit_row.clone(),
            ignore_rules: std::cell::Cell::new(0),
            ignored_toggle: ignored_toggle.clone(),
            synced_toggle: synced_toggle.clone(),
            dirty_toggle: dirty_toggle.clone(),
            staged_toggle: staged_toggle.clone(),
            stashed_toggle: stashed_toggle.clone(),
            stashed_face,
            dirty_face,
            staged_face,
            conflicts_face,
            review: RefCell::new(None),
            on_open_review_diff: RefCell::new(None),
            on_review_ended: RefCell::new(None),
            logs_list,
            log_sparklines,
            ports_list,
            ports_empty,
            ports: RefCell::new(Vec::new()),
            tasks_list,
            tasks_scroller,
            task_row_names: RefCell::new(Vec::new()),
            tasks_open: RefCell::new(HashSet::new()),
            tasks_note,
            tasks_empty,
            tasks_ghost_label,
            tasks: RefCell::new((Vec::new(), None, false)),
            services: RefCell::new(std::collections::BTreeMap::new()),
            task_in_front: RefCell::new(None),
            file_in_front: RefCell::new(None),
            task_hits: RefCell::new(HashMap::new()),
            tasks_results,
            on_open_task: RefCell::new(None),
            on_run_task: RefCell::new(None),
            port_sparklines: RefCell::new(HashMap::new()),
            port_traffic_seed: RefCell::new(HashMap::new()),
            files_results,
            ports_results,
            logs_results,
            log_hits: RefCell::new(Vec::new()),
            files_hits: Cell::new(0),
            pending_select: RefCell::new(None),
            selection_held: Cell::new(false),
            reveal_held: Cell::new(false),
            selection_check_queued: Cell::new(false),
            on_open_log: RefCell::new(None),
            on_open_port: RefCell::new(None),
            on_step_file: RefCell::new(None),
            log_badges,
            stashed: RefCell::new(HashSet::new()),
            selection: RefCell::new(HashSet::new()),
            syncing_selection: std::cell::Cell::new(false),
            changed_list: RefCell::new(None),
            changed_rows: RefCell::new(HashMap::new()),
            check_anchor: RefCell::new(None),
            shift_click: std::cell::Cell::new(false),
            stash_discard: RefCell::new(None),
            container_mode: std::cell::Cell::new(workspace.exec.is_container()),
            commit_overlay,
            commit_blocker,
            pane: std::cell::Cell::new(PaneKind::None),
            rendered_non_repo: std::cell::Cell::new(false),
            showing: Rc::new(Cell::new(Showing::default())),
            submodule_heads: RefCell::new(HashMap::new()),
            folder_syncing: Cell::new(false),
            submodule_setup: RefCell::new(Vec::new()),
            submodule_bar,
            merged_bar,
            submodule_busy: Cell::new(false),
            on_open: RefCell::new(None),
            on_open_diff: RefCell::new(None),
            sync_ignored: RefCell::new(Default::default()),
            commit_suggester: RefCell::new(None),
            open_menu: RefCell::new(None),
            rows: RefCell::new(HashMap::new()),
            moving: RefCell::new(syncing_for_probe()),
            expanded_dirs: RefCell::new(HashSet::new()),
            refresh: RefreshGate::default(),
            unreachable: Cell::new(false),
            git_refused_since: RefCell::new(None),
            unreachable_retry_armed: Cell::new(false),
        });
        // The icon set swaps a few icons between the schemes (`file_icons`),
        // and a row resolves its icon when it is bound: a flip rebinds.
        {
            let weak = Rc::downgrade(&tree);
            adw::StyleManager::default().connect_dark_notify(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.rebuild_rows_in_place();
                }
            });
        }
        {
            // The ghost's click: the config that exists, in the editor — or
            // the conventional one as an unsaved buffer to fill in. The
            // existence check is a stat, so it runs off the main thread.
            let weak = Rc::downgrade(&tree);
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let Some(tree) = weak.upgrade() else { return };
                let root = tree.view_root();
                let files = tree.workspace.files();
                let weak = weak.clone();
                glib::spawn_future_local(async move {
                    let handle = crate::runtime::runtime().spawn_blocking(move || {
                        taste_devcontainer::config::candidate_paths(&root)
                            .into_iter()
                            .find(|path| files.is_file(path))
                            .ok_or_else(|| root.join(PORTS_CONFIG_HINT_PATH))
                    });
                    let Ok(found) = handle.await else { return };
                    let Some(tree) = weak.upgrade() else { return };
                    match found {
                        Ok(existing) => tree.open(existing, None),
                        Err(absent) => tree.create_ghost(&absent),
                    }
                });
            });
            tree.ports_empty.add_controller(click);
        }
        {
            // The Tasks ghost: the Taskfile there is, under whichever of
            // task's names — or the canonical one as an unsaved buffer.
            let weak = Rc::downgrade(&tree);
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let Some(tree) = weak.upgrade() else { return };
                let root = tree.view_root();
                let files = tree.workspace.files();
                let weak = weak.clone();
                glib::spawn_future_local(async move {
                    let handle = crate::runtime::runtime().spawn_blocking(move || {
                        taste_core::conventions::TASKFILE_NAMES
                            .iter()
                            .map(|name| root.join(name))
                            .find(|path| files.is_file(path))
                            .ok_or_else(|| root.join(taste_core::conventions::TASKFILE_NAMES[0]))
                    });
                    let Ok(found) = handle.await else { return };
                    let Some(tree) = weak.upgrade() else { return };
                    match found {
                        Ok(existing) => tree.open(existing, None),
                        Err(absent) => tree.create_ghost(&absent),
                    }
                });
            });
            tree.tasks_empty.add_controller(click);
        }
        {
            let weak = Rc::downgrade(&tree);
            tree.tasks_list.connect_row_activated(move |_, row| {
                let Some(tree) = weak.upgrade() else { return };
                let Ok(index) = usize::try_from(row.index()) else {
                    return;
                };
                let name = tree.task_row_names.borrow().get(index).cloned().flatten();
                let env = tree.aimed_environment();
                let hook = tree.on_open_task.borrow();
                if let (Some(name), Some(hook)) = (name, hook.as_ref()) {
                    hook(env, name);
                }
                drop(hook);
            });
        }

        {
            let weak = Rc::downgrade(&tree);
            tree.logs_list.connect_row_activated(move |_, row| {
                let Some(tree) = weak.upgrade() else { return };
                let Some(kind) = usize::try_from(row.index())
                    .ok()
                    .and_then(|index| crate::logview::LogKind::ALL.get(index).copied())
                else {
                    return;
                };
                let env = tree.aimed_environment();
                let hook = tree.on_open_log.borrow();
                if let Some(hook) = hook.as_ref() {
                    hook(env, kind);
                }
                drop(hook);
            });
        }
        {
            let weak = Rc::downgrade(&tree);
            tree.ports_list.connect_row_activated(move |_, row| {
                let Some(tree) = weak.upgrade() else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let port = tree.ports.borrow().get(index as usize).map(|r| r.spec.port);
                let env = tree.aimed_environment();
                let hook = tree.on_open_port.borrow();
                if let (Some(port), Some(hook)) = (port, hook.as_ref()) {
                    hook(env, port);
                }
                drop(hook);
            });
        }

        let weak = Rc::downgrade(&tree);
        {
            let weak = Rc::downgrade(&tree);
            tree.merged_bar.switch.connect_clicked(move |_| {
                if let Some(tree) = weak.upgrade() {
                    if let Some(default) = tree.merged_bar.default_branch() {
                        tree.run_branch_op(default, false);
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&tree);
            tree.submodule_bar.button.connect_clicked(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.set_up_submodules();
                }
            });
        }
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
        for held in [false, true] {
            let weak = weak.clone();
            let run = move || {
                if let Some(tree) = weak.upgrade() {
                    tree.push(held);
                }
            };
            if held {
                push_hold.set_on_confirm(run);
            } else {
                push_hold.set_on_click(run);
            }
        }
        let weak = Rc::downgrade(&tree);
        let weak_branches = Rc::downgrade(&tree);
        branch_popover.connect_show(move |_| {
            if let Some(tree) = weak_branches.upgrade() {
                tree.populate_branch_menu();
            }
        });
        {
            // Named at click time: the primary's checkout moves into the
            // VM after the window is up.
            let open_menu = {
                let weak = Rc::downgrade(&tree);
                let anchor = root_face.clone();
                Rc::new(move || {
                    if let Some(tree) = weak.upgrade() {
                        let root_node = FileNode {
                            path: tree.view_root(),
                            is_dir: true,
                            ghost: false,
                            create: None,
                        };
                        tree.show_context_menu(&anchor, &root_node);
                    }
                })
            };
            let context = gtk::GestureClick::builder().button(3).build();
            let on_right = open_menu.clone();
            context.connect_released(move |_, _, _, _| on_right());
            // On the folder's button, not its row: the row holds the fetch
            // too, whose right-click is not the folder's.
            root_button.add_controller(context);
            root_button.connect_clicked(move |_| open_menu());
        }
        // A click takes what moves the branch forward and nothing else; a
        // hold rebases, replaying the branch's own commits, which a click
        // that needed it says (David, 2026-10-15: "Like with other
        // press-and-hold buttons, show a toast if the user needs to hold").
        for (from, hold_button) in [
            (PullFrom::Own, &pull_hold),
            (PullFrom::Canonical, &canon_hold),
        ] {
            for (mode, hold) in [(Pull::FastForward, false), (Pull::Rebase, true)] {
                let weak_pull = Rc::downgrade(&tree);
                let run = move || {
                    if let Some(tree) = weak_pull.upgrade() {
                        let button = match from {
                            PullFrom::Own => tree.pull_button.clone(),
                            PullFrom::Canonical => tree.canon_button.clone(),
                        };
                        tree.begin_sync_op(&button);
                        tree.sync(mode, from);
                    }
                };
                if hold {
                    hold_button.set_on_confirm(run);
                } else {
                    hold_button.set_on_click(run);
                }
            }
        }
        // Fetch, as its tooltip says, and nothing else: it ran the whole
        // Pull, which was a no-op only while the rebase target was the
        // branch's own tip, and became an 8274-commit rebase the moment it
        // was a fork's base instead (David, 2026-10-02: "I should not be
        // getting questions about the rebase from running 'fetch the
        // remote'").
        sync_button.connect_clicked(move |button| {
            if let Some(tree) = weak.upgrade() {
                tree.begin_sync_op(button);
                tree.sync(Pull::Fetch, PullFrom::Own);
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
                tree.clear_search();
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
                tree.clear_search();
                tree.rebuild();
            });
        }
        for toggle in [&ignored_toggle, &synced_toggle] {
            let weak = Rc::downgrade(&tree);
            toggle.connect_toggled(move |_| {
                if let Some(tree) = weak.upgrade() {
                    let ignored = tree.ignored_toggle.is_active();
                    // Every file is every synced one too: the synced toggle
                    // decides nothing while ignored files are shown, and
                    // says so rather than reading "off" over synced rows
                    // (review, 2026-10-20).
                    tree.synced_toggle.set_sensitive(!ignored);
                    let showing = Showing::of(ignored, tree.synced_toggle.is_active());
                    tree.set_showing(showing);
                }
            });
        }

        // Focus leaving the tree takes the highlight back to the file in
        // front: a click on a folder's arrow selects its row without
        // activating it, so nothing gave it back, and the folder stayed
        // lit beside the PDF being read (David, 2026-10-20: "The wrong
        // thing is selected in files despite focus being on the PDF").
        // Inside the tree a lit row is where the keyboard is, and stays.
        {
            let focus = gtk::EventControllerFocus::new();
            let weak = Rc::downgrade(&tree);
            focus.connect_leave(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                if tree.pending_select.borrow().is_none() && !tree.selection_held.get() {
                    tree.keep_selection_on_front();
                }
            });
            tree.list_holder.add_controller(focus);
        }

        // The panel does not aim the panes itself: the window owns that
        // transition, because it also drops the environment's watcher and
        // re-aims the editor, and two places deciding what "watching"
        // means is how they come to disagree. The panel's own current-view
        // marker follows from `aim_at`, which the window calls back into.
        tree.backlog.set_current(None);
        {
            // The panel's X: an ad-hoc flow cancelled gives the filter views
            // their selection pane back (`dismiss_intervention`).
            let weak = Rc::downgrade(&tree);
            tree.intervention.set_on_dismiss(move || {
                if let Some(tree) = weak.upgrade() {
                    tree.dismiss_intervention();
                }
            });
        }

        tree.refresh_status();
        tree.rebuild();
        tree.rebuild_index();
        // The kernel task's row says how the notebooks' kernel stands, and
        // follows it.
        {
            let weak = Rc::downgrade(&tree);
            crate::kernels::kernels().connect(move || {
                if let Some(tree) = weak.upgrade() {
                    tree.render_tasks();
                }
            });
        }
        tree
    }

    /// What the tree is looking at: the watched environment's clone, or the
    /// primary's checkout — in its own world, which is a path in the VM
    /// once the registry has placed it there.
    fn view_root(&self) -> PathBuf {
        match self.watching.borrow().as_ref() {
            Some(aim) => aim.checkout.path().to_path_buf(),
            None => self.workspace.checkout_path(),
        }
    }

    /// The repository on this host whose REFS the git views read — log,
    /// branches, ahead/behind, review, issues, and the fetch and push that
    /// carry the user's keys: the watched clone, or the folder the user
    /// opened, which is the primary's peer once its checkout has moved.
    fn refs_root(&self) -> PathBuf {
        match self.watching.borrow().as_ref() {
            Some(aim) => aim.peer.clone(),
            None => self.workspace.root().to_path_buf(),
        }
    }

    /// The working tree the panes act on — status, stage, commit, discard,
    /// switch, stash, rebase — wherever it is. One call per operation
    /// (`taste_devcontainer::Worktree`); a tree in the VM syncs the peer
    /// after every ref it moves.
    fn worktree(&self) -> Worktree {
        match self.watching.borrow().as_ref() {
            // The watched environment's checkout wherever it is, read
            // through its own files service — the VM's keeper, when it is
            // in one (David, 2026-09-21: "But they should be browsable
            // now?"). Read-only here, so no ref hook.
            Some(aim) => Worktree::for_checkout(&aim.checkout, aim.files.clone()),
            None => {
                let tree =
                    Worktree::for_checkout(&self.workspace.checkout(), self.workspace.files());
                match self.after_ref_change.borrow().as_ref() {
                    Some(hook) => tree.with_after_ref_change(hook.clone()),
                    None => tree,
                }
            }
        }
    }

    /// What paths in the working tree are relative to. The discovered
    /// repository's workdir on this host — the folder may sit inside a
    /// larger repository — and the checkout's own root in the VM.
    fn worktree_root(&self) -> Option<PathBuf> {
        let watched = self.watching.borrow();
        let local = match watched.as_ref() {
            Some(aim) => aim.checkout.is_local(),
            None => self.workspace.checkout().is_local(),
        };
        if local {
            self.git
                .borrow()
                .as_ref()
                .map(|g| g.workdir().to_path_buf())
        } else {
            Some(match watched.as_ref() {
                Some(aim) => aim.checkout.path().to_path_buf(),
                None => self.workspace.checkout_path(),
            })
        }
    }

    /// The root the write policy is asked against: the folder the user
    /// opened while watching (so every path in a clone is refused, as
    /// before), and otherwise the checkout wherever it is.
    fn policy_root(&self) -> PathBuf {
        if self.read_only() {
            self.workspace.root().to_path_buf()
        } else {
            self.view_root()
        }
    }

    /// List `showing`'s files from now on: the tree is listed again —
    /// never over search results, or a filter view, which never lists
    /// ignored files anyway; the choice applies when the tree next shows.
    fn set_showing(self: &Rc<Self>, showing: Showing) {
        if self.showing.replace(showing) == showing {
            return;
        }
        if self.query.borrow().is_empty() && !self.filters_active() {
            self.rebuild();
        }
    }

    /// The window's: the primary's working copy is somewhere else now, and
    /// how the checkout in the VM keeps the peer in step. Everything that
    /// was read from the old place is read again from the new one.
    pub fn set_peer_hooks(
        &self,
        after_ref_change: std::sync::Arc<dyn Fn() + Send + Sync>,
        share_remotes: std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
        note_agent: std::sync::Arc<dyn Fn(String) + Send + Sync>,
    ) {
        *self.after_ref_change.borrow_mut() = Some(after_ref_change);
        *self.share_remotes.borrow_mut() = Some(share_remotes);
        *self.note_agent.borrow_mut() = Some(note_agent);
    }

    /// The primary's checkout moved (`Event::CheckoutMoved`): re-aim at the
    /// same environment, whose files are somewhere else now. What
    /// `aim_at` does for a change of target, for a change of place.
    pub fn rehome(self: &Rc<Self>) {
        if self.watching.borrow().is_some() {
            return;
        }
        self.close_open_menu();
        self.close_intervention();
        self.selection.borrow_mut().clear();
        self.status.borrow_mut().clear();
        self.stashed.borrow_mut().clear();
        self.rendered_non_repo.set(false);
        *self.index.borrow_mut() = None;
        self.refresh_status();
        self.rebuild();
        self.rebuild_index();
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
    pub fn aim_at(self: &Rc<Self>, target: Option<Aim>) {
        if *self.watching.borrow() == target {
            return;
        }
        self.close_open_menu();
        *self.watching.borrow_mut() = target;
        let root = self.view_root();
        // Opening the repository touches disk (`.git`, packed-refs, HEAD),
        // so it runs on the blocking pool. Cleared to `None` right away
        // rather than left holding the OLD checkout's handle: the
        // commit/push/sync actions and `repo_relative` all read `self.git`
        // synchronously at click time, and a stale handle pointed at
        // another checkout is the one answer worse than "not ready yet".
        // The header and rows do not wait on this — `refresh_status`
        // below runs its own independent discovery and repaints them —
        // this handle only backs the click-driven actions and the
        // relative-path lookup `state_of` uses while binding rows.
        *self.git.borrow_mut() = None;
        {
            let weak = Rc::downgrade(self);
            let discover_root = self.refs_root();
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime()
                    .spawn_blocking(move || GitWorkspace::discover(&discover_root));
                let Ok(git) = handle.await else { return };
                let Some(tree) = weak.upgrade() else { return };
                // Another `aim_at` (or a return to this same target) may
                // have landed while this discovery was in flight; apply it
                // only if it is still the checkout this pane is aimed at.
                if tree.view_root() == root {
                    *tree.git.borrow_mut() = git;
                }
            });
        }
        self.selection.borrow_mut().clear();
        // A review belongs to the checkout it was opened against, so
        // aiming the panes elsewhere leaves it rather than showing one
        // environment's branch over another's files.
        self.leave_review();
        self.close_intervention();
        self.clear_search();
        *self.search_view.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.stashed.borrow_mut().clear();
        // The counts were the old checkout's.
        self.clear_sync_counts();
        // Force the next status snapshot through the unchanged-guard: the
        // maps above are empty now, so anything the new checkout has is a
        // change, and an EMPTY one still has to repaint the rows.
        self.rendered_non_repo.set(false);
        // The panel is the indicator, and the only one: it says where the
        // panes are aimed, tints itself when that is not home, and holds
        // the way back. The root row goes back to naming the project.
        self.backlog.set_current(self.watching());
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

    /// Set the sync row's status text (rebase-paused notice, "no
    /// upstream"), with the full sentence in the tooltip: same convention
    /// as [`FileTree::set_branch_label`], and for the same reason — the
    /// label ellipsizes now, so an empty string clears the tooltip along
    /// with the text rather than leaving a stale one behind.
    fn set_sync_label(&self, text: &str) {
        self.sync_label.set_label(text);
        self.sync_label
            .set_tooltip_text((!text.is_empty()).then_some(text));
    }

    /// Which environment the panes are aimed at (`None` = the primary).
    pub fn watching(&self) -> Option<taste_core::environment::EnvironmentId> {
        self.watching.borrow().as_ref().map(|aim| aim.env.clone())
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
    /// the primary included, because "back to Personal" is that row like
    /// any other and not a second kind of action.
    pub fn set_on_open_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_select(hook);
    }

    /// Start, from the backlog's composer: the window makes the issue's
    /// environment and its chat (`window.rs`).
    pub fn set_on_start_issue(&self, hook: impl Fn(crate::backlog::StartedIssue) + 'static) {
        self.backlog.set_on_start(hook);
    }

    pub fn set_on_stop_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_stop(hook);
    }

    /// Is the backlog on screen right now?
    ///
    /// The notifier's "do not tell them what they can already see" test for
    /// the two fleet-shaped notices — a build that failed, an environment
    /// asking for review. Each is a row here, with its own light and its
    /// own accent rail, so a visible list IS the news. It used to ask the
    /// console whether its environment tab was the selected page; that tab
    /// is gone, and this is the surface that inherited the question.
    ///
    /// The panel folds (`wire_collapse`) and the whole flank can be
    /// closed, so this is a real question and not a constant.
    pub fn backlog_on_screen(&self) -> bool {
        self.backlog.list_is_on_screen()
    }

    /// Rename, from the backlog row's `⋮` menu: the console owns the
    /// workspace state the name lives in, and the intervention that asks
    /// for it.
    pub fn set_on_rename_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_rename(hook);
    }

    /// Nuke, from the same menu: the container and its image, so the next
    /// start rebuilds from scratch.
    pub fn set_on_nuke_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_nuke(hook);
    }

    /// Open Review, from the same menu: the git views aimed at that
    /// environment's branch of record.
    pub fn set_on_open_review(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_open_review(hook);
    }

    /// Refresh, from the backlog header: re-read every environment's
    /// branches, published work, podman resources and disk footprint.
    pub fn set_on_refresh_environments(&self, hook: impl Fn() + 'static) {
        self.backlog.set_on_refresh_environments(hook);
    }

    /// The backlog's intervention panel, for a flow this column does not
    /// own — the console's rename, destroy and reject. They are about an
    /// environment, and an environment is a backlog row, so they open
    /// under the backlog's list. Opening replaces whatever was in it.
    pub fn open_named_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
        self.backlog.open_panel(title)
    }

    /// ...and the way back out of one.
    pub fn dismiss_named_intervention(self: &Rc<Self>) {
        self.backlog.close_panel();
    }

    pub fn set_on_destroy_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_destroy(hook);
    }

    pub fn set_on_rebuild_environment(
        &self,
        hook: impl Fn(taste_core::environment::EnvironmentId) + 'static,
    ) {
        self.backlog.set_on_rebuild(hook);
    }

    /// Called on the panel's own tick, so a list that is always on screen
    /// says what is true now rather than what was true when something last
    /// moved.
    pub fn set_on_panel_tick(&self, hook: impl Fn() + 'static) {
        self.backlog.set_on_tick(hook);
    }

    /// The assembled fleet: the panel's rows, their lights, their names and
    /// what each is working on — and the backlog's started_by lookup, which
    /// resolves a slug through these same rows so the queue's tooltip and
    /// the panel cannot disagree about what an environment is called.
    pub fn set_fleet(&self, rows: &[crate::fleet::FleetRow]) {
        self.backlog.set_fleet(rows);
    }

    /// The workspace's issue queue, in the ref's own order. Read by the
    /// console (which is where the off-thread git passes live) and handed
    /// here, so there is one read of `refs/taste/issues` per change and not
    /// one per surface that renders it.
    pub fn set_issues(&self, issues: &[taste_git::Issue]) {
        self.backlog.set_issues(issues);
    }

    /// Select an issue in the backlog by id — a pill in a transcript was
    /// clicked (`crate::issue_pill`).
    pub fn reveal_issue(&self, id: &str) {
        self.backlog.reveal_issue(id);
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

    /// TASTE_PROBE_CHECK only: plant fabricated activity windows on the
    /// environment panel's rows, so a headless shot has sparklines in it.
    /// Paired with the console's `seed_fleet_for_probe`, which is what put
    /// those rows there.
    pub fn seed_activity_for_probe(&self, shapes: &[(&str, crate::backlog::Shape)]) {
        for (slug, shape) in shapes {
            if let Ok(env) = taste_core::environment::EnvironmentId::parse(slug) {
                self.backlog.seed_activity_for_probe(&env, *shape);
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
        // Below the breakpoint the backlog is the bottom of the WINDOW
        // rather than the bottom of a pane, and there is nothing under it
        // to hand the leftover height to.
        self.backlog.set_filling(true);
        let panels: Vec<gtk::Widget> = vec![self.backlog.widget.clone().upcast()];
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
        // ...and back in the pane the tree is what grows, so the backlog
        // goes back to being a capped strip. The exact inverse, or
        // stretching the window back would leave the tree squeezed under a
        // backlog that had kept the whole flank.
        self.backlog.set_filling(false);
        for panel in panels {
            if panel.parent().is_none() {
                self.widget.append(&panel);
            }
        }
    }

    /// TASTE_PROBE_CHECK only: fold the backlog away, for the shots that
    /// are about something above it.
    /// TASTE_PROBE_CHECK only: open one backlog row's context menu, so a
    /// still frame can show what the rows do.
    pub fn seed_backlog_actions_for_probe(&self, id: &str) {
        self.backlog.seed_menu_for_probe(id);
    }

    /// TASTE_PROBE_CHECK only: the Dirty filter view, through its toggle.
    pub fn seed_dirty_view_for_probe(&self) {
        self.dirty_toggle.set_active(true);
    }

    /// TASTE_PROBE_CHECK only: the Stashed filter view with its first two
    /// rows checked — the state a click and a shift-click leave — so the
    /// pane's Unstash and its held trash are in the frame. The rows are
    /// the checkout's own stash, listed a moment after the toggle.
    pub fn seed_stashed_view_for_probe(self: &Rc<Self>) {
        self.stashed_toggle.set_active(true);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            let Some(tree) = weak.upgrade() else { return };
            let mut rows: Vec<(i32, gtk::CheckButton)> = tree
                .changed_rows
                .borrow()
                .values()
                .map(|handle| (handle.row.index(), handle.check.clone()))
                .collect();
            rows.sort_by_key(|(at, _)| *at);
            for (_, check) in rows.into_iter().take(2) {
                check.set_active(true);
            }
        });
    }

    /// Put the keyboard in the environment panel (Ctrl+Shift+E). Nothing
    /// opens — the list is already there — so this focuses the row the
    /// panes are aimed at, and walks the list on repeat presses.
    pub fn focus_environment_panel(&self) {
        self.backlog.focus();
    }

    /// Everything that writes is disabled — never hidden — while watching,
    /// with two exceptions.
    ///
    /// Disabled controls still say what the pane can do; hiding them would
    /// make a watched checkout look like a different, smaller application.
    /// But **Fetch and Pull work**, and the Abort and Continue of the rebase
    /// a Pull can pause on: taking Personal's commits into an environment
    /// is the user's to do from here (David, 2026-10-04: "I ought to be able
    /// to use the GUI pull button in these envs, too"), and it moves the
    /// environment's branch onto `personal/<branch>` without a byte of the
    /// user's own editing. Push stays, disabled like the rest, but without
    /// the tooltip a snapshot gives it — "Push 1 commit to origin/main"
    /// offered to push the environment's branch into the user's own, which
    /// is not a thing this view does (David, 2026-10-04: "I just want it
    /// disabled without such a dumb message"); an environment's work
    /// reaches the user's branch by being published and reviewed.
    fn apply_view_permissions(&self) {
        let read_only = self.read_only();
        if read_only {
            for button in [&self.push_button, &self.init_button] {
                button.set_sensitive(false);
            }
            *self.push_card.borrow_mut() = Some(
                crate::sync_card::SyncCard::new("Read-only")
                    .gesture("An environment's work reaches your branch by Publish and review"),
            );
            self.branch_label.set_sensitive(false);
            self.branch_label
                .set_tooltip_text(Some("Read-only: this is another environment's checkout"));
            self.commit_box.set_sensitive(false);
        } else {
            self.branch_label.set_sensitive(true);
            self.branch_label.set_tooltip_text(None);
        }
        // The IDE's own log is the IDE's, not an environment's: the same
        // stream whichever checkout the panes are aimed at, and offered
        // beside another environment's logs it reads as that environment's.
        // Greyed, not hidden, while watching (David, 2026-09-06: "Gray out
        // the IDE logs option for the non-personal env").
        if let Some(index) = crate::logview::LogKind::ALL
            .iter()
            .position(|kind| *kind == crate::logview::LogKind::Ide)
        {
            if let Some(row) = self.logs_list.row_at_index(index as i32) {
                row.set_sensitive(!read_only);
                row.set_tooltip_text(read_only.then_some(
                    "The IDE's own log is the same for every environment; read it from Personal",
                ));
            }
        }
    }

    /// Push and Pull with nothing to count against: disabled, at zero,
    /// and saying nothing. Their faces and tooltips are otherwise only ever
    /// written by a snapshot WITH an upstream, so what the last such one
    /// said stayed — another environment's "Push 1 commit to origin/main"
    /// shown on Personal, which has no remote at all (David, 2026-10-04:
    /// "the push button on my personal env showed that I had 1 commit to
    /// push to *myself*").
    fn clear_sync_counts(&self) {
        self.push_face.set_label(" 0");
        self.push_face.remove_css_class(WARNED);
        self.pull_face.set_label(" 0");
        self.pull_face.remove_css_class(WARNED);
        self.canon_face.remove_css_class(WARNED);
        self.canon_face.set_label(" 0");
        self.canon_button.set_visible(false);
        for button in [&self.push_button, &self.pull_button, &self.canon_button] {
            button.set_sensitive(false);
        }
        for card in [&self.push_card, &self.pull_card, &self.canon_card] {
            card.borrow_mut().take();
        }
    }

    /// The refusal a read-only view gives, naming the environment.
    fn refuse_read_only(&self) -> bool {
        let Some(Aim { env, .. }) = self.watching.borrow().clone() else {
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
        self.report_index(0, true);
        let root = self.view_root();
        let files = self.worktree().files();
        let (tx, rx) = async_channel::unbounded::<usize>();
        let handle = crate::runtime::runtime().spawn_blocking(move || {
            if files.is_local() {
                taste_core::search::collect_files(&root, |count| {
                    let _ = tx.try_send(count);
                })
            } else {
                // `rg --files` beside the files: one answer, no progress
                // to report on the way.
                taste_core::search::collect_files_via(&files, &root)
            }
        });
        {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                while let Ok(count) = rx.recv().await {
                    if let Some(tree) = weak.upgrade() {
                        tree.report_index(count, true);
                    }
                }
            });
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let files = handle.await.unwrap_or_default();
            let Some(tree) = weak.upgrade() else { return };
            let count = files.len();
            *tree.index.borrow_mut() = Some(std::sync::Arc::new(files));
            tree.report_index(count, false);
            tree.index_building.set(false);
            // A query typed while the index was building searched a walk of
            // its own; now that the index is here, answer it from the index.
            let query = tree.query.borrow().clone();
            if !query.is_empty() {
                tree.run_search(query.text.clone());
            }
        });
    }

    /// The tree's answer to the one query (search.rs). Filtering the tree
    /// is fast and lands at once; the content search reports its progress
    /// as it goes, and stops when the next query arrives.
    pub fn attach_search(self: &Rc<Self>, search: &Rc<crate::search::Search>) {
        *self.search.borrow_mut() = Some(search.clone());
        self.backlog.attach_search(search);
        let weak = Rc::downgrade(self);
        search.subscribe("tree", move |query, _generation| {
            let Some(tree) = weak.upgrade() else { return };
            tree.apply_query(query.clone());
        });
        let weak = Rc::downgrade(self);
        search.register_stepper(crate::search::Panel::Files, move |step| {
            weak.upgrade().is_some_and(|tree| tree.step(step))
        });
        // The Ports and Logs rows are stops of their own, stepped in place
        // (search.rs::ListStepper); each section's banner is the stop's
        // placeholder when it has nothing.
        let ports_stepper = crate::search::ListStepper::new(&self.ports_list);
        search.register_stepper(crate::search::Panel::Ports, move |step| {
            ports_stepper.step(step)
        });
        let logs_stepper = crate::search::ListStepper::new(&self.logs_list);
        search.register_stepper(crate::search::Panel::Logs, move |step| {
            logs_stepper.step(step)
        });
        search.register_placeholder(crate::search::Panel::Files, &self.files_results);
        search.register_placeholder(crate::search::Panel::Ports, &self.ports_results);
        search.register_placeholder(crate::search::Panel::Logs, &self.logs_results);
        let tasks_stepper = crate::search::ListStepper::new(&self.tasks_list);
        search.register_stepper(crate::search::Panel::Tasks, move |step| {
            tasks_stepper.step(step)
        });
        search.register_placeholder(crate::search::Panel::Tasks, &self.tasks_results);
        crate::search::Search::tab_switches_panels(&self.tasks_list, search);
        // Tab from a row of the tree: the next panel with results.
        crate::search::Search::tab_switches_panels(&self.list_holder, search);
        crate::search::Search::tab_switches_panels(&self.ports_list, search);
        crate::search::Search::tab_switches_panels(&self.logs_list, search);
        // The arrows, in the lists stepped in place, step their own hits.
        for (list, panel) in [
            (&self.ports_list, crate::search::Panel::Ports),
            (&self.tasks_list, crate::search::Panel::Tasks),
            (&self.logs_list, crate::search::Panel::Logs),
        ] {
            crate::search::Search::arrows_step_panel(list, search, panel);
        }
    }

    fn apply_query(self: &Rc<Self>, query: crate::search::Query) {
        let was_empty = self.query.borrow().is_empty();
        *self.query.borrow_mut() = query.clone();
        // The Ports rows wear the query's badge; the Logs rows' badges come
        // from the window, which holds the logs.
        self.render_ports();
        self.render_tasks();
        if query.is_empty() {
            self.meaning_hits.borrow_mut().clear();
            self.files_results.hide();
            self.logs_results.hide();
            // ...and the Logs rows come back: badges off, everything shown.
            self.refresh_log_rows();
        } else {
            // The files' count arrives when the content search lands
            // (`run_search`); until then the banner says it is looking.
            self.files_results.show_count(&query, "files", 0, true);
            self.refresh_log_rows();
        }
        if query.is_empty() {
            if let Some(previous) = self.search_cancel.borrow_mut().take() {
                previous.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *self.search_view.borrow_mut() = None;
            self.files_matched.borrow_mut().clear();
            self.branch_count.set_visible(false);
            if !was_empty {
                if self.filters_active() {
                    self.render_filter_view();
                } else {
                    self.rebuild();
                }
            }
            return;
        }
        self.run_search(query.text.clone());
    }

    /// A path and every ancestor under the root into the visible set.
    fn make_visible(visible: &mut HashSet<PathBuf>, path: &Path, root: &Path) {
        visible.insert(path.to_path_buf());
        let mut current = path;
        while let Some(parent) = current.parent() {
            if parent == root || !parent.starts_with(root) {
                break;
            }
            visible.insert(parent.to_path_buf());
            current = parent;
        }
    }

    /// The files the query found by meaning, per file: count and first
    /// line. Empty when the toggle is off or the index has not answered.
    fn meaning_by_file(&self) -> HashMap<PathBuf, (usize, u32)> {
        let mut by_file: HashMap<PathBuf, (usize, u32)> = HashMap::new();
        for hit in self.meaning_hits.borrow().iter() {
            let entry = by_file
                .entry(hit.path.clone())
                .or_insert((0, hit.start_line));
            entry.0 += 1;
            entry.1 = entry.1.min(hit.start_line);
        }
        by_file
    }

    /// The Files lozenge's number: how many files the query reached — by
    /// the word, or, while the meaning toggle is on, by the idea.
    ///
    /// A union, not a sum. The lozenge counts FILES, and a file the query
    /// reaches both ways is one file; so is a file whose name matches and
    /// whose contents do (David, 2026-09-08: "the sums should include all
    /// matches. (Of course, don't count AI matches if AI matches are
    /// disabled for the search.)"). Called from both ends, because the
    /// word's answer and the index's arrive at different moments and
    /// either one moves the number.
    fn report_files_hits(&self) {
        let Some(search) = self.search.borrow().clone() else {
            return;
        };
        let query = self.query.borrow();
        if query.is_empty() {
            search.set_panel_hits(crate::search::Panel::Files, 0);
            return;
        }
        let reached = files_reached(
            &self.files_matched.borrow(),
            &self.meaning_hits.borrow(),
            query.meaning,
        );
        search.set_panel_hits(crate::search::Panel::Files, reached);
    }

    /// The files banner: the literal count, and — when the index has
    /// answered — how many files the query reached by meaning alone, so
    /// what the ≈ badges add is said in words too.
    fn show_files_banner(&self, running: bool) {
        let query = self.query.borrow().clone();
        if query.is_empty() {
            self.files_results.hide();
            return;
        }
        self.files_results
            .show_count(&query, "files", self.files_hits.get(), running);
        let by_meaning = self.meaning_by_file().len();
        if by_meaning > 0 {
            self.files_results.note(&format!(
                "{by_meaning} file{} by meaning",
                if by_meaning == 1 { "" } else { "s" }
            ));
        }
    }

    /// The semantic index answered (or the toggle went off: an empty
    /// answer). Files found by meaning join the search view — kept, with a
    /// ≈ badge — without a second walk of the checkout.
    pub fn set_meaning_hits(self: &Rc<Self>, hits: Vec<crate::search::MeaningHit>) {
        *self.meaning_hits.borrow_mut() = hits;
        self.report_files_hits();
        if self.query.borrow().is_empty() {
            return;
        }
        self.show_files_banner(false);
        let Some(view) = self.search_view.borrow().clone() else {
            return;
        };
        let root = self.view_root();
        let meaning = self.meaning_by_file();
        let mut visible: HashSet<PathBuf> = HashSet::new();
        for path in view.hits.keys() {
            Self::make_visible(&mut visible, path, &root);
        }
        for path in view.visible.iter() {
            visible.insert(path.clone());
        }
        for path in meaning.keys() {
            if !view.hits.contains_key(path) {
                Self::make_visible(&mut visible, path, &root);
            }
        }
        *self.search_view.borrow_mut() = Some(Rc::new(SearchView {
            hits: view.hits.clone(),
            counts: view.counts.clone(),
            meaning,
            visible: Rc::new(visible),
            pinned: view.pinned.clone(),
        }));
        self.rebuild();
    }

    fn clear_search(self: &Rc<Self>) {
        if let Some(search) = self.search.borrow().clone() {
            search.clear();
        }
    }

    fn report_index(&self, count: usize, running: bool) {
        if let Some(search) = self.search.borrow().as_ref() {
            search.report(
                "index",
                crate::search::Status {
                    hits: 0,
                    done: count,
                    total: count.max(1),
                    running,
                },
            );
        }
    }

    /// Down and Up from the search box move the tree's selection; Enter
    /// opens (or expands) the selected row.
    fn step(self: &Rc<Self>, step: crate::search::Step) -> bool {
        let Some(list) = self.list_holder.child().and_downcast::<gtk::ListView>() else {
            return false;
        };
        let Some(selection) = list.model().and_downcast::<gtk::SingleSelection>() else {
            return false;
        };
        let count = selection.n_items();
        if count == 0 {
            return false;
        }
        match step {
            crate::search::Step::Next | crate::search::Step::Prev => {
                let current = selection.selected();
                let next = if current == gtk::INVALID_LIST_POSITION {
                    0
                } else if step == crate::search::Step::Next {
                    (current + 1).min(count - 1)
                } else {
                    current.saturating_sub(1)
                };
                selection.set_selected(next);
                list.scroll_to(next, gtk::ListScrollFlags::NONE, None);
                // From the box, the box keeps the keyboard and the selection
                // moves under it; arriving by Tab from another listing, the
                // keyboard comes along.
                if self
                    .search
                    .borrow()
                    .as_ref()
                    .is_some_and(|search| !search.box_has_focus())
                {
                    list.grab_focus();
                }
                true
            }
            // Nothing to take down. What this stop lights is the tree's
            // own SELECTION, and that is not a search cursor: it is the
            // file the tree has selected, which follows the editor's tab
            // (`select_for_editor`). Clearing it when the stop moves on
            // would lose the user's place in their own file list.
            crate::search::Step::Leave => false,
            crate::search::Step::Activate => {
                let position = selection.selected();
                if position == gtk::INVALID_LIST_POSITION {
                    return false;
                }
                list.emit_by_name::<()>("activate", &[&position]);
                true
            }
        }
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

    /// A Logs row was activated: open that log for the environment the
    /// panes are aimed at.
    pub fn set_on_open_log(
        &self,
        f: impl Fn(taste_core::environment::EnvironmentId, crate::logview::LogKind) + 'static,
    ) {
        *self.on_open_log.borrow_mut() = Some(Box::new(f));
    }

    /// A Tasks row was activated: open that task's output for the
    /// environment the panes are aimed at.
    pub fn set_on_open_task(
        &self,
        f: impl Fn(taste_core::environment::EnvironmentId, String) + 'static,
    ) {
        *self.on_open_task.borrow_mut() = Some(Box::new(f));
    }

    /// A Tasks row's button: run the task, or stop it when it is running.
    pub fn set_on_run_task(
        &self,
        f: impl Fn(taste_core::environment::EnvironmentId, String) + 'static,
    ) {
        *self.on_run_task.borrow_mut() = Some(Box::new(f));
    }

    /// The Tasks section's rows: the tasks with their last runs, why they
    /// cannot be run when they cannot, and whether there is a Taskfile.
    pub fn set_tasks(
        self: &Rc<Self>,
        rows: Vec<TaskRow>,
        cannot_run: Option<String>,
        has_taskfile: bool,
    ) {
        let next = (rows, cannot_run, has_taskfile);
        if *self.tasks.borrow() == next {
            return;
        }
        *self.tasks.borrow_mut() = next;
        self.render_tasks();
        // A service's state is its port's button too.
        if !self.services.borrow().is_empty() {
            self.render_ports();
        }
    }

    fn render_tasks(self: &Rc<Self>) {
        while let Some(child) = self.tasks_list.first_child() {
            self.tasks_list.remove(&child);
        }
        let (rows, cannot_run, has_taskfile) = self.tasks.borrow().clone();
        let runnable = cannot_run.is_none();
        // The one query here as in Ports and Logs: a task's hits are the
        // matches in its own name and description and inside its output.
        let query = self.query.borrow().clone();
        let hits: Vec<usize> = rows
            .iter()
            .map(|row| {
                if query.is_empty() {
                    return 0;
                }
                query.ranges(&row.info.name).len()
                    + query.ranges(&row.info.desc).len()
                    + self
                        .task_hits
                        .borrow()
                        .get(&row.info.name)
                        .copied()
                        .unwrap_or(0)
            })
            .collect();
        let total: usize = hits.iter().sum();
        let names: Vec<String> = rows.iter().map(|row| row.info.name.clone()).collect();
        let outline = crate::tasks::outline(&names);
        let mut row_names = Vec::with_capacity(outline.len());
        let mut kept_height = 0;
        // Which namespaces are open: the ones opened by hand, and — while a
        // query stands — every one with a hit inside, so a match is never
        // folded out of sight.
        let mut open = self.tasks_open.borrow().clone();
        if !query.is_empty() {
            for (at, entry) in outline.iter().enumerate() {
                if entry.folds && subtree_hits(&outline, at, &hits) > 0 {
                    open.insert(entry.path.clone());
                }
            }
        }
        // A line shows when every namespace above it is open: a heading's
        // own path is its to fold, a task's the one it sits in.
        let shown = |entry: &crate::tasks::OutlineEntry| {
            let mut above: Vec<&str> = Vec::new();
            let parts: Vec<&str> = entry.path.split(':').filter(|p| !p.is_empty()).collect();
            let upto = if entry.folds {
                parts.len().saturating_sub(1)
            } else {
                parts.len()
            };
            (1..=upto).all(|n| {
                above.clear();
                above.extend_from_slice(&parts[..n]);
                open.contains(&above.join(":"))
            })
        };
        let states: Vec<crate::tasks::RunState> = rows.iter().map(|row| row.state).collect();
        for (at, entry) in outline.iter().enumerate() {
            // Whether this line or anything under it answers the query: a
            // heading stays for its tasks' sake.
            let subtree = subtree_hits(&outline, at, &hits);
            let below = subtree_states(&outline, at, &states);
            let fold = entry
                .folds
                .then(|| (entry.path.as_str(), open.contains(&entry.path), below));
            let item = match entry.task {
                Some(index) => {
                    let row = &rows[index];
                    row_names.push(Some(row.info.name.clone()));
                    self.task_row(row, &entry.label, hits[index], runnable, fold)
                }
                None => {
                    row_names.push(None);
                    self.task_heading(&entry.label, &entry.path, open.contains(&entry.path), below)
                }
            };
            // The depth's indent, and the row's start inset — except on a
            // heading that is a task, whose fold strip already holds that
            // inset (`task_row`), so stating it here too put the whole row
            // ROW_INSET right of the column.
            if let Some(child) = item.child() {
                let inset = if entry.task.is_some() && entry.folds {
                    0
                } else {
                    ROW_INSET
                };
                child.set_margin_start(inset + entry.depth as i32 * TASK_INDENT);
            }
            let unmatched = !query.is_empty() && subtree == 0;
            let hidden = !shown(entry) || (unmatched && !query.ghost);
            if hidden {
                item.set_visible(false);
            } else if unmatched {
                item.add_css_class("search-dim");
            }
            if !hidden {
                kept_height += if entry.task.is_some() { 40 } else { 28 };
            }
            self.tasks_list.append(&item);
        }
        // Never under two rows, the ones the query keeps: a scroller's own
        // minimum is nothing, and a short column would take the section to
        // a sliver. Only two — its natural height is up to the cap, which
        // it gets where there is room; a floor of all five would raise the
        // window's own minimum past a 900px screen.
        self.tasks_scroller
            .set_min_content_height(kept_height.min(TASKS_MIN_HEIGHT));
        // And nothing at all with no row kept — no tasks, or a query that
        // matched none of them: an empty scroller still takes its
        // scrollbar's height, a blank band under the section's header. (A
        // second, older line below set it visible again whenever there
        // were tasks at all, which is why the band outlived this one.)
        self.tasks_scroller.set_visible(kept_height > 0);
        *self.task_row_names.borrow_mut() = row_names;
        if let Some(search) = self.search.borrow().as_ref() {
            search.set_panel_hits(crate::search::Panel::Tasks, total);
        }
        if query.is_empty() {
            self.tasks_results.hide();
        } else {
            self.tasks_results.show_count(&query, "tasks", total, false);
        }
        match &cannot_run {
            Some(why) => {
                let mut said = why.clone();
                if let Some(first) = said.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                self.tasks_note.set_label(&format!("{said}."));
                self.tasks_note.set_visible(true);
            }
            None => self.tasks_note.set_visible(false),
        }
        self.tasks_ghost_label.set_label(if has_taskfile {
            "Add more in the Taskfile."
        } else {
            "Add tasks in a Taskfile.yml."
        });
        // The tab in front keeps its row lit through a redraw.
        let front = self.task_in_front.borrow().clone();
        let lit = front
            .and_then(|name| {
                self.task_row_names
                    .borrow()
                    .iter()
                    .position(|row| row.as_deref() == Some(name.as_str()))
            })
            .and_then(|i| self.tasks_list.row_at_index(i as i32));
        self.tasks_list.select_row(lit.as_ref());
    }

    /// A namespace's heading in the Tasks outline: a disclosure arrow and
    /// the namespace. A click opens or folds it (David, 2026-09-23: "This
    /// doesn't seem to drill down for sections like \"dev\""); it runs
    /// and opens nothing, so it is neither selectable nor activated, and
    /// nothing lights it.
    fn task_heading(
        self: &Rc<Self>,
        label: &str,
        path: &str,
        open: bool,
        below: FoldStates,
    ) -> gtk::ListBoxRow {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
        // Both insets, as `section_row` states them: with only the end one
        // the arrow sat ROW_INSET left of every task's light below it.
        line.set_margin_start(ROW_INSET);
        line.set_margin_end(ROW_INSET);
        let arrow = fold_arrow(open, below);
        line.append(&leading_slot(&arrow));
        // The whole slot, so the lights stand at its end beside the arrow.
        arrow.set_halign(gtk::Align::Fill);
        line.append(
            &gtk::Label::builder()
                .label(label)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        let row = gtk::ListBoxRow::builder()
            .child(&line)
            .activatable(false)
            .selectable(false)
            .css_classes(["task-heading"])
            .tooltip_text(if open {
                format!("Fold the {path} tasks")
            } else {
                format!("Show the {path} tasks{}", fold_words(open, below))
            })
            .build();
        row.set_cursor_from_name(Some("pointer"));
        let click = gtk::GestureClick::new();
        {
            let weak = Rc::downgrade(self);
            let path = path.to_string();
            click.connect_released(move |_, _, _, _| {
                let Some(tree) = weak.upgrade() else { return };
                {
                    let mut open = tree.tasks_open.borrow_mut();
                    if !open.remove(&path) {
                        open.insert(path.clone());
                    }
                }
                tree.render_tasks();
            });
        }
        row.add_controller(click);
        row
    }

    /// One task's row: its last run's light, its name's last part, what it
    /// does, its hits, and the button that runs or stops it.
    fn task_row(
        self: &Rc<Self>,
        row: &TaskRow,
        label: &str,
        hits: usize,
        runnable: bool,
        fold: Option<(&str, bool, FoldStates)>,
    ) -> gtk::ListBoxRow {
        let running = row.state == crate::tasks::RunState::Running;
        let button = gtk::Button::builder()
            .icon_name(if running {
                "media-playback-stop-symbolic"
            } else {
                "media-playback-start-symbolic"
            })
            .tooltip_text(if running {
                format!("Stop {}", row.info.name)
            } else {
                format!("Run {} in the environment", row.info.name)
            })
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .sensitive(runnable || running)
            .build();
        {
            let weak = Rc::downgrade(self);
            let name = row.info.name.clone();
            button.connect_clicked(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                let env = tree.aimed_environment();
                let hook = tree.on_run_task.borrow();
                if let Some(hook) = hook.as_ref() {
                    hook(env, name.clone());
                }
            });
        }
        // The kernel's task says how the notebooks' kernel stands in place
        // of what it does (`crate::kernels`), while a notebook is open.
        let kernel_note = (row.info.name == taste_core::kernel_task::TASK)
            .then(|| crate::kernels::kernels().row_note(&self.aimed_environment()))
            .flatten();
        let desc = kernel_note.as_deref().unwrap_or(row.info.desc.as_str());
        // A service says it is one, and the port it serves.
        let service =
            self.services
                .borrow()
                .get(&row.info.name)
                .map(|service| match service.port {
                    Some(port) => format!("service on {port}"),
                    None => "service".to_string(),
                });
        let desc = match (service, desc) {
            (Some(service), "") => service,
            (Some(service), desc) => format!("{service} · {desc}"),
            (None, desc) => desc.to_string(),
        };
        let desc = desc.as_str();
        let subtitle = match (row.state.words(), desc) {
            ("", desc) => desc.to_string(),
            (words, "") => words.to_string(),
            (words, desc) => format!("{words} · {desc}"),
        };
        let trailing = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        trailing.set_valign(gtk::Align::Center);
        if hits > 0 {
            trailing.append(&crate::search::hit_badge(hits));
        }
        trailing.append(&button);
        let item = section_row(
            fold.is_none().then(|| crate::tasks::dot(row.state)),
            // A slot for the arrow, filled below with one that folds.
            fold.map(|_| "pan-end-symbolic"),
            label,
            &subtitle,
            Some(trailing.upcast_ref()),
        );
        item.set_tooltip_text(Some(&row.info.name));
        // A run under way spins where its light will be: amber is this
        // window's word for a warning, and a task that is running is not
        // one (David, 2026-09-23: "Show a spinner when it's running, not
        // yellow status").
        if running && fold.is_none() {
            if let Some(slot) = item
                .child()
                .and_then(|line| line.first_child())
                .and_downcast::<gtk::Box>()
            {
                while let Some(child) = slot.first_child() {
                    slot.remove(&child);
                }
                let spinner = gtk::Spinner::builder()
                    .spinning(true)
                    .width_request(12)
                    .height_request(12)
                    .valign(gtk::Align::Center)
                    .tooltip_text("Running")
                    .build();
                spinner.set_hexpand(true);
                spinner.set_halign(gtk::Align::Center);
                slot.append(&spinner);
            }
        }
        // A heading that is a task: its arrow folds what is under it, in
        // the slot a leaf's light takes — its state is in its subtitle —
        // and the rest of the row opens it as any task's does.
        //
        // The fold target is the row's whole leading strip — its inset and
        // the slot, top to bottom — rather than the 14-pixel glyph, which
        // was all a click could land on (David, 2026-09-23: "the clickable
        // area for opening up nesting needs to be much larger"). The line
        // gives its start inset up to the strip, and the strip puts the
        // arrow back where the slot centres a light, so the arrow and the
        // lights below it are one column.
        if let (Some((path, open, below)), Some(line)) = (fold, item.child()) {
            if let Some(slot) = line.first_child().and_downcast::<gtk::Box>() {
                while let Some(child) = slot.first_child() {
                    slot.remove(&child);
                }
                line.set_margin_start(0);
                slot.set_width_request(ROW_INSET + LEAD_WIDTH);
                slot.set_valign(gtk::Align::Fill);
                let arrow = fold_arrow(open, below);
                arrow.set_margin_start(ROW_INSET);
                arrow.set_hexpand(true);
                arrow.set_halign(gtk::Align::Fill);
                arrow.set_valign(gtk::Align::Center);
                slot.append(&arrow);
                self.make_fold_target(&slot, path, open, below);
            }
        }
        item
    }

    /// Make `target` fold or open the heading at `path` on a click, which
    /// goes no further — a heading that is a task is not opened by it.
    fn make_fold_target(
        self: &Rc<Self>,
        target: &impl IsA<gtk::Widget>,
        path: &str,
        open: bool,
        below: FoldStates,
    ) {
        target.set_tooltip_text(Some(&if open {
            format!("Fold the {path} tasks")
        } else {
            format!("Show the {path} tasks{}", fold_words(open, below))
        }));
        target.set_cursor_from_name(Some("pointer"));
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let weak = Rc::downgrade(self);
            let path = path.to_string();
            click.connect_pressed(move |gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let Some(tree) = weak.upgrade() else { return };
                {
                    let mut open = tree.tasks_open.borrow_mut();
                    if !open.remove(&path) {
                        open.insert(path.clone());
                    }
                }
                let tree = tree.clone();
                glib::idle_add_local_once(move || tree.render_tasks());
            });
        }
        target.add_controller(click);
    }

    /// The query's hits inside each task's output, by name: badges, and
    /// which rows the query keeps. Redrawn only when a count moved.
    pub fn set_task_hits(self: &Rc<Self>, hits: HashMap<String, usize>) {
        if *self.task_hits.borrow() == hits {
            return;
        }
        *self.task_hits.borrow_mut() = hits;
        self.render_tasks();
    }

    /// The task rows as they are, for the window to count their hits.
    pub fn task_rows(&self) -> Vec<TaskRow> {
        self.tasks.borrow().0.clone()
    }

    /// Each row's light and button again from `state`, without listing
    /// the tasks again: a run started or ended.
    pub fn update_task_states(self: &Rc<Self>, state: impl Fn(&str) -> crate::tasks::RunState) {
        let (mut rows, cannot_run, has_taskfile) = self.tasks.borrow().clone();
        for row in &mut rows {
            row.state = state(&row.info.name);
        }
        self.set_tasks(rows, cannot_run, has_taskfile);
    }

    /// A Ports row was activated: open that port for the environment the
    /// panes are aimed at.
    pub fn set_on_open_port(
        &self,
        f: impl Fn(taste_core::environment::EnvironmentId, u16) + 'static,
    ) {
        *self.on_open_port.borrow_mut() = Some(Box::new(f));
    }

    /// The editor's selected tab changed: select the row that corresponds
    /// — a file's in the tree, a log's or a port's in its section — and
    /// clear the others. A tab with no row (a terminal, a chat face, a
    /// review, another environment's file) clears them all.
    pub fn select_for_editor(self: &Rc<Self>, focused: crate::editor::Focused) {
        use crate::editor::Focused;
        *self.pending_select.borrow_mut() = None;
        *self.file_in_front.borrow_mut() = match &focused {
            Focused::File(path) => Some(path.clone()),
            _ => None,
        };
        // Whatever the strip moved to, the changed-file rows answer for it:
        // the one whose document is in front is the one lit.
        self.mark_open_change_row();
        let aimed = self.aimed_environment();
        match focused {
            Focused::File(path) => {
                self.logs_list.select_row(gtk::ListBoxRow::NONE);
                self.ports_list.select_row(gtk::ListBoxRow::NONE);
                self.clear_task_selection();
                self.select_file(path);
            }
            Focused::Log(env, kind) => {
                self.clear_tree_selection();
                self.ports_list.select_row(gtk::ListBoxRow::NONE);
                self.clear_task_selection();
                let index = crate::logview::LogKind::ALL
                    .iter()
                    .position(|k| *k == kind)
                    .unwrap_or(0) as i32;
                let row = (env == aimed || !kind.per_environment())
                    .then(|| self.logs_list.row_at_index(index))
                    .flatten();
                self.logs_list.select_row(row.as_ref());
            }
            Focused::Port(env, port) => {
                self.clear_tree_selection();
                self.logs_list.select_row(gtk::ListBoxRow::NONE);
                self.clear_task_selection();
                let index = (env == aimed)
                    .then(|| self.ports.borrow().iter().position(|r| r.spec.port == port))
                    .flatten();
                let row = index.and_then(|i| self.ports_list.row_at_index(i as i32));
                self.ports_list.select_row(row.as_ref());
            }
            // A chat's document lights its step in the transcript
            // (chats.rs), and nothing here.
            // A task's tab lights its row, as a log's and a port's do
            // (David, 2026-09-23: "the same blue on selection as anything
            // else that opens in the editor").
            Focused::Task(env, name) => {
                self.clear_tree_selection();
                self.logs_list.select_row(gtk::ListBoxRow::NONE);
                self.ports_list.select_row(gtk::ListBoxRow::NONE);
                let index = (env == aimed)
                    .then(|| {
                        self.task_row_names
                            .borrow()
                            .iter()
                            .position(|row| row.as_deref() == Some(name.as_str()))
                    })
                    .flatten();
                *self.task_in_front.borrow_mut() = index.is_some().then_some(name);
                let row = index.and_then(|i| self.tasks_list.row_at_index(i as i32));
                self.tasks_list.select_row(row.as_ref());
            }
            Focused::Doc(..) | Focused::Other => {
                self.clear_tree_selection();
                self.logs_list.select_row(gtk::ListBoxRow::NONE);
                self.ports_list.select_row(gtk::ListBoxRow::NONE);
                self.clear_task_selection();
            }
        }
    }

    fn clear_task_selection(&self) {
        *self.task_in_front.borrow_mut() = None;
        self.tasks_list.select_row(gtk::ListBoxRow::NONE);
    }

    fn tree_selection(&self) -> Option<(gtk::ListView, gtk::SingleSelection, gtk::TreeListModel)> {
        let list = self.list_holder.child().and_downcast::<gtk::ListView>()?;
        let selection = list.model().and_downcast::<gtk::SingleSelection>()?;
        let model = selection.model().and_downcast::<gtk::TreeListModel>()?;
        Some((list, selection, model))
    }

    /// Put the tree's selection back on the file in front — its row if it
    /// is showing, else nothing — without scrolling or opening a folder.
    fn restore_tree_selection(&self) {
        let Some((_, selection, model)) = self.tree_selection() else {
            return;
        };
        let front = self.file_in_front.borrow().clone();
        let at = front.and_then(|path| {
            (0..model.n_items()).find(|&index| {
                model
                    .row(index)
                    .and_then(|row| row.item().and_downcast::<BoxedAnyObject>())
                    .is_some_and(|item| item.borrow::<FileNode>().path == path)
            })
        });
        selection.set_selected(at.unwrap_or(gtk::INVALID_LIST_POSITION));
    }

    /// [`Self::restore_tree_selection`], when the lit row is not the file
    /// in front already.
    fn keep_selection_on_front(&self) {
        let Some((_, selection, _)) = self.tree_selection() else {
            return;
        };
        let lit = selection
            .selected_item()
            .and_downcast::<gtk::TreeListRow>()
            .and_then(|row| row.item().and_downcast::<BoxedAnyObject>())
            .map(|item| item.borrow::<FileNode>().path.clone());
        if lit.is_some() && lit != *self.file_in_front.borrow() {
            self.restore_tree_selection();
        }
    }

    fn clear_tree_selection(&self) {
        if let Some((_, selection, _)) = self.tree_selection() {
            selection.set_selected(gtk::INVALID_LIST_POSITION);
        }
    }

    /// Select a file's row, expanding the folders above it. A folder just
    /// expanded lists its children asynchronously, so a file whose row is
    /// not there yet is remembered and retried as rows arrive.
    /// TASTE_PROBE_EXPAND: `folder` (relative to the tree's root) expanded
    /// once the tree is up, a ticker every 5ms meanwhile, and the longest
    /// gap between its ticks — how long the main thread was held — printed
    /// with the rows the folder brought.
    #[doc(hidden)]
    pub fn expand_bench_for_probe(self: &Rc<Self>, folder: PathBuf) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(3000), move || {
            let Some(tree) = weak.upgrade() else { return };
            let Some((_, _, model)) = tree.tree_selection() else {
                println!("expand bench: no tree");
                return;
            };
            let target = tree.view_root().join(&folder);
            let before = model.n_items();
            let row = (0..model.n_items())
                .filter_map(|i| model.row(i))
                .find(|row| {
                    row.item()
                        .and_downcast::<BoxedAnyObject>()
                        .is_some_and(|item| item.borrow::<FileNode>().path == target)
                });
            let Some(row) = row else {
                println!("expand bench: no row for {}", target.display());
                return;
            };
            let longest = Rc::new(Cell::new(0.0f64));
            let last = Rc::new(Cell::new(std::time::Instant::now()));
            let ticks = Rc::new(Cell::new(0u32));
            {
                let (longest, last, ticks) = (longest.clone(), last.clone(), ticks.clone());
                glib::timeout_add_local(std::time::Duration::from_millis(5), move || {
                    let now = std::time::Instant::now();
                    let gap = now.duration_since(last.get()).as_secs_f64() * 1000.0;
                    longest.set(longest.get().max(gap));
                    last.set(now);
                    ticks.set(ticks.get() + 1);
                    if ticks.get() < 1000 {
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                });
            }
            // `TASTE_PROBE_EXPAND_PENDING=<file>`: that file of the folder
            // waiting to be selected as the rows land, as a tab open on it
            // leaves it.
            if let Ok(file) = std::env::var("TASTE_PROBE_EXPAND_PENDING") {
                *tree.pending_select.borrow_mut() = Some(target.join(file));
            }
            let started = std::time::Instant::now();
            row.set_expanded(true);
            let weak = Rc::downgrade(&tree);
            glib::timeout_add_local_once(std::time::Duration::from_millis(6000), move || {
                let Some(tree) = weak.upgrade() else { return };
                let rows = tree
                    .tree_selection()
                    .map(|(_, _, m)| m.n_items())
                    .unwrap_or(0);
                println!(
                    "expand bench: {} rows came in; the main thread was held for {:.0}ms at \
                     most ({:.1}s after the expand)",
                    rows.saturating_sub(before),
                    longest.get(),
                    started.elapsed().as_secs_f64()
                );
            });
        });
    }

    /// While `held`, a file brought to the front lights its row only where
    /// it already shows: a launch reopening its tabs brings each to the
    /// front in turn, and each opened the folders above its file.
    pub fn hold_reveal(&self, held: bool) {
        self.reveal_held.set(held);
    }

    fn select_file(self: &Rc<Self>, path: PathBuf) {
        if !path.starts_with(self.view_root()) {
            self.clear_tree_selection();
            return;
        }
        let Some((list, selection, model)) = self.tree_selection() else {
            return;
        };
        if model.is_autoexpand() {
            // A search's model: every row is there already, or not at all.
            *self.pending_select.borrow_mut() = None;
        }
        let mut deepest_ancestor: Option<(gtk::TreeListRow, PathBuf)> = None;
        for index in 0..model.n_items() {
            let Some(row) = model.row(index) else {
                continue;
            };
            let Some(item) = row.item().and_downcast::<BoxedAnyObject>() else {
                continue;
            };
            let node = item.borrow::<FileNode>();
            if node.path == path {
                if selection.selected() != index {
                    selection.set_selected(index);
                    list.scroll_to(index, gtk::ListScrollFlags::NONE, None);
                }
                *self.pending_select.borrow_mut() = None;
                return;
            }
            if node.is_dir
                && path.starts_with(&node.path)
                && deepest_ancestor.as_ref().is_none_or(|(_, dir)| {
                    node.path.components().count() > dir.components().count()
                })
            {
                deepest_ancestor = Some((row.clone(), node.path.clone()));
            }
        }
        match deepest_ancestor {
            // Not shown, and not to be revealed now.
            Some(_) if self.reveal_held.get() => {
                *self.pending_select.borrow_mut() = None;
                self.clear_tree_selection();
            }
            // Open, its listing in, and what comes next on the way to the
            // file not in it: the file — or a folder above it — is not
            // listed (an ignored file not shown, a `.render/`). The reveal
            // ends here. Kept waiting, it ran again at every refresh and
            // opened that chain of folders again, after the user had
            // closed them, for as long as the tab stayed in front.
            Some((row, _))
                if row.is_expanded()
                    && row
                        .children()
                        .is_some_and(|children| children.n_items() > 0) =>
            {
                *self.pending_select.borrow_mut() = None;
                self.clear_tree_selection();
            }
            Some((row, dir)) => {
                // Its children are on their way.
                self.expanded_dirs.borrow_mut().insert(dir);
                *self.pending_select.borrow_mut() = Some(path);
                if !row.is_expanded() {
                    row.set_expanded(true);
                }
            }
            None => self.clear_tree_selection(),
        }
    }

    /// Hits inside environments — their chats and terminals — for the
    /// backlog's rows and the rule in its header (see `BacklogPanel::set_inner_hits`).
    pub fn set_inner_hits(&self, hits: HashMap<String, usize>, done: usize, total: usize) {
        self.backlog.set_inner_hits(hits, done, total);
    }

    /// The environment the tree is aimed at, as an id: the primary when
    /// it is home.
    pub fn aimed_environment(&self) -> taste_core::environment::EnvironmentId {
        self.watching()
            .unwrap_or_else(taste_core::environment::EnvironmentId::primary)
    }

    /// The Ports section's rows: the selected environment's forwarded
    /// ports and what the window's probe knows about each. Rebuilt only
    /// when something changed, which on the once-a-second tick is almost
    /// never.
    /// The aimed environment's services, as its config says them now;
    /// redrawn when they moved.
    pub fn set_services(
        self: &Rc<Self>,
        services: std::collections::BTreeMap<String, taste_devcontainer::config::ServiceSpec>,
    ) {
        if *self.services.borrow() == services {
            return;
        }
        *self.services.borrow_mut() = services;
        self.render_tasks();
        self.render_ports();
    }

    pub fn set_ports(self: &Rc<Self>, rows: Vec<PortRow>) {
        if *self.ports.borrow() == rows {
            return;
        }
        *self.ports.borrow_mut() = rows;
        self.render_ports();
    }

    /// The rows as stored, drawn — with the query's badge on a port whose
    /// name or address carries the word.
    /// The tick's: bytes through each published port, onto its row's two
    /// sparklines; kept, so a redrawn row starts from them.
    pub fn set_port_traffic(&self, traffic: &HashMap<u16, taste_devcontainer::PortTraffic>) {
        let sparklines = self.port_sparklines.borrow();
        let mut seed = self.port_traffic_seed.borrow_mut();
        for (port, traffic) in traffic {
            if let Some((ingress, egress)) = sparklines.get(port) {
                Self::show_port_traffic(ingress, egress, traffic);
            }
            seed.insert(*port, traffic.clone());
        }
    }

    fn show_port_traffic(
        ingress: &crate::sparkline::Sparkline,
        egress: &crate::sparkline::Sparkline,
        traffic: &taste_devcontainer::PortTraffic,
    ) {
        let rate = |kib_s: u32| {
            if kib_s >= 1024 {
                format!("{:.1} MiB/s", f64::from(kib_s) / 1024.0)
            } else {
                format!("{kib_s} KiB/s")
            }
        };
        let peak = |series: &[u16]| u32::from(series.iter().copied().max().unwrap_or(0));
        ingress.set_samples(&traffic.ingress);
        ingress.widget.set_tooltip_text(Some(&format!(
            "In — {} now, {} at the peak of the last five minutes; TCP and UDP alike, counted in the VM",
            rate(traffic.ingress_now_kib_s),
            rate(peak(&traffic.ingress))
        )));
        egress.set_samples(&traffic.egress);
        egress.widget.set_tooltip_text(Some(&format!(
            "Out — {} now, {} at the peak of the last five minutes; TCP and UDP alike, counted in the VM",
            rate(traffic.egress_now_kib_s),
            rate(peak(&traffic.egress))
        )));
    }

    fn render_ports(self: &Rc<Self>) {
        self.port_sparklines.borrow_mut().clear();
        while let Some(child) = self.ports_list.first_child() {
            self.ports_list.remove(&child);
        }
        let query = self.query.borrow().clone();
        let rows = self.ports.borrow();
        let mut total = 0usize;
        let any_service = rows.iter().any(|row| {
            self.services
                .borrow()
                .values()
                .any(|service| service.port == Some(row.spec.port))
        });
        for row in rows.iter() {
            // The word is the short one: this row has an address after
            // it and a narrow pane around it, and the tab's header says
            // the same state at length.
            let (dot, state) = match row.state {
                Some(state) => (state.dot(), state.word()),
                None => ("unknown", "checking"),
            };
            let hits = if query.is_empty() {
                0
            } else {
                query.ranges(&row.spec.title()).len() + query.ranges(&row.spec.url()).len()
            };
            total += hits;
            let badge = (hits > 0).then(|| crate::search::hit_badge(hits));
            // Bytes into the service and out of it, the last five minutes,
            // counted in the VM (`taste_devcontainer::ports`); the tick
            // keeps them current.
            let trailing = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            trailing.set_valign(gtk::Align::Center);
            if let Some(badge) = &badge {
                trailing.append(badge);
            }
            let ingress = crate::sparkline::Sparkline::new();
            let egress = crate::sparkline::Sparkline::new();
            for (sparkline, what) in [(&ingress, "In"), (&egress, "Out")] {
                sparkline.widget.set_can_target(true);
                sparkline
                    .widget
                    .set_tooltip_text(Some(&format!("{what} — nothing counted yet")));
                trailing.append(&sparkline.widget);
            }
            if let Some(traffic) = self.port_traffic_seed.borrow().get(&row.spec.host) {
                Self::show_port_traffic(&ingress, &egress, traffic);
            }
            // The service that serves this port, when the config pairs one
            // and the Taskfile has it: its play and stop, here as on its
            // own row (David, 2026-10-11: "offer play/stop next to ports
            // and trigger the corresponding task").
            match self.port_service_button(row.spec.port) {
                Some(button) => trailing.append(&button),
                // A row without one keeps its place, so every row's
                // sparklines stand in one column.
                None if any_service => {
                    let slot = gtk::Button::builder()
                        .icon_name("media-playback-start-symbolic")
                        .css_classes(["flat", "circular"])
                        .valign(gtk::Align::Center)
                        .opacity(0.0)
                        .can_target(false)
                        .can_focus(false)
                        .build();
                    slot.update_state(&[gtk::accessible::State::Hidden(true)]);
                    trailing.append(&slot);
                }
                None => {}
            }
            self.port_sparklines
                .borrow_mut()
                .insert(row.spec.host, (ingress, egress));
            let widget = section_row(
                Some(dot),
                None,
                &row.spec.title(),
                &format!("{state} · {}", row.spec.url()),
                Some(trailing.upcast_ref()),
            );
            // The one query filters here as it does in the backlog: a port
            // that does not carry the word hides, or dims under the ghost.
            if !query.is_empty() && hits == 0 {
                if query.ghost {
                    widget.add_css_class("search-dim");
                } else {
                    widget.set_visible(false);
                }
            }
            self.ports_list.append(&widget);
        }
        // The ghost row stays under the list whatever it holds, pushed
        // down by the rows above it.
        if let Some(search) = self.search.borrow().as_ref() {
            search.set_panel_hits(crate::search::Panel::Ports, total);
        }
        if query.is_empty() {
            self.ports_results.hide();
        } else {
            self.ports_results.show_count(&query, "ports", total, false);
        }
    }

    /// Run or stop the task that serves `port` — `None` when no service
    /// is paired with it, or the Taskfile has no such task.
    fn port_service_button(self: &Rc<Self>, port: u16) -> Option<gtk::Button> {
        let name = self
            .services
            .borrow()
            .iter()
            .find(|(_, service)| service.port == Some(port))
            .map(|(name, _)| name.clone())?;
        let state = self
            .tasks
            .borrow()
            .0
            .iter()
            .find(|row| row.info.name == name)
            .map(|row| row.state)?;
        let running = state == crate::tasks::RunState::Running;
        let runnable = self.tasks.borrow().1.is_none();
        let button = gtk::Button::builder()
            .icon_name(if running {
                "media-playback-stop-symbolic"
            } else {
                "media-playback-start-symbolic"
            })
            .tooltip_text(if running {
                format!("Stop {name}, which serves this port")
            } else {
                format!("Start {name}, which serves this port")
            })
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .sensitive(runnable || running)
            .build();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            let Some(tree) = weak.upgrade() else { return };
            let env = tree.aimed_environment();
            let hook = tree.on_run_task.borrow();
            if let Some(hook) = hook.as_ref() {
                hook(env, name.clone());
            }
        });
        Some(button)
    }

    /// What each Logs row is worth to the current query: the matches
    /// INSIDE the log, as the window last counted them, plus the matches in
    /// what the row itself says.
    ///
    /// The row's own words count. Searching "build" should find the
    /// Environment Build row (David, 2026-09-08: "'build' should match the
    /// 'Environment Build' log item"), the way "3000" finds a port by its
    /// own title rather than by anything inside it. Computed here rather
    /// than stored, so it is right for the query in force even when the
    /// window's content scan is a beat behind.
    fn log_totals(&self) -> Vec<usize> {
        let query = self.query.borrow().clone();
        let inside = self.log_hits.borrow();
        crate::logview::LogKind::ALL
            .iter()
            .enumerate()
            .map(|(index, kind)| {
                if query.is_empty() {
                    // No word, no hits — whatever the last scan left behind
                    // belonged to a query that is gone.
                    return 0;
                }
                inside.get(index).copied().unwrap_or(0)
                    + query.ranges(kind.title()).len()
                    + query.ranges(kind.subtitle()).len()
            })
            .collect()
    }

    /// The query's hits in each log (`LogKind::ALL`'s order), as badges.
    pub fn set_log_hits(&self, counts: &[usize]) {
        *self.log_hits.borrow_mut() = counts.to_vec();
        self.refresh_log_rows();
    }

    /// The Logs rows against the query in force: badges, and which rows
    /// stay. Called when the window's content scan lands (once a second)
    /// AND the moment the query changes, because a row that matches by its
    /// own title matches immediately — waiting a tick for a word that is
    /// already on screen reads as a bug, and under the probe the scan does
    /// not run at all.
    fn refresh_log_rows(&self) {
        let counts = self.log_totals();
        if let Some(search) = self.search.borrow().as_ref() {
            search.set_panel_hits(crate::search::Panel::Logs, counts.iter().sum());
        }
        let query = self.query.borrow().clone();
        if query.is_empty() {
            self.logs_results.hide();
        } else {
            self.logs_results
                .show_count(&query, "logs", counts.iter().sum(), false);
        }
        for (index, slot) in self.log_badges.iter().enumerate() {
            let count = counts.get(index).copied().unwrap_or(0);
            while let Some(child) = slot.first_child() {
                slot.remove(&child);
            }
            if count > 0 {
                slot.append(&crate::search::hit_badge(count));
            }
            // ...and the row itself filters: a log with nothing to say
            // about the word hides, or dims under the ghost.
            if let Some(row) = self.logs_list.row_at_index(index as i32) {
                let out = !query.is_empty() && count == 0;
                row.set_visible(!(out && !query.ghost));
                if out && query.ghost {
                    row.add_css_class("search-dim");
                } else {
                    row.remove_css_class("search-dim");
                }
            }
        }
    }

    /// The row the panes aim at was clicked again while it has hits inside:
    /// step through them (the backlog's own gesture, handed up).
    pub fn set_on_step_hits(&self, f: impl Fn(taste_core::environment::EnvironmentId) + 'static) {
        self.backlog.set_on_step_hits(f);
    }

    /// A click on a file with hits that is already on screen steps to the
    /// next hit; the hook says whether it did.
    pub fn set_on_step_file(&self, f: impl Fn(PathBuf) -> bool + 'static) {
        *self.on_step_file.borrow_mut() = Some(Box::new(f));
    }

    /// The Logs rows' sparklines: one set of samples per `LogKind::ALL`
    /// entry, the selected environment's. Called on the panel's tick; the
    /// sparkline itself skips a redraw when nothing changed.
    pub fn set_log_activity(
        &self,
        samples: &[[taste_core::activity::Count; taste_core::activity::BUCKETS]],
    ) {
        for (index, sparkline) in self.log_sparklines.iter().enumerate() {
            let Some(series) = samples.get(index) else {
                continue;
            };
            sparkline.set_samples(series);
            if let Some(row) = self.logs_list.row_at_index(index as i32) {
                row.set_tooltip_text(Some(&crate::sparkline::Sparkline::describe(series)));
            }
        }
    }

    /// TASTE_PROBE_CHECK only: the Logs rows with a shape each — a build
    /// that just finished, a container saying little, an IDE saying
    /// nothing — so the frame shows what the sparklines are for.
    #[doc(hidden)]
    pub fn seed_log_activity_for_probe(&self) {
        use taste_core::activity::BUCKETS;
        let mut environment = [0u16; BUCKETS];
        for (index, slot) in environment.iter_mut().enumerate() {
            *slot = match index {
                30..=44 => 40 + ((index * 7) % 23) as u16,
                45..=52 => 12,
                _ => 0,
            };
        }
        let mut container = [0u16; BUCKETS];
        for (index, slot) in container.iter_mut().enumerate() {
            *slot = if index % 9 == 0 { 3 } else { 0 };
        }
        // The VM: a boot's worth of console three minutes ago, then the
        // provisioner's few lines.
        let mut vm = [0u16; BUCKETS];
        for (index, slot) in vm.iter_mut().enumerate() {
            *slot = match index {
                20..=26 => 60 + ((index * 11) % 40) as u16,
                27..=29 => 4,
                _ => 0,
            };
        }
        self.set_log_activity(&[container, environment, vm, [0; BUCKETS]]);
    }

    /// TASTE_PROBE_CHECK only: tasks that ran, one running, ones never run,
    /// and namespaced ones two levels deep — more than the list's height,
    /// so it scrolls — posed without a container running `task`.
    #[doc(hidden)]
    pub fn seed_tasks_for_probe(self: &Rc<Self>) {
        use crate::tasks::{RunState, TaskInfo};
        // One namespace open, one folded: both of the heading's faces.
        self.tasks_open.borrow_mut().insert("dev".to_string());
        let row = |name: &str, desc: &str, state| TaskRow {
            info: TaskInfo {
                name: name.to_string(),
                desc: desc.to_string(),
            },
            state,
        };
        self.set_tasks(
            vec![
                // Folded, and every light under it: its arrow's badges.
                // First, so the list's first rows have it in the frame.
                row("ci:check", "Lint and format", RunState::Failed),
                row("ci:test", "The whole suite", RunState::Running),
                row("ci:e2e", "Against a server", RunState::Succeeded),
                row("build", "Build the workspace", RunState::Succeeded),
                row("test", "Run the test suite", RunState::Running),
                row("lint", "", RunState::Idle),
                row("dev", "Build for dev", RunState::Idle),
                row("dev:init", "Seed the dev database", RunState::Failed),
                row("dev:serve", "Serve on :3000", RunState::Idle),
                row("db:migrate:up", "Apply migrations", RunState::Idle),
            ],
            None,
            true,
        );
    }

    /// TASTE_PROBE_CHECK only: two ports, one answering, so the section
    /// has rows in every frame. `state` poses the answering one in some
    /// other state instead — `TASTE_PROBE_PORT`, which poses the tab the
    /// same way, so the row and the tab in one frame agree about the port
    /// they are both naming.
    #[doc(hidden)]
    pub fn seed_ports_for_probe(self: &Rc<Self>, state: Option<crate::portview::PortState>) {
        // Traffic through the app's port: requests in, pages out, a burst
        // two minutes ago; nothing through the database's.
        {
            use taste_core::activity::BUCKETS;
            let mut traffic = taste_devcontainer::PortTraffic::default();
            for index in 0..BUCKETS {
                let busy = (28..=40).contains(&index);
                traffic.push(
                    if busy {
                        20 + ((index * 7) % 30) as u32
                    } else {
                        (index % 6 == 0) as u32 * 4
                    },
                    if busy {
                        300 + ((index * 131) % 900) as u32
                    } else {
                        (index % 6 == 0) as u32 * 60
                    },
                );
            }
            self.port_traffic_seed.borrow_mut().insert(3000, traffic);
        }
        // The seeded `dev:serve` task serves the first port: its play
        // button there, and "service on 3000" on its own row.
        self.set_services(std::collections::BTreeMap::from([(
            "dev:serve".to_string(),
            taste_devcontainer::config::ServiceSpec { port: Some(3000) },
        )]));
        self.set_ports(vec![
            PortRow {
                spec: PortSpec {
                    port: 3000,
                    host: 3000,
                    label: Some("App".into()),
                    protocol: None,
                },
                state: Some(state.unwrap_or(crate::portview::PortState::Listening)),
            },
            PortRow {
                spec: PortSpec {
                    port: 5432,
                    host: 5432,
                    label: Some("Postgres".into()),
                    protocol: None,
                },
                state: Some(crate::portview::PortState::Nothing),
            },
        ]);
    }

    pub fn set_commit_suggester(&self, f: impl Fn(String, Box<dyn FnOnce(String)>) + 'static) {
        *self.commit_suggester.borrow_mut() = Some(Box::new(f));
    }

    /// Ask the chat agent for a commit message describing the staged diff;
    /// the reply lands in the commit entry (editable before committing).
    fn suggest_commit_message(self: &Rc<Self>, button: gtk::Button) {
        if self.commit_suggester.borrow().is_none() {
            return;
        }
        // The diff of the WORKING tree's index, which is in the VM when
        // the checkout is: the peer's index is not where staging happens.
        // Off the main thread, since for a remote checkout it is a `git`
        // through the keeper and not a libgit2 call.
        let worktree = self.worktree();
        button.set_sensitive(false);
        let weak = Rc::downgrade(self);
        let button = button.downgrade();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || worktree.staged_diff(48 * 1024).unwrap_or_default());
            let diff = handle.await.unwrap_or_default();
            let Some(tree) = weak.upgrade() else { return };
            if diff.trim().is_empty() {
                if let Some(button) = button.upgrade() {
                    button.set_sensitive(true);
                }
                tree.workspace.events.publish(Event::Toast(
                    "Stage changes first, then ask for a suggestion".into(),
                ));
                return;
            }
            let suggester = tree.commit_suggester.borrow();
            let Some(suggester) = suggester.as_ref() else {
                return;
            };
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
                    if let Some(tree) = weak.upgrade() {
                        if let Some(hook) = tree.on_suggestion.borrow().as_ref() {
                            hook(message);
                        }
                    }
                }),
            );
        });
    }

    /// Where a drafted commit message lands: the universal composer, on
    /// Commit.
    pub fn set_on_suggestion(&self, hook: impl Fn(String) + 'static) {
        *self.on_suggestion.borrow_mut() = Some(Box::new(hook));
    }

    /// The Staged view's facts, as they change: how many files are staged,
    /// and whether an unchecked one blocks a commit. Called at once with
    /// what is known now.
    pub fn set_on_commit_state(&self, hook: impl Fn(usize, bool) + 'static) {
        hook(self.staged_count.get(), self.commit_blocked.get());
        *self.on_commit_state.borrow_mut() = Some(Box::new(hook));
    }

    fn announce_commit_state(&self) {
        if let Some(hook) = self.on_commit_state.borrow().as_ref() {
            hook(self.staged_count.get(), self.commit_blocked.get());
        }
    }

    /// The backlog panel at the pane's foot, for the window to wire.
    pub fn backlog(&self) -> &Rc<crate::backlog::BacklogPanel> {
        &self.backlog
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
        let files = self.worktree().files();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let text = files.read_to_string(&file).ok()?;
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
        // reconcile the tree; the changed-files view refreshes with status.
        let query = self.query.borrow().text.clone();
        if !query.is_empty() {
            // Coalesced. A re-search is a walk of the whole checkout, and
            // these arrive in bursts — a build writing, an agent editing,
            // a branch changing under us — where every one of them would
            // ask for the same walk again (CLAUDE.md: per-change work is
            // bounded or coalesced). One walk after the writing stops.
            let generation = self.research.get() + 1;
            self.research.set(generation);
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(RESEARCH_IDLE, move || {
                let Some(tree) = weak.upgrade() else { return };
                if tree.research.get() != generation {
                    return; // another change landed; its timer owns the walk
                }
                let query = tree.query.borrow().text.clone();
                if !query.is_empty() {
                    tree.run_search(query);
                }
            });
        } else if !self.filters_active() {
            self.reconcile_tree();
        }
    }

    /// The tree after something changed on disk — a save, an agent's edit,
    /// a checkout — brought up to date **in place**. The root and every
    /// open folder are re-listed on the blocking pool and the fresh listing
    /// is spliced into the store each already backs
    /// ([`reconcile_store`]): rows that are still there are the same rows,
    /// so nothing blanks, nothing scrolls, and open folders stay open
    /// without being reopened as their children land again. (David,
    /// 2026-09-06: "The file listing flickers when there's a change" — it
    /// was rebuilt whole, which is a blank list and a cascade of splices
    /// every time anything was written.) `rebuild` remains for the changes
    /// of *shape*: construction, the ignored toggle, a search, an aim.
    fn reconcile_tree(self: &Rc<Self>) {
        let Some((_, _, model)) = self.tree_selection() else {
            self.rebuild();
            return;
        };
        if model.is_autoexpand() {
            self.rebuild();
            return;
        }
        let Ok(root_store) = model.model().downcast::<gtk::gio::ListStore>() else {
            self.rebuild();
            return;
        };
        let showing = self.showing.get();
        let synced = std::sync::Arc::new(self.sync_ignored.borrow().clone());
        let worktree = self.worktree();
        let ghosts_root = (!self.read_only()).then(|| self.view_root());
        let mut targets: Vec<(gtk::gio::ListStore, PathBuf, Option<PathBuf>)> =
            vec![(root_store, self.view_root(), ghosts_root.clone())];
        for index in 0..model.n_items() {
            let Some(row) = model.row(index) else {
                continue;
            };
            if !row.is_expanded() {
                continue;
            }
            let Some(children) = row.children().and_downcast::<gtk::gio::ListStore>() else {
                continue;
            };
            let Some(item) = row.item().and_downcast::<BoxedAnyObject>() else {
                continue;
            };
            let path = item.borrow::<FileNode>().path.clone();
            targets.push((children, path, ghosts_root.clone()));
        }
        for (store, dir, ghosts_root) in targets {
            let weak = store.downgrade();
            let worktree = worktree.clone();
            let synced = synced.clone();
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                // The ghosts' existence checks are filesystem reads too, so
                // they come along rather than running on the main thread.
                let creators = ghosts_root.is_some();
                let ghosts = ghosts_root
                    .map(|root| ghost_candidates(&worktree.files(), &root))
                    .unwrap_or_default();
                scan_dir_nodes(&worktree, &dir, showing, &synced, &ghosts, creators, None)
            });
            glib::spawn_future_local(async move {
                let Ok(nodes) = handle.await else { return };
                let Some(store) = weak.upgrade() else { return };
                reconcile_store(&store, nodes);
            });
        }
    }

    // --- find in project -------------------------------------------------

    fn run_search(self: &Rc<Self>, query: String) {
        let root = self.view_root();
        let index = self.index.borrow().clone();
        // Stop the search this one supersedes, and arm this one's flag: a
        // search reads every file, and the one a keystroke just made stale
        // should not finish reading for a result the guard below discards.
        // The spine's own flag when the query came from the box (the next
        // keystroke raises it for every source at once); a private one for
        // a scan started by something else, such as a filter change.
        let cancel = self
            .search
            .borrow()
            .as_ref()
            .map(|search| search.cancel_token())
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        if let Some(previous) = self.search_cancel.borrow_mut().replace(cancel.clone()) {
            if !std::sync::Arc::ptr_eq(&previous, &cancel) {
                previous.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let weak = Rc::downgrade(self);
        let search_query = self.query.borrow().clone();
        let search = self.search.borrow().clone();
        let (progress_tx, progress_rx) = async_channel::unbounded::<(usize, usize)>();
        {
            let search = search.clone();
            glib::spawn_future_local(async move {
                while let Ok(mut latest) = progress_rx.recv().await {
                    // The scan reports every file; the box needs the latest.
                    // Drain what has piled up since this wakeup so a
                    // thousand-file checkout is one label update per frame,
                    // not a thousand.
                    while let Ok(newer) = progress_rx.try_recv() {
                        latest = newer;
                    }
                    let (done, total) = latest;
                    if let Some(search) = search.as_ref() {
                        search.report(
                            "files",
                            crate::search::Status {
                                hits: 0,
                                done,
                                total,
                                running: true,
                            },
                        );
                    }
                }
            });
        }
        let service = self.worktree().files();
        let refs_root = self.refs_root();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let files = match index {
                    Some(files) => files,
                    None => std::sync::Arc::new(if service.is_local() {
                        taste_core::search::collect_files(&root, |_| {})
                    } else {
                        taste_core::search::collect_files_via(&service, &root)
                    }),
                };
                let total = files.len();
                let matches = if service.is_local() {
                    taste_core::search::search_files_reporting(
                        &files[..],
                        &search_query,
                        MATCH_LINES_PER_FILE,
                        &cancel,
                        &mut |done| {
                            let _ = progress_tx.try_send((done, total));
                        },
                    )?
                } else {
                    // `rg` beside the files: one answer, complete counts,
                    // the same case rule; the progress line gets its end.
                    let matches = taste_core::search::search_matches_via(
                        &service,
                        &root,
                        &search_query,
                        MATCH_LINES_PER_FILE,
                    );
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return None;
                    }
                    let _ = progress_tx.try_send((total, total));
                    matches
                };
                // File names are the first Filter surface: a path that
                // carries the word is reachable whatever its contents say.
                let by_name: Vec<PathBuf> = files
                    .iter()
                    .filter(|path| {
                        path.strip_prefix(&root)
                            .ok()
                            .and_then(|rel| rel.to_str())
                            .is_some_and(|rel| search_query.matches(rel))
                    })
                    .cloned()
                    .collect();
                // Branches, for the button's count. (Commit messages are
                // not listed anywhere now that every listing is one
                // document's — `ide_find` still searches them.)
                let branches = taste_git::GitWorkspace::discover(&refs_root)
                    .and_then(|git| git.local_branches().ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|branch| search_query.matches(branch))
                    .count();
                Some((matches, by_name, branches))
            });
            let Ok(Some((matches, by_name, branches))) = handle.await else {
                return;
            };
            let Some(tree) = weak.upgrade() else { return };
            if tree.query.borrow().text.trim() != query.trim() {
                return;
            }
            tree.branch_count.set_label(&branches.to_string());
            tree.branch_count.set_visible(branches > 0);
            let total_hits = matches.iter().map(|m| m.count).sum::<usize>() + by_name.len();
            tree.files_hits.set(total_hits);
            tree.show_files_banner(false);
            if let Some(search) = search.as_ref() {
                search.report(
                    "files",
                    crate::search::Status {
                        hits: matches.iter().map(|m| m.count).sum::<usize>() + by_name.len(),
                        done: 1,
                        total: 1,
                        running: false,
                    },
                );
            }
            *tree.files_matched.borrow_mut() = matches
                .iter()
                .map(|m| m.path.clone())
                .chain(by_name.iter().cloned())
                .collect();
            tree.report_files_hits();
            let root = tree.view_root();
            let mut grouped: HashMap<PathBuf, Vec<taste_core::search::SearchHit>> = HashMap::new();
            let mut counts: HashMap<PathBuf, usize> = HashMap::new();
            for file in matches {
                counts.insert(file.path.clone(), file.count);
                grouped.insert(file.path, file.hits);
            }
            let mut visible: HashSet<PathBuf> = HashSet::new();
            for path in grouped.keys().chain(by_name.iter()) {
                Self::make_visible(&mut visible, path, &root);
            }
            // ...and what the semantic index has already said about this
            // query, if it has (`set_meaning_hits` folds a later answer in).
            let meaning = tree.meaning_by_file();
            for path in meaning.keys() {
                Self::make_visible(&mut visible, path, &root);
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
                Self::make_visible(&mut visible, pinned, &root);
            }
            let empty = grouped.is_empty() && pinned.is_none() && meaning.is_empty();
            let fresh = SearchView {
                hits: grouped,
                counts,
                meaning,
                visible: Rc::new(visible),
                pinned: pinned.clone(),
            };
            // Nothing about the answer moved, so nothing about the list
            // should. A search re-runs on every structural change on disk
            // (`refresh_tree`), and while a query stands the model
            // autoexpands, which is the one shape `reconcile_tree` cannot
            // splice — so it rebuilt whole, and a rebuild is a blank list
            // and a cascade of splices. Under a build, or an agent
            // writing, that is a listing that will not sit still (David,
            // 2026-09-08: "the files listing keeps flickering while I have
            // the search results up"). Almost every one of those writes
            // leaves the matches exactly as they were.
            let unchanged = tree
                .search_view
                .borrow()
                .as_ref()
                .is_some_and(|current| **current == fresh);
            if unchanged {
                return;
            }
            *tree.search_view.borrow_mut() = Some(Rc::new(fresh));
            if empty && by_name.is_empty() && !tree.query.borrow().ghost {
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
        });
    }

    // --- file operations ---------------------------------------------------

    fn file_op_allowed(&self, path: &Path) -> bool {
        if self.read_only() {
            return false;
        }
        let safe_mode = !self.workspace.exec.is_container();
        taste_core::policy::write_allowed(&self.policy_root(), safe_mode, path)
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
        self.prompt_name_or(heading, affirm, initial, on_done, |_| {});
    }

    /// [`Self::prompt_name`], with what a cancel does: the dialog closed,
    /// or nothing usable typed.
    fn prompt_name_or(
        self: Rc<Self>,
        heading: &str,
        affirm: &str,
        initial: &str,
        on_done: impl Fn(&Rc<Self>, String) + 'static,
        on_cancel: impl Fn(&Rc<Self>) + 'static,
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
        // The name is what the dialog asks for: typed at once, without a
        // click into the field first (David, 2026-10-20).
        dialog.set_focus(Some(&entry));
        let tree = self.clone();
        dialog.connect_response(None, move |_, response| {
            let name = entry.text().to_string();
            let name = name.trim();
            if response == "ok"
                && !name.is_empty()
                && !name.contains('/')
                && name != "."
                && name != ".."
            {
                on_done(&tree, name.to_string());
            } else {
                on_cancel(&tree);
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
        self.confirm_destructive_as(heading, body, "Delete", on_confirm);
    }

    /// [`Self::confirm_destructive`] with the confirming button saying
    /// what it does.
    fn confirm_destructive_as(
        self: Rc<Self>,
        heading: &str,
        body: &str,
        label: &str,
        on_confirm: impl Fn(&Rc<Self>) + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("confirm", label)]);
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let tree = self.clone();
        dialog.connect_response(Some("confirm"), move |_, _| on_confirm(&tree));
        dialog.present(Some(&self.widget));
    }

    fn create_file(self: &Rc<Self>, path: &Path, is_dir: bool) {
        self.create_file_then(path, is_dir, |_, _| {});
    }

    /// [`Self::create_file`], with `done` told whether it was made — after
    /// the file exists and is open, or once it is refused or fails.
    fn create_file_then(
        self: &Rc<Self>,
        path: &Path,
        is_dir: bool,
        done: impl FnOnce(&Rc<Self>, bool) + 'static,
    ) {
        if !self.file_op_allowed(path) {
            self.op_denied_dialog();
            done(self, false);
            return;
        }
        let target = path.to_path_buf();
        let files = self.worktree().files();
        let failure = format!("Could not create {}", path.display());
        self.file_op(
            failure,
            move || {
                if is_dir {
                    return files.mkdir_all(&target);
                }
                if let Some(parent) = target.parent() {
                    files.mkdir_all(parent)?;
                }
                if files.exists(&target) {
                    Ok(())
                } else {
                    files.write(&target, b"")
                }
            },
            (!is_dir).then(|| path.to_path_buf()),
            done,
        );
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
        let from = from.to_path_buf();
        let files = self.worktree().files();
        self.file_op(
            "Rename failed".to_string(),
            move || files.rename(&from, &to),
            None,
            |_, _| {},
        );
    }

    fn delete(self: &Rc<Self>, path: &Path, is_dir: bool) {
        if !self.file_op_allowed(path) {
            self.op_denied_dialog();
            return;
        }
        let target = path.to_path_buf();
        let files = self.worktree().files();
        self.file_op(
            "Delete failed".to_string(),
            move || files.remove(&target, is_dir),
            None,
            |_, _| {},
        );
    }

    /// One filesystem mutation from the tree's menus — create, rename,
    /// delete — run off the main thread, then the tree brought current.
    ///
    /// These three used to run inline in the menu handlers, which is the
    /// GTK thread: fine for touching an empty file, and a frozen window for
    /// as long as `remove_dir_all` takes on a build directory. The policy
    /// check stays with the caller, synchronous and BEFORE anything moves;
    /// what crosses to the blocking pool is only the syscalls. `open_after`
    /// is the file to put in the editor once it exists, and `done` is told
    /// whether the operation went through, last.
    fn file_op(
        self: &Rc<Self>,
        failure: String,
        op: impl FnOnce() -> std::io::Result<()> + Send + 'static,
        open_after: Option<PathBuf>,
        done: impl FnOnce(&Rc<Self>, bool) + 'static,
    ) {
        let tree = self.clone();
        glib::spawn_future_local(async move {
            let result = match crate::runtime::runtime().spawn_blocking(op).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = result {
                tree.workspace
                    .events
                    .publish(Event::Toast(format!("{failure}: {e}")));
                done(&tree, false);
                return;
            }
            tree.workspace.events.publish(Event::GitStatusChanged);
            tree.refresh_status();
            tree.rebuild();
            if let Some(path) = open_after {
                tree.open(path, None);
            }
            done(&tree, true);
        });
    }

    /// Right-click menu on a row (native GMenu/PopoverMenu): stage/unstage
    /// when applicable, then New File / New Folder / Rename / Delete.
    fn show_context_menu(self: &Rc<Self>, anchor: &gtk::Box, node: &FileNode) {
        use gtk::gio;
        if node.create.is_some() {
            return;
        }

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
        create_section.append(Some("New file…"), Some("row.new-file"));
        create_section.append(Some("New folder…"), Some("row.new-folder"));
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

        // Ignored paths sync between the folder and Personal when asked
        // to: a line in the root `.gitattributes`
        // (`taste_git::sync_ignored`). On a folder, for the ignored files
        // under it, and on a file wherever an ignored one is listed: every
        // file, or the synced ones (the default), whose rule is taken back
        // from the row that wears it. Each checkbox is one line of the
        // IDE's — the path's, and a file's type's (`*.pdf taste-sync`) —
        // and any other line that applies, a folder's above or one written
        // by hand, is taken out by one more item rather than shown as a
        // box that cannot be unticked (David, 2026-10-20: "offer an option
        // to 'Remove other matching sync rules'").
        let remote = self.watching.borrow().is_none() && !self.workspace.files().is_local();
        let rel = self.repo_relative(&node.path);
        if let (true, false, Some(rel)) = (
            remote && (node.is_dir || self.showing.get() != Showing::Tracked),
            node.ghost,
            rel,
        ) {
            use taste_git::sync_ignored::{path_line, type_line};
            let is_dir = node.is_dir;
            let same = |a: &str, b: &str| a.split_whitespace().eq(b.split_whitespace());
            let path_rule = path_line(&rel, is_dir);
            let type_rule = (!is_dir)
                .then(|| rel.extension().and_then(|e| e.to_str()))
                .flatten()
                .and_then(|ext| Some((ext.to_string(), type_line(ext)?)));
            let (own, type_on, others) = {
                let shown = self.sync_ignored.borrow();
                let own = shown.toggled.has(&rel, is_dir);
                let type_on = type_rule
                    .as_ref()
                    .is_some_and(|(ext, _)| shown.toggled.types.contains(ext));
                let others: Vec<(PathBuf, String)> = shown
                    .lines
                    .iter()
                    .filter(|line| line.applies_to(&rel, is_dir))
                    .filter(|line| {
                        let boxed = line.file == Path::new(".gitattributes")
                            && (same(&line.line, &path_rule)
                                || type_rule.as_ref().is_some_and(|(_, t)| same(&line.line, t)));
                        !boxed
                    })
                    .map(|line| (line.file.clone(), line.line.clone()))
                    .collect();
                (own, type_on, others)
            };
            let sync_section = gio::Menu::new();
            sync_section.append(
                Some(if is_dir {
                    "Sync Ignored Files"
                } else {
                    "Force sync this file"
                }),
                Some("row.sync-ignored"),
            );
            let action = gio::SimpleAction::new_stateful("sync-ignored", None, &own.to_variant());
            action.set_enabled(writable);
            {
                let (tree, line) = (self.clone(), path_rule.clone());
                action.connect_activate(move |_, _| tree.set_sync_line(&line, !own));
            }
            actions.add_action(&action);
            if let Some((ext, line)) = type_rule {
                sync_section.append(Some(&format!("Sync *.{ext} files")), Some("row.sync-type"));
                let action =
                    gio::SimpleAction::new_stateful("sync-type", None, &type_on.to_variant());
                action.set_enabled(writable);
                let tree = self.clone();
                action.connect_activate(move |_, _| tree.set_sync_line(&line, !type_on));
                actions.add_action(&action);
            }
            if !others.is_empty() {
                sync_section.append(
                    Some("Remove other matching sync rules"),
                    Some("row.sync-others"),
                );
                let tree = self.clone();
                add_action(
                    "sync-others",
                    Box::new(move || tree.remove_sync_lines(others.clone())),
                );
            }
            menu.append_section(None, &sync_section);
        }

        // The folder's own menu — its row in the header — opens another
        // window, which asks for its folder (`main::open_new_window`,
        // through the window's action; David, 2026-10-20).
        if node.is_dir && node.path == self.view_root() {
            let window_section = gio::Menu::new();
            window_section.append(Some("Open folder in new window…"), Some("win.open-folder"));
            menu.append_section(None, &window_section);
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

    /// One sync rule written or taken out — a path's line, or a type's —
    /// in the root `.gitattributes`, beside the files, off the main thread,
    /// then the tree brought current. The sync carries the files from there.
    fn set_sync_line(self: &Rc<Self>, line: &str, on: bool) {
        let worktree = self.worktree();
        let line = line.to_string();
        self.file_op(
            if on {
                "Syncing ignored files failed".to_string()
            } else {
                "Stopping the sync of ignored files failed".to_string()
            },
            move || {
                worktree
                    .set_sync_line(&line, on)
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))
            },
            None,
            |_, _| {},
        );
    }

    /// The sync rules a row matches beyond its own checkboxes, taken out of
    /// the attributes files they are in.
    fn remove_sync_lines(self: &Rc<Self>, lines: Vec<(PathBuf, String)>) {
        let worktree = self.worktree();
        self.file_op(
            "Removing the other sync rules failed".to_string(),
            move || {
                worktree
                    .remove_sync_lines(&lines)
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))
            },
            None,
            |_, _| {},
        );
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
        if self.unreachable.get() {
            if !self.unreachable_retry_armed.replace(true) {
                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(UNREACHABLE_RETRY, move || {
                    let Some(tree) = weak.upgrade() else { return };
                    tree.unreachable_retry_armed.set(false);
                    tree.unreachable.set(false);
                    tree.refresh_status();
                });
            }
            return;
        }
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
        let worktree = self.worktree();
        let refs_root = self.refs_root();
        // The answer belongs to the checkout it was asked about, and an
        // older answer must not be applied to a newer aim.
        let root_for_apply = root.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                // The working tree answers for itself, wherever it is; the
                // relation to the upstream is the peer's, on this host,
                // which holds the remote-tracking refs the user fetched.
                let status = match worktree.status() {
                    Ok(status) => status,
                    // git itself said no, beside the files: the VM is
                    // answering, so this is not `Unreachable`.
                    Err(e)
                        if e.downcast_ref::<taste_devcontainer::worktree::GitFailed>()
                            .is_some() =>
                    {
                        return Err(StatusFail::Git(format!("{e}")));
                    }
                    Err(e) => {
                        // A checkout on this host that is not a repository
                        // says so quietly; one in a VM that cannot be read
                        // is a files service that is down — the VM
                        // stopped, or its keeper gone — which is a
                        // different fact, told differently
                        // (`apply_unreachable`), and worth a line in the
                        // terminal.
                        if !worktree.is_local() {
                            // Once a minute, not once a query: the same
                            // sentence three times a second is noise over
                            // the line that would say when it changed.
                            static LAST_WARNED: std::sync::Mutex<Option<std::time::Instant>> =
                                std::sync::Mutex::new(None);
                            let mut last = LAST_WARNED.lock().unwrap();
                            if last
                                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(60))
                            {
                                *last = Some(std::time::Instant::now());
                                tracing::warn!(
                                    "file tree: status of {} failed: {e:#}",
                                    root.display()
                                );
                            }
                            return Err(StatusFail::Unreachable);
                        }
                        return Err(StatusFail::NotRepo);
                    }
                };
                // The branch the WORKING TREE is on, asked of the repository
                // that holds the remote-tracking refs: for a checkout in a
                // VM that is the peer, whose own HEAD is whatever the
                // folder last fast-forwarded to — asking about HEAD there
                // read a branch just made in the VM as main against
                // origin/main.
                let branch = worktree.branch_name();
                let folder = GitWorkspace::discover(if worktree.is_local() {
                    &root
                } else {
                    &refs_root
                });
                let sync = folder.as_ref().and_then(|git| match &branch {
                    Some(branch) => git.sync_status_of(branch).ok(),
                    None => git.sync_status().ok(),
                });
                let merged = crate::merged_bar::posed_for_probe().or_else(|| {
                    let branch = branch.as_deref()?;
                    folder.as_ref()?.merged_into_default(branch)
                });
                let submodule_sync = match (folder.as_ref(), branch.as_deref()) {
                    (Some(git), Some(branch)) => SubmoduleSync::read(git, branch),
                    _ => SubmoduleSync::default(),
                };
                Ok(StatusSnapshot {
                    submodule_sync,
                    merged,
                    submodules: worktree.submodule_heads(),
                    submodule_setup: crate::submodule_bar::posed_for_probe().unwrap_or_else(|| {
                        folder
                            .as_ref()
                            .map(|git| git.submodules_to_set_up())
                            .unwrap_or_default()
                    }),
                    sync_ignored: synced_for_probe()
                        .unwrap_or_else(|| worktree.sync_ignored(&status)),
                    status,
                    stashed: worktree.stashed_paths().unwrap_or_default(),
                    ignore_rules: worktree
                        .files()
                        .read_to_string(&root.join(".gitignore"))
                        .map(|text| {
                            text.lines()
                                .filter(|l| {
                                    let l = l.trim();
                                    !l.is_empty() && !l.starts_with('#')
                                })
                                .count()
                        })
                        .unwrap_or(0),
                    branch,
                    sync,
                    rebasing: worktree.rebase_in_progress(),
                })
            });
            let snapshot = handle.await;
            let Some(tree) = weak.upgrade() else { return };
            match snapshot {
                Ok(Ok(snapshot)) => tree.apply_status(&root_for_apply, Some(snapshot)),
                Ok(Err(StatusFail::NotRepo)) => tree.apply_status(&root_for_apply, None),
                Ok(Err(StatusFail::Unreachable)) => tree.apply_unreachable(&root_for_apply),
                Ok(Err(StatusFail::Git(said))) => tree.apply_git_refused(&root_for_apply, &said),
                Err(_) => {}
            }
            // Every exit from a query has to release the gate, or the tree
            // stops refreshing for the life of the window.
            if tree.refresh.finish() {
                tree.refresh_status();
            }
        });
    }

    /// The checkout is in a VM that is not answering — stopped, or its
    /// files service gone. Not "not a git repository": the repository is
    /// there and so are its rows, which stay as last seen; what goes is
    /// the branch and sync tools, and one insensitive line in their place
    /// says why (David, 2026-09-21: "I shouldn't see 'initialize repo'
    /// under these circumstances. I stopped the VM"). The next status
    /// that succeeds puts everything back.
    fn apply_unreachable(self: &Rc<Self>, root: &Path) {
        self.apply_blocked(root, "Checkout unreachable — its VM is not answering");
    }

    /// git, beside a checkout in a VM, refused the status: the VM is
    /// answering, so it is not unreachable. A refusal is often a moment's
    /// — a container's hook cloning a submodule writes paths git outside
    /// the container cannot follow, and takes them away again when it is
    /// done (2026-10-05: "This VM error shouldn't show during startup") —
    /// so the rows stay as last seen and nothing is said; only a refusal
    /// that outlasts [`GIT_REFUSAL_GRACE`] takes the header, in git's own
    /// words.
    fn apply_git_refused(self: &Rc<Self>, root: &Path, said: &str) {
        if self.view_root() != root {
            return;
        }
        let since = *self
            .git_refused_since
            .borrow_mut()
            .get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= GIT_REFUSAL_GRACE {
            self.apply_blocked(root, &format!("git: {}", single_line_note(said)));
        }
    }

    /// The branch and sync tools give way to one insensitive line saying
    /// why; the rows stay as last seen.
    fn apply_blocked(self: &Rc<Self>, root: &Path, why: &str) {
        if self.view_root() != root {
            return;
        }
        self.unreachable.set(true);
        self.init_button.set_label(why);
        self.init_button.set_sensitive(false);
        self.init_button.set_visible(true);
        self.branch_label.set_visible(false);
        self.canon_button.set_visible(false);
        self.pull_button.set_visible(false);
        self.push_button.set_visible(false);
        self.sync_button.set_visible(false);
        for toggle in [
            &self.stashed_toggle,
            &self.dirty_toggle,
            &self.staged_toggle,
            &self.conflicts_toggle,
        ] {
            toggle.set_sensitive(false);
        }
    }

    fn apply_status(self: &Rc<Self>, root: &Path, snapshot: Option<StatusSnapshot>) {
        self.unreachable.set(false);
        self.git_refused_since.borrow_mut().take();
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
                // A submodule that moved rewrites its row's name, which no
                // file state says: a full rebind, as rarely as that is.
                let heads_changed = *self.submodule_heads.borrow() != snapshot.submodules;
                // Unchanged status must not churn row widgets: factory
                // resets during agent/build activity are what made hovering
                // feel laggy. ignore_rules participates: a new rule changes
                // no file state, but the Staged pane restore rides on this
                // refresh actually rendering.
                unchanged = !mode_changed
                    && !heads_changed
                    && *self.status.borrow() == snapshot.status
                    && *self.stashed.borrow() == snapshot.stashed
                    && *self.sync_ignored.borrow() == snapshot.sync_ignored
                    && self.ignore_rules.get() == snapshot.ignore_rules;
                states_only = !mode_changed
                    && !heads_changed
                    && self.ignore_rules.get() == snapshot.ignore_rules;
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
                // What syncs is what the tree lists of what is ignored: a
                // change to it is a change of rows, so they are listed again.
                let synced_moved = *self.sync_ignored.borrow() != snapshot.sync_ignored;
                *self.sync_ignored.borrow_mut() = snapshot.sync_ignored;
                if synced_moved {
                    let tree = self.clone();
                    glib::idle_add_local_once(move || tree.refresh_tree());
                }
                *self.submodule_heads.borrow_mut() = snapshot.submodules;
                self.submodule_bar
                    .show(&snapshot.submodule_setup, self.submodule_busy.get());
                *self.submodule_setup.borrow_mut() = snapshot.submodule_setup;
                self.merged_bar.show(snapshot.merged.as_ref());
                self.ignore_rules.set(snapshot.ignore_rules);
                self.sync_filter_counts();
                *self.status_branch.borrow_mut() = snapshot.branch.clone();
                self.set_branch_label(&snapshot.branch.unwrap_or_else(|| "(no branch)".into()));
                self.abort_button.set_visible(snapshot.rebasing);
                self.continue_button.set_visible(snapshot.rebasing);
                // Mid-operation: this snapshot predates the result, so
                // applying it would paint the pre-operation counts over
                // the spinner, then correct them a moment later.
                if !self.sync_busy.get() {
                    for button in [
                        &self.sync_button,
                        &self.push_button,
                        &self.pull_button,
                        &self.canon_button,
                    ] {
                        button_idle(button);
                    }
                    self.sync_button.set_sensitive(!snapshot.rebasing);
                    if snapshot.rebasing {
                        // A pull or a push of a branch halfway through a
                        // rebase is neither: Continue or Abort first.
                        self.push_button.set_sensitive(false);
                        self.pull_button.set_sensitive(false);
                        self.canon_button.set_sensitive(false);
                        self.set_sync_label("rebase paused — resolve, mark, Continue");
                    } else {
                        match snapshot.sync {
                            Some(sync) => match sync.upstream {
                                Some(ref upstream) => {
                                    // The upstream name lives in the button
                                    // tooltips; the label is for exceptions.
                                    // A branch rewritten here is not one:
                                    // its ↓ is 0, the commits it lacks being
                                    // its own old copies, and Push's
                                    // tooltip and click say the hold (a note
                                    // here was cut to "rewritt…" in a
                                    // 335px pane).
                                    self.set_sync_label("");
                                    let hold = crate::holdbutton::HOLD.as_secs();
                                    // A fork's rebased branch, or a branch
                                    // rewritten here: what the push replaces
                                    // there is the point of it.
                                    let replacing =
                                        replacing_push(&sync).map(|(_, replaced)| replaced);
                                    // ↑: the branch's own commits — not the
                                    // canonical branch's, which a rebase onto
                                    // it brings along — in the warning colour
                                    // when the push replaces what the remote
                                    // has, which is a hold.
                                    let own = sync.own_ahead;
                                    self.push_face.set_label(&if sync.push_unknown {
                                        " ?".to_string()
                                    } else {
                                        format!(" {own}")
                                    });
                                    if replacing.is_some() {
                                        self.push_face.add_css_class(WARNED);
                                    } else {
                                        self.push_face.remove_css_class(WARNED);
                                    }
                                    // A branch the remote does not have yet
                                    // is worth a push with nothing ahead:
                                    // the push is what creates it there.
                                    self.push_button.set_sensitive(
                                        sync.ahead > 0
                                            || sync.new_branch
                                            || replacing.is_some()
                                            || sync.push_unknown,
                                    );
                                    let push_hold =
                                        push_hold_for(sync.on_canonical, replacing.is_some());
                                    self.push_hold.set_hold(push_hold);
                                    let push_secs = push_hold.as_secs();
                                    let ahead = sync.ahead;
                                    let carried = ahead.saturating_sub(own);
                                    let sends = match &sync.canonical {
                                        Some(canonical) if carried > 0 => format!(
                                            "{}, and {} from {} under {}",
                                            count(own, "commit"),
                                            carried,
                                            canonical.name,
                                            if own == 1 { "it" } else { "them" }
                                        ),
                                        _ => count(own, "commit"),
                                    };
                                    let default_branch = if sync.on_canonical {
                                        " (the default branch)"
                                    } else {
                                        ""
                                    };
                                    let leased = format!(
                                        "Hold {push_secs} s · refused if the remote moved since \
                                         the last fetch"
                                    );
                                    *self.push_card.borrow_mut() =
                                        Some(match replacing {
                                            Some(replaced) => {
                                                let card = SyncCard::new(format!(
                                                    "Replace {upstream}{default_branch}"
                                                ))
                                                .fact("Sends", sends)
                                                .fact("Replaces", count(replaced, "commit"));
                                                match sync.rewritten {
                                                    Some(0) => card.fact(
                                                        "Their changes",
                                                        "all in this branch; nothing is lost",
                                                    ),
                                                    Some(dropped) => card.warning(format!(
                                                    "{} there {} changes this branch no longer \
                                                     has; the push drops {}",
                                                    count(dropped, "commit"),
                                                    if dropped == 1 { "makes" } else { "make" },
                                                    if dropped == 1 { "it" } else { "them" }
                                                )),
                                                    // A fork's branch rebased onto
                                                    // its base, as the IDE has no
                                                    // way to check: what is there
                                                    // matched nothing here, so it
                                                    // may be anyone's (review,
                                                    // 2026-10-20).
                                                    None => card.warning(format!(
                                                        "{} there {} no match in this \
                                                         branch: from before a rebase onto \
                                                         the base, or someone else's",
                                                        count(replaced, "commit"),
                                                        if replaced == 1 { "has" } else { "have" },
                                                    )),
                                                }
                                                .gesture(leased)
                                            }
                                            None if sync.push_unknown => {
                                                SyncCard::new(format!("Push to {upstream}"))
                                                    .fact(
                                                        "Sends",
                                                        format!("up to {}", count(ahead, "commit")),
                                                    )
                                                    .fact("Remote tip", "not fetched yet")
                                                    .gesture("Click to push")
                                            }
                                            None if sync.new_branch => {
                                                SyncCard::new(format!("Create {upstream}"))
                                                    .fact("Sends", sends)
                                                    .gesture("Click to push")
                                            }
                                            None => SyncCard::new(format!(
                                                "Push to {upstream}{default_branch}"
                                            ))
                                            .fact("Sends", sends)
                                            .gesture(if sync.on_canonical {
                                                format!("Hold {push_secs} s")
                                            } else {
                                                "Click to push".to_string()
                                            }),
                                        });
                                    if let Some(card) = self.push_card.borrow_mut().as_mut() {
                                        // What ↑ counts, listed under the
                                        // facts.
                                        card.commits = sync.sends.clone();
                                        if ahead > 0 {
                                            snapshot.submodule_sync.tell_push(card);
                                        }
                                    }
                                    // With the background fetch held back
                                    // for a key's touch the counts are as
                                    // old as the last Pull, so both pulls
                                    // stay pressable: they are what fetches.
                                    let stale = self.fetch_hold.borrow().is_some();
                                    // A view of another environment's
                                    // checkout pulls nothing into it, as it
                                    // pushes nothing from it.
                                    let writable = !self.read_only();
                                    let held = self.fetch_hold.borrow().as_ref().map(|reason| {
                                        format!(
                                            "Not fetched in the background: {reason}. These \
                                             counts are from the last fetch"
                                        )
                                    });
                                    // ↓: the branch's own copy on the remote,
                                    // which someone else may have pushed to.
                                    let behind = sync.own_behind;
                                    self.pull_face.set_label(&format!(" {behind}"));
                                    // Amber, as a push that replaces is,
                                    // when the hold would rebase — replay
                                    // the branch's own commits as new ones —
                                    // rather than fast-forward (David,
                                    // 2026-10-19: "we should probably have
                                    // gold for a pull that isn't a fast
                                    // forward").
                                    let rebases = behind > 0 && sync.ahead > 0;
                                    if rebases {
                                        self.pull_face.add_css_class(WARNED);
                                    } else {
                                        self.pull_face.remove_css_class(WARNED);
                                    }
                                    self.pull_button.set_sensitive(
                                        (behind > 0 || stale)
                                            && !sync.new_branch
                                            && !sync.push_unknown
                                            && !sync.push_by_url
                                            && writable,
                                    );
                                    let replaced = sync.replaces.as_ref().map_or(0, |(_, n)| *n);
                                    *self.pull_card.borrow_mut() = Some(
                                        if sync.rewritten.is_some() {
                                            SyncCard::new(format!(
                                                "Nothing to pull from {upstream}"
                                            ))
                                            .fact(
                                                "There",
                                                format!(
                                                    "{}: this branch's own, before it was \
                                                         rebased",
                                                    count(replaced, "commit")
                                                ),
                                            )
                                            .gesture("Hold ↑ to replace them")
                                        } else if sync.new_branch {
                                            SyncCard::new(format!(
                                                "Nothing to pull from {upstream}"
                                            ))
                                            .fact("There", "not created yet")
                                        } else if sync.push_by_url {
                                            // No ref here keeps a URL's tip,
                                            // so there is nothing to move the
                                            // branch onto (review, 2026-10-20).
                                            SyncCard::new(format!("Can't pull from {upstream}"))
                                                .fact("New there", count(behind, "commit"))
                                                .commits(sync.incoming.clone())
                                                .fact(
                                                    "Why",
                                                    "it is a URL, not a remote; add it as a \
                                                     remote to pull from it",
                                                )
                                        } else {
                                            let mut card =
                                                SyncCard::new(format!("Catch up with {upstream}"))
                                                    .fact("New there", count(behind, "commit"))
                                                    .commits(sync.incoming.clone());
                                            SubmoduleSync::tell_moves(
                                                &snapshot.submodule_sync.own_moves,
                                                &mut card,
                                            );
                                            card.gesture(format!(
                                                "Click: fast-forward · Hold {hold} s: rebase"
                                            ))
                                        }
                                        .warning_if(held.clone()),
                                    );
                                    // ⇊: the branch a pull request's branch
                                    // is rebased onto. Not on that branch
                                    // itself, whose own copy is ↓.
                                    match &sync.canonical {
                                        Some(canonical) => {
                                            let (name, new, mine) =
                                                (&canonical.name, canonical.new, canonical.own);
                                            self.canon_button.set_visible(true);
                                            self.canon_face.set_label(&format!(" {new}"));
                                            if new > 0 && mine > 0 {
                                                self.canon_face.add_css_class(WARNED);
                                            } else {
                                                self.canon_face.remove_css_class(WARNED);
                                            }
                                            self.canon_button
                                                .set_sensitive((new > 0 || stale) && writable);
                                            let canon_moves = &snapshot.submodule_sync.canon_moves;
                                            *self.canon_card.borrow_mut() = Some(
                                                if new == 0 {
                                                    SyncCard::new(format!("Up to date with {name}"))
                                                        .fact("Yours", count(mine, "commit"))
                                                } else if mine == 0 {
                                                    let mut card = SyncCard::new(format!(
                                                        "Fast-forward to {name}"
                                                    ))
                                                    .fact("New there", count(new, "change"))
                                                    .commits(canonical.incoming.clone());
                                                    SubmoduleSync::tell_moves(
                                                        canon_moves,
                                                        &mut card,
                                                    );
                                                    card.gesture("Click to fast-forward")
                                                } else {
                                                    let mut card = SyncCard::new(format!(
                                                        "Rebase onto {name}"
                                                    ))
                                                    .fact("New there", count(new, "change"))
                                                    .fact("Yours", count(mine, "commit"))
                                                    .commits(canonical.incoming.clone());
                                                    SubmoduleSync::tell_moves(
                                                        canon_moves,
                                                        &mut card,
                                                    );
                                                    card.gesture(format!(
                                                        "Hold {hold} s to rebase · a click \
                                                         only fast-forwards"
                                                    ))
                                                }
                                                .warning_if(held.clone()),
                                            );
                                        }
                                        None => self.canon_button.set_visible(false),
                                    }
                                    self.probe_card();
                                }
                                None => {
                                    self.set_sync_label("no upstream");
                                    self.clear_sync_counts();
                                }
                            },
                            None => {
                                self.set_sync_label("");
                                self.clear_sync_counts();
                            }
                        }
                    }
                }
            }
            None => {
                // The handle is the other half of this answer, and the two
                // may not disagree. A cached `GitWorkspace` for a checkout
                // that has gone still hands the branch menu a branch —
                // which is how a header read `not a git repository` over a
                // dropdown offering `main` (David, 2026-09-17) — and it
                // still backs commit, push, and `repo_relative`, which is
                // the worse half: wrong actions outlive a wrong label.
                //
                // Dropped only when the pane is still aimed where this
                // query looked, for the reason `aim_at` gives when it
                // clears the handle itself: an older answer must never
                // clobber a newer aim.
                if self.view_root() == root {
                    self.git.borrow_mut().take();
                }
                // Nothing to diff between non-repo refreshes: after the
                // first render, only a mode flip warrants row churn.
                unchanged = !mode_changed && self.rendered_non_repo.replace(true);
                // Through `branch_child`, like the repo arm, and NEVER
                // `branch_label.set_label` — `branch_label` is a
                // `MenuButton` built with a custom child, and setting a
                // MenuButton's label REPLACES that child with GTK's own.
                // After one non-repo render the child was detached, so
                // every later `set_branch_label` wrote into an orphan and
                // the button read `not a git repository` forever, over a
                // popover correctly offering `main` (David, 2026-09-17:
                // "Seems like a bug"). The dropdown arrow in that frame is
                // the tell: the custom child is built without one, so an
                // arrow means GTK is drawing its own label.
                self.set_branch_label("not a git repository");
                self.init_button.set_label("Initialize Repository");
                self.init_button.set_size_request(-1, -1);
                self.init_button.set_sensitive(true);
            }
        }
        // Not a repo: the branch dropdown and sync tools mean nothing;
        // one Initialize button takes their place until init succeeds.
        self.init_button.set_visible(!is_repo);
        self.branch_label.set_visible(is_repo);
        self.pull_button.set_visible(is_repo);
        // Shown by the status that says there is a canonical branch.
        if !is_repo {
            self.canon_button.set_visible(false);
        }
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
        // environment is read-only but for its Pull: this has the last word.
        self.apply_view_permissions();
        // Views refresh only now, with the fresh map in place — and only
        // if something actually changed.
        if unchanged {
            return;
        }
        if self.filters_active() && self.query.borrow().is_empty() {
            self.render_filter_view();
        } else if states_only {
            self.restyle_changed_rows();
        } else {
            self.rebuild_rows_in_place();
        }
    }

    /// Commit-building view: only changed files, each with an explicit
    /// stage checkbox — selection you can see.
    /// Refresh the counts carried by the filter buttons.
    fn sync_filter_counts(&self) {
        let status = self.status.borrow();
        let dirty = status.values().filter(|s| s.stageable()).count();
        let staged = status.values().filter(|s| **s == FileState::Staged).count();
        let conflicts = status
            .values()
            .filter(|s| **s == FileState::Conflicted)
            .count();
        drop(status);
        let stashed = self.stashed.borrow().len();
        self.dirty_face.set_label(&dirty.to_string());
        self.staged_face.set_label(&staged.to_string());
        if self.staged_count.replace(staged) != staged {
            self.announce_commit_state();
        }
        if staged > 0 {
            self.staged_toggle.add_css_class("accent");
        } else {
            self.staged_toggle.remove_css_class("accent");
        }
        self.stashed_face.set_label(&stashed.to_string());
        self.conflicts_face.set_label(&conflicts.to_string());
        // Present only while it means something — but never yanked out
        // from under its own active view.
        self.conflicts_toggle
            .set_visible(conflicts > 0 || self.conflicts_toggle.is_active());
        match self.index_files() {
            Some(files) => {
                self.all_face.set_label(&files.len().to_string());
                // What the number counts, which is not what the tree lists
                // once it shows ignored files too (David, 2026-10-20:
                // "That's false if I'm showing ignored files").
                self.all_toggle.set_tooltip_text(Some(&format!(
                    "No git filter: every file the tree lists. {} {} not ignored by git",
                    files.len(),
                    if files.len() == 1 {
                        "file is"
                    } else {
                        "files are"
                    }
                )));
            }
            None => {
                self.all_face.set_label("");
                self.all_toggle
                    .set_tooltip_text(Some("No git filter: every file the tree lists"));
            }
        }
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
    /// Light the changed-file row whose document is the tab in front.
    ///
    /// The one open wears the open-item blue, the same one a selected
    /// section row and the front editor tab wear (David, 2026-09-08:
    /// "this should be blue when selected"). These rows are
    /// `SelectionMode::None` — a click opens the diff rather than
    /// selecting — so being open IS the selection here.
    ///
    /// Called from the list's render AND from `select_for_editor`, because
    /// the two facts change at different times: the list is rebuilt by a
    /// git tick, and which tab is in front by a click in the strip.
    /// Marking only on the render left a row grey until the next status
    /// refresh happened to come along (David, 2026-09-08: "this should
    /// have a blue highlight if selected and showing in the editor panel
    /// area").
    fn mark_open_change_row(&self) {
        let workdir = self.worktree_root();
        let active = self
            .workspace
            .ide
            .open_files()
            .into_iter()
            .find(|f| f.active)
            .map(|f| f.path);
        for (rel, entry) in self.changed_rows.borrow().iter() {
            let open = active.as_ref().is_some_and(|path| {
                let abs = workdir
                    .as_ref()
                    .map(|w| w.join(rel))
                    .unwrap_or_else(|| rel.clone());
                *path == abs
            });
            if open {
                entry.row.add_css_class("doc-open");
            } else {
                entry.row.remove_css_class("doc-open");
            }
        }
    }

    fn render_changed_list(self: &Rc<Self>) {
        let flags = ChangedFlags {
            dirty_on: self.dirty_toggle.is_active(),
            staged_on: self.staged_toggle.is_active(),
            stashed_on: self.stashed_toggle.is_active(),
            conflicts_on: self.conflicts_toggle.is_active(),
        };
        let ChangedFlags {
            dirty_on,
            staged_on,
            stashed_on,
            conflicts_on,
        } = flags;
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
        if entries.is_empty() {
            self.changed_rows.borrow_mut().clear();
            *self.changed_list.borrow_mut() = None;
            self.selection.borrow_mut().clear();
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
        let workdir = self.worktree_root();
        // The list is KEPT and reconciled, not rebuilt: rows whose path and
        // state are unchanged are the same widgets, checkboxes and all, so
        // a status tick under an agent's edits changes the one row that
        // moved and nothing blanks or scrolls (David, 2026-09-08: "I'm
        // still getting flicker on the file tree when changes happen" —
        // this list was built afresh on every tick). A change of view
        // (Dirty to Staged) is a change of shape and starts over.
        let showing = self.list_holder.child();
        let reuse = self
            .changed_list
            .borrow()
            .as_ref()
            .filter(|(list, had)| {
                *had == flags && showing.as_ref() == Some(list.upcast_ref::<gtk::Widget>())
            })
            .map(|(list, _)| list.clone());
        let list = match reuse {
            Some(list) => list,
            None => {
                self.changed_rows.borrow_mut().clear();
                self.selection.borrow_mut().clear();
                let list = gtk::ListBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    // `.change-list`: the stylesheet draws these rows'
                    // checkboxes a touch smaller than stock (main.rs).
                    // `navigation-sidebar`: the inset, rounded row every
                    // other listing in this pane wears (David, 2026-09-08:
                    // "these checklist-style listings should also use the
                    // same margins and rounded corners").
                    .css_classes(["change-list", "navigation-sidebar"])
                    .build();
                *self.changed_list.borrow_mut() = Some((list.clone(), flags));
                list
            }
        };
        self.syncing_selection.set(true);
        let wanted: HashMap<PathBuf, (FileState, bool)> = entries
            .iter()
            .map(|(path, state, in_stash)| (path.clone(), (*state, *in_stash)))
            .collect();
        // Rows that vanished go, and drop out of the selection; rows whose
        // state moved are rebuilt below with their checkbox as it was.
        let mut carried: HashMap<PathBuf, bool> = HashMap::new();
        {
            let mut rows = self.changed_rows.borrow_mut();
            let mut selection = self.selection.borrow_mut();
            rows.retain(|path, handle| {
                let keep = wanted.get(path) == Some(&(handle.state, handle.in_stash));
                if !keep {
                    list.remove(&handle.row);
                    if wanted.contains_key(path) {
                        carried.insert(path.clone(), handle.check.is_active());
                    } else {
                        let abs = workdir
                            .as_ref()
                            .map(|w| w.join(path))
                            .unwrap_or_else(|| path.clone());
                        selection.remove(&abs);
                    }
                }
                keep
            });
        }
        for (index, (rel, state, in_stash)) in entries.iter().enumerate() {
            if self.changed_rows.borrow().contains_key(rel) {
                continue;
            }
            let (row, check, mark) =
                self.changed_row(rel, *state, *in_stash, flags, workdir.as_deref());
            mark.show(self.moving.borrow().get(rel), None);
            if let Some(active) = carried.get(rel) {
                check.set_active(*active);
            }
            list.insert(&row, index as i32);
            self.changed_rows.borrow_mut().insert(
                rel.clone(),
                ChangedRow {
                    row,
                    check,
                    state: *state,
                    in_stash: *in_stash,
                    mark,
                },
            );
        }
        self.mark_open_change_row();
        self.syncing_selection.set(false);
        if showing.as_ref() != Some(list.upcast_ref::<gtk::Widget>()) {
            self.list_holder.set_child(Some(&list));
        }
        // The Staged view carries its ops-and-commit pane at the bottom of
        // the list; elsewhere a selection-derived pane is now stale. Ad-hoc
        // flows survive.
        if staged_on {
            self.selection_intervention();
        } else if matches!(self.pane.get(), PaneKind::Selection | PaneKind::Staged) {
            self.close_intervention();
        }
    }

    /// Set every checkbox between `from` and `to` (both rows included, in
    /// the list's order) to `active`, as one change: the pane is rebuilt
    /// once, by the caller.
    fn check_range(&self, from: &Path, to: &Path, active: bool) {
        let rows = self.changed_rows.borrow();
        let (Some(a), Some(b)) = (rows.get(from), rows.get(to)) else {
            return;
        };
        let (a, b) = (a.row.index(), b.row.index());
        let (low, high) = (a.min(b), a.max(b));
        self.syncing_selection.set(true);
        for handle in rows.values() {
            let at = handle.row.index();
            if at >= low && at <= high && handle.row.is_visible() {
                handle.check.set_active(active);
            }
        }
        self.syncing_selection.set(false);
    }

    /// One row of the changed-files list: the checkbox, the name and path,
    /// the state badge, discard, and the row's menu.
    fn changed_row(
        self: &Rc<Self>,
        rel: &Path,
        state: FileState,
        in_stash: bool,
        flags: ChangedFlags,
        workdir: Option<&Path>,
    ) -> (adw::ActionRow, gtk::CheckButton, crate::sync_mark::SyncMark) {
        let ChangedFlags {
            staged_on,
            conflicts_on,
            ..
        } = flags;
        let rel = rel.to_path_buf();
        let workdir = workdir.map(Path::to_path_buf);
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
            let row_rel = rel.clone();
            check.connect_toggled(move |check| {
                let Some(tree) = weak.upgrade() else { return };
                if check.is_active() {
                    tree.selection.borrow_mut().insert(abs.clone());
                } else {
                    tree.selection.borrow_mut().remove(&abs);
                }
                if tree.syncing_selection.get() {
                    return;
                }
                // Shift-click: every row from the anchor to this one takes
                // this one's new state, as a list's range select does
                // (David, 2026-09-28: "I should be able to shift-click the
                // checkboxes").
                if tree.shift_click.replace(false) {
                    let anchor = tree.check_anchor.borrow().clone();
                    if let Some(anchor) = anchor.filter(|a| *a != row_rel) {
                        tree.check_range(&anchor, &row_rel, check.is_active());
                    }
                } else {
                    *tree.check_anchor.borrow_mut() = Some(row_rel.clone());
                }
                tree.selection_intervention();
            });
            // The press comes before the toggle, and knows the modifiers.
            let weak = Rc::downgrade(self);
            let press = gtk::GestureClick::new();
            press.set_propagation_phase(gtk::PropagationPhase::Capture);
            press.connect_pressed(move |gesture, _, _, _| {
                if let Some(tree) = weak.upgrade() {
                    tree.shift_click.set(
                        gesture
                            .current_event_state()
                            .contains(gtk::gdk::ModifierType::SHIFT_MASK),
                    );
                }
            });
            check.add_controller(press);
        }
        // Staged view: everything selected by default — the checkboxes
        // show what the commit takes (which is all of it).
        if staged_on {
            check.set_active(true);
        }
        let row = adw::ActionRow::builder()
            // Text from the agent, the repository, or an adapter:
            // never parsed as markup.
            .use_markup(false)
            .title(
                rel.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
            .subtitle(rel.display().to_string())
            // One line each, ellipsized: a wrapping title in a narrow
            // column went to a letter a line once the sync's words took
            // their place beside it.
            .title_lines(1)
            .subtitle_lines(1)
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
        // Its sync, left of its letter as in the tree.
        let mark = crate::sync_mark::SyncMark::new();
        mark.set_row(&row);
        row.add_suffix(&mark.widget);
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
                matches!(state, FileState::Modified | FileState::Conflicted) && !self.read_only(),
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
        (row, check, mark)
    }

    /// One environment branch's changed files, against its merge base
    /// with the current branch. Rows open as diffs — the editor's Changes
    /// face, the same plumbing the Dirty and Staged lists use.
    fn render_review_files(self: &Rc<Self>, aim: ReviewAim) {
        if self.git.borrow().is_none() {
            return;
        }
        // The checkout's root, wherever it is: the rows are files to open,
        // and the peer's folder is not where the files are.
        let Some(workdir) = self.worktree_root() else {
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
            row.add_prefix(&Self::back_gutter());
            list.append(&row);
        }
        let tooltip = format!(
            "Opens this file as {branch} left it, against {} — read-only, and not \
             your working copy",
            aim.target
        );
        for file in changed {
            let row = adw::ActionRow::builder()
                // Text from the agent, the repository, or an adapter:
                // never parsed as markup.
                .use_markup(false)
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
            row.add_prefix(&Self::back_gutter());
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

    /// The column the back chevron sits in, as an empty widget the rows
    /// under it wear too.
    ///
    /// A back row's chevron is a prefix, and a prefix pushes its row's
    /// title 28px right of a row that has none — so the way out of the
    /// review sat indented from the very files it was the header for,
    /// which reads as an accident rather than as a hierarchy. The chevron
    /// hangs in a gutter instead, which is how a navigation page aligns a
    /// back affordance against its content: give every row in the list
    /// the same prefix column and every title in it shares one left edge,
    /// the header's included.
    ///
    /// An empty `GtkImage` rather than a box of some measured width,
    /// because the width to match is *the chevron's* — the same widget
    /// asking for the same icon size, so the two stay equal through a
    /// theme that scales icons and through anything that changes the row's
    /// own spacing.
    fn back_gutter() -> gtk::Image {
        gtk::Image::new()
    }

    /// Fill the branch dropdown: every local branch (current checked,
    /// activate to switch) plus a create-and-switch entry.
    fn populate_branch_menu(self: &Rc<Self>) {
        if self.git.borrow().is_none() {
            return;
        }
        // The branch checked out in the WORKING tree — the peer's may lag
        // it when the folder is dirty — is the one the status pass read.
        let current = self.status_branch.borrow().clone();
        let branches = self
            .git
            .borrow()
            .as_ref()
            .and_then(|g| g.local_branches().ok())
            .unwrap_or_default();

        // Under a query the menu lists the branches that match (all of
        // them, matches in bold, when the ghost is on): the button is the
        // way to the list, and the list answers the query like every other
        // surface.
        let query = self.query.borrow().clone();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        for branch in &branches {
            let matched = query.is_empty() || query.matches(branch);
            if !matched && !query.ghost {
                continue;
            }
            let row = adw::ActionRow::builder()
                .title(if query.is_empty() {
                    glib::markup_escape_text(branch).to_string()
                } else {
                    query.highlight_markup(branch)
                })
                // One line, ellipsized — never hyphen-wrap a branch name.
                .title_lines(1)
                .activatable(true)
                .build();
            if !matched {
                row.add_css_class("search-dim");
            }
            if Some(branch) == current.as_ref() {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            let weak = Rc::downgrade(self);
            let name = branch.clone();
            row.connect_activated(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                tree.branch_popover.popdown();
                tree.run_branch_op(name.clone(), false);
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
            create.connect_clicked(move |_| {
                let name = entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                let Some(tree) = weak.upgrade() else { return };
                tree.branch_popover.popdown();
                tree.run_branch_op(name, true);
            });
        }
        // Enter in the field is Create: a name typed is a name meant
        // (David, 2026-10-08: "When I type a new branch name and press
        // enter, it should apply the change without a button click").
        {
            let create = create.clone();
            entry.connect_activate(move |_| create.emit_clicked());
        }
        let create_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        create_row.set_margin_top(6);
        create_row.append(&entry);
        create_row.append(&create);

        // Only the branches scroll: the create row stands under them,
        // whole, however many there are. Scrolled with them it was the
        // bottom of a 360px window over a taller list, cut through the
        // field and the button.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .max_content_height(360)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        // `TASTE_PROBE_BRANCHES` shoots it by this name.
        content.set_widget_name("branch-menu");
        // Real menu width: branch names and the create entry need room.
        content.set_width_request(260);
        content.append(&scroller);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&create_row);
        self.branch_popover.set_child(Some(&content));
    }

    /// Switch to (or create-and-switch to) a branch, off the main thread;
    /// failures (e.g. conflicting working-tree changes) surface as toasts.
    fn run_branch_op(self: &Rc<Self>, name: String, create: bool) {
        if self.refuse_read_only() {
            return;
        }
        let events = self.workspace.events.clone();
        let worktree = self.worktree();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let branch = name.clone();
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                if create {
                    worktree.create_branch(&branch).map_err(|e| e.to_string())
                } else {
                    worktree.switch_branch(&branch).map_err(|e| e.to_string())
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
                // Redrawn as the view the toggles say — rebuild() alone is
                // the whole tree, which put All under a lit Dirty after
                // every new branch (David, 2026-09-23).
                tree.refresh_status();
                if tree.filters_active() {
                    tree.render_filter_view();
                } else {
                    tree.rebuild();
                }
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
        let workdir = self.worktree_root();
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
        let untracked = selected
            .iter()
            .filter(|p| status.get(&rel_of(p)) == Some(&FileState::Untracked))
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
                // Away from the commit is out of the pipeline altogether:
                // the stash stops holding them (David, 2026-09-28: "I should
                // also be able to dispose of items from the stash, not just
                // unstash them").
                vec![(
                    "Discard",
                    in_stash,
                    "discard-stashed",
                    "Delete the checked files' stashed changes — the stash stops holding \
                     them, and the working tree is not touched",
                )],
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
                vec![
                    (
                        "← Stash",
                        stageable + staged,
                        "stash",
                        "Set the checked files aside — out of the working tree",
                    ),
                    // Out of the pipeline altogether: git stops seeing
                    // them (David, 2026-09-23: "I should have an ignore
                    // option for checked dirty files"). Untracked ones
                    // only — .gitignore does not untrack a file git
                    // already has, and a button that says Ignore over a
                    // modified file would do nothing it could show.
                    (
                        "Ignore",
                        untracked,
                        "ignore",
                        "Add the checked untracked files to .gitignore",
                    ),
                ],
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
            // Discarding stashed work has no other copy to come back to, so
            // it is the trash every other deletion here is: pressed and
            // held until the ring closes, a release before that a toast
            // (David, 2026-09-28: "The discard should use a trash icon and
            // hold-to-confirm like other trash operations").
            if *op == "discard-stashed" {
                let hold = crate::holdbutton::HoldButton::new("user-trash-symbolic", tip);
                hold.widget.set_sensitive(*count > 0 && writable);
                let weak = Rc::downgrade(self);
                hold.set_on_confirm(move || {
                    if let Some(tree) = weak.upgrade() {
                        tree.run_selection_op("discard-stashed");
                    }
                });
                let events = self.workspace.events.clone();
                hold.set_on_early_release(move |note| events.publish(Event::Toast(note)));
                let widget = hold.widget.clone();
                *self.stash_discard.borrow_mut() = Some(hold);
                return widget;
            }
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
        let Some(root) = self.worktree_root() else {
            return;
        };
        // Ignore takes the untracked ones only (see the button), read off
        // the status here, where it is.
        let untracked: HashSet<PathBuf> = {
            let status = self.status.borrow();
            selected
                .iter()
                .filter_map(|p| p.strip_prefix(&root).ok())
                .filter(|rel| status.get(*rel) == Some(&FileState::Untracked))
                .map(Path::to_path_buf)
                .collect()
        };
        let worktree = self.worktree();
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let err = |e: anyhow::Error| e.to_string();
                let rels: Vec<PathBuf> = selected
                    .iter()
                    .filter_map(|p| p.strip_prefix(&root).ok().map(|r| r.to_path_buf()))
                    .collect();
                match op {
                    "stage" => {
                        for rel in &rels {
                            worktree.stage(rel).map_err(err)?;
                        }
                    }
                    "unstage" => {
                        for rel in &rels {
                            worktree.unstage(rel).map_err(err)?;
                        }
                    }
                    "stash" => {
                        worktree
                            .stash_paths(&rels, "taste-ide selection")
                            .map_err(err)?;
                    }
                    "keep-yours" | "take-remote" => {
                        // A rebase inverts git's ours/theirs: replaying
                        // YOUR commits onto the remote tip makes --ours
                        // the remote side. The buttons speak meaning;
                        // this maps meaning back to git's flag.
                        let keep_yours = op == "keep-yours";
                        let side = match (worktree.rebase_in_progress(), keep_yours) {
                            (true, true) | (false, false) => "--theirs",
                            (true, false) | (false, true) => "--ours",
                        };
                        worktree.checkout_side(side, &rels).map_err(err)?;
                        // Taking a side resolves: mark it so.
                        for rel in &rels {
                            worktree.stage(rel).map_err(err)?;
                        }
                    }
                    "mark-resolved" => {
                        for rel in &rels {
                            worktree.stage(rel).map_err(err)?;
                        }
                    }
                    "ignore" => {
                        let ignoring: Vec<&PathBuf> =
                            rels.iter().filter(|rel| untracked.contains(*rel)).collect();
                        let files = worktree.files();
                        let path = root.join(".gitignore");
                        let current = match files.read_to_string(&path) {
                            Ok(text) => text,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                            Err(e) => return Err(format!("reading .gitignore: {e}")),
                        };
                        let updated = with_ignored(&current, &ignoring);
                        if updated != current {
                            files
                                .write(&path, updated.as_bytes())
                                .map_err(|e| format!("writing .gitignore: {e}"))?;
                        }
                    }
                    "unstash" => {
                        for rel in &rels {
                            // Re-read per file: an entry rebuilt below is a
                            // new entry, under a new id. The entry is named
                            // by its commit, never its position, which a
                            // stash pushed meanwhile would have moved.
                            let entries = worktree.stash_entries().map_err(err)?;
                            let Some(entry) =
                                entries.iter().find(|entry| entry.paths.contains(rel))
                            else {
                                continue;
                            };
                            worktree.unstash_file(&entry.id, rel).map_err(err)?;
                            // Out of its entry too, or it goes on showing as
                            // stashed beside the copy now in the tree; an
                            // entry that held it alone is dropped.
                            worktree.remove_from_stash(&entry.id, rel).map_err(err)?;
                        }
                    }
                    "discard-stashed" => {
                        for rel in &rels {
                            let entries = worktree.stash_entries().map_err(err)?;
                            let Some(entry) =
                                entries.iter().find(|entry| entry.paths.contains(rel))
                            else {
                                continue;
                            };
                            worktree.remove_from_stash(&entry.id, rel).map_err(err)?;
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
                    "discard-stashed" => "Discarded from the stash".to_string(),
                    "keep-yours" => "Resolved with your version".to_string(),
                    "take-remote" => "Resolved with the remote version".to_string(),
                    "mark-resolved" => "Marked resolved".to_string(),
                    "ignore" => "Added to .gitignore".to_string(),
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
        let workdir = self.worktree_root();
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
        if self.commit_blocked.replace(!all_selected) != !all_selected {
            self.announce_commit_state();
        }
    }

    /// Open the intervention panel with a title; returns the content box.
    /// Replaces any previous intervention. Closing cancels.
    fn open_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
        self.open_pane(title, true)
    }

    fn open_pane(self: &Rc<Self>, title: &str, closable: bool) -> gtk::Box {
        // Ad-hoc until a selection pane claims otherwise after building.
        self.pane.set(PaneKind::Adhoc);
        self.intervention.open(title, closable)
    }

    fn close_intervention(&self) {
        self.pane.set(PaneKind::None);
        self.intervention_file.borrow_mut().take();
        self.intervention.close();
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
            let Some(workdir) = tree.worktree_root() else {
                return;
            };
            let worktree = tree.worktree();
            let rel = abs.strip_prefix(&workdir).unwrap_or(&abs).to_path_buf();
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            let name = rel.display().to_string();
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime()
                    .spawn_blocking(move || worktree.restore_file(&rel).map_err(|e| e.to_string()));
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
        let Some(workdir) = self.worktree_root() else {
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
            let worktree = tree.worktree();
            let rel = rel.clone();
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    worktree
                        .stash_file(&rel, &message)
                        .map_err(|e| e.to_string())
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
        let Some(workdir) = self.worktree_root() else {
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
            let files = tree.worktree().files();
            let events = tree.workspace.events.clone();
            let weak = Rc::downgrade(&tree);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    let mut content = files.read_to_string(&gitignore).unwrap_or_default();
                    if !content.lines().any(|line| line.trim() == expr) {
                        if !content.is_empty() && !content.ends_with('\n') {
                            content.push('\n');
                        }
                        content.push_str(&expr);
                        content.push('\n');
                        files
                            .write(&gitignore, content.as_bytes())
                            .map_err(|e| e.to_string())?;
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

    /// Fetch, then — when `rebase` — rebase onto the remote tip: the full
    /// sync, conflicts and all (a paused rebase gets the Conflicts view and
    /// the Continue/Abort pair). Without `rebase` it is the refresh
    /// button's fetch: every remote, the push target's tip, and the issues
    /// ref, and the counts redrawn. Push stays a separate, deliberate
    /// action.
    /// The submodule banner's Set Up: each submodule the folder has not set
    /// up, cloned, fetched, or moved forward in the folder with the user's
    /// keys, as Fetch runs (`taste_git::submodule_setup`); then all of them
    /// decided again, since a fetch can leave one behind and so to be moved
    /// forward, until a round finds nothing left or the same as before. One
    /// that fails says so and the rest go on. The sync gives Personal what
    /// the folder now has.
    fn set_up_submodules(self: &Rc<Self>) {
        if self.submodule_busy.replace(true) {
            return;
        }
        button_busy(&self.submodule_bar.button);
        let root = self.refs_root();
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut before = Vec::new();
            loop {
                let found = crate::runtime::runtime()
                    .spawn_blocking({
                        let root = root.clone();
                        move || {
                            let Some(git) = GitWorkspace::discover(&root) else {
                                return Vec::new();
                            };
                            git.submodules_to_set_up()
                                .into_iter()
                                .filter_map(|setup| {
                                    let command = git.submodule_setup_command(&setup)?;
                                    Some((setup, command))
                                })
                                .collect::<Vec<_>>()
                        }
                    })
                    .await
                    .unwrap_or_default();
                let now: Vec<_> = found
                    .iter()
                    .map(|(setup, _)| (setup.path.clone(), setup.need))
                    .collect();
                if found.is_empty() || now == before {
                    break;
                }
                before = now;
                for (setup, (program, args)) in found {
                    let envs = crate::ssh_agent::network_env(root.clone(), setup.url.clone()).await;
                    let handle = crate::runtime::runtime().spawn(run_git_step_within(
                        program,
                        args,
                        envs,
                        SUBMODULE_SETUP_TIMEOUT,
                    ));
                    let failure = match handle.await {
                        Ok(Ok(_)) => None,
                        Ok(Err(reason)) => Some(reason),
                        Err(_) => Some("interrupted".to_string()),
                    };
                    if let Some(reason) = failure {
                        events.publish(Event::Toast(format!(
                            "setting up the submodule {} failed: {reason}",
                            setup.path.display()
                        )));
                    }
                }
            }
            let Some(tree) = weak.upgrade() else { return };
            tree.submodule_busy.set(false);
            button_idle(&tree.submodule_bar.button);
            tree.submodule_bar.button.set_sensitive(true);
            tree.refresh_status();
        });
    }

    fn sync(self: &Rc<Self>, mode: Pull, from: PullFrom) {
        // The working tree's branch, as the push has it: for a checkout in
        // a VM, not this repository's HEAD, whose upstream is main's.
        let branch = self.status_branch.borrow().clone();
        // Every remote is fetched, because a fork's workflow has two: the
        // base it rebases onto and the fork it pushes to. What moves the
        // branch is the button's: ↓ its own copy on the remote, ⇊ the
        // canonical branch ([`PullFrom`]).
        let Some((fetch, fetch_issues, move_command, remote, upstream)) =
            self.git.borrow().as_ref().map(|git| {
                let target = match (&branch, from) {
                    (Some(branch), PullFrom::Own) => git.own_target_of(branch),
                    (Some(branch), PullFrom::Canonical) => git.canonical_ref_of(branch),
                    (None, _) => git.upstream_ref(),
                };
                let onto = target.as_deref().unwrap_or("@{upstream}");
                (
                    git.fetch_all_command(),
                    git.fetch_issues_command(),
                    match mode {
                        Pull::Rebase => Some(git.rebase_onto_command(onto)),
                        Pull::FastForward => Some(git.fast_forward_onto_command(onto)),
                        Pull::Fetch => None,
                    },
                    git.upstream_remote_url(),
                    target,
                )
            })
        else {
            return;
        };
        let root = self.refs_root();
        // The fetch is the peer's, on this host, with the user's keys. The
        // rebase is the WORKING tree's: here when the tree is here, and in
        // the VM when it is there — given the peer's remote-tracking refs
        // first, and syncing the peer after.
        let worktree = self.worktree();
        let share_remotes = self.share_remotes.borrow().clone();
        let note_agent = self.note_agent.borrow().clone();
        // The button pressed shows a spinner; nothing is written beside the
        // branch name, where "fetching…" had no room, came out as a bare
        // "…", and moved every button as it came and went (David,
        // 2026-10-19: "It jostles things around").
        self.sync_button.set_sensitive(false);
        self.last_fetch.set(Some(std::time::Instant::now()));
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // The user pressed this: a question it runs into — a password,
            // a passphrase, a key to touch — is theirs to answer, on the
            // IDE's strip rather than a terminal nobody is watching, and
            // through the IDE's own agent, whose touch notices land on the
            // same strip (`ssh_agent`).
            let envs = crate::ssh_agent::network_env(root.clone(), remote).await;
            let mut failed = false;
            // How far behind the branch was once the fetch had landed — the
            // commits the rebase is about to take — for the toast at the
            // end (David, 2026-09-22: "Same for pull").
            let mut behind: Option<(usize, Option<String>)> = None;
            // What the click can do, which only the fetch can say.
            let mut plan: Option<taste_git::PullPlan> = None;
            // Whether the branch is the default one, whose replacing push
            // is the long hold.
            let mut on_canonical = false;
            {
                let (program, args) = fetch;
                let handle =
                    crate::runtime::runtime().spawn(run_git_step(program, args, envs.clone()));
                let failure = match handle.await {
                    Ok(Ok(_)) => None,
                    Ok(Err(reason)) => Some(reason),
                    Err(_) => Some("interrupted".to_string()),
                };
                if let Some(reason) = failure {
                    events.publish(Event::Toast(format!("fetch failed: {reason}")));
                    failed = true;
                } else {
                    // A push target named by URL is asked for its tip here
                    // too, so the counts after this Sync are true ones.
                    let _ = crate::runtime::runtime()
                        .spawn(observe_push_tip(root.clone(), branch.clone(), envs.clone()))
                        .await;
                    let measure_root = root.clone();
                    let measure_branch = branch.clone();
                    let measure_target = upstream.clone();
                    let measured = crate::runtime::runtime()
                        .spawn_blocking(move || {
                            let git = taste_git::GitWorkspace::discover(&measure_root)?;
                            let sync = match &measure_branch {
                                Some(branch) => git.sync_status_of(branch).ok(),
                                None => git.sync_status().ok(),
                            }?;
                            let plan = match (&measure_branch, &measure_target) {
                                (Some(branch), Some(target)) => {
                                    git.pull_plan_onto(branch, target, from == PullFrom::Own)
                                }
                                _ => None,
                            };
                            let counted = match (from, sync.canonical) {
                                (PullFrom::Canonical, Some(canonical)) => {
                                    (canonical.new, Some(canonical.name))
                                }
                                _ => (sync.own_behind, sync.upstream),
                            };
                            Some((counted, plan, sync.on_canonical))
                        })
                        .await;
                    if let Ok(Some((measured, measured_plan, canonical))) = measured {
                        behind = Some(measured);
                        plan = measured_plan;
                        on_canonical = canonical;
                    }
                }
            }
            // Each submodule cloned in the folder, fetched with the same
            // keys: its refs are as much the user's as the parent's (David,
            // 2026-10-06: "We should ensure the refs get fetched as part of
            // the 'fetch' button and other fetch operations"). One that
            // fails says so and the rest go on.
            if !failed {
                let commands = crate::runtime::runtime()
                    .spawn_blocking({
                        let root = root.clone();
                        move || submodule_fetch_commands(&root)
                    })
                    .await
                    .unwrap_or_default();
                for (name, (program, args)) in commands {
                    let handle =
                        crate::runtime::runtime().spawn(run_git_step(program, args, envs.clone()));
                    let failure = match handle.await {
                        Ok(Ok(_)) => None,
                        Ok(Err(reason)) => Some(reason),
                        Err(_) => Some("interrupted".to_string()),
                    };
                    if let Some(reason) = failure {
                        events.publish(Event::Toast(format!(
                            "fetching the submodule {name} failed: {reason}"
                        )));
                    }
                }
            }
            // What was fetched, given to the checkout in the VM, which
            // cannot fetch for itself: the parent's remote-tracking refs and
            // each submodule's — after every fetch, not only before a
            // rebase, so a branch the user fetched is there to be checked
            // out or compared against.
            let shared = if !failed && !worktree.is_local() {
                match share_remotes.clone() {
                    Some(share) => crate::runtime::runtime()
                        .spawn_blocking(move || share())
                        .await
                        .unwrap_or_else(|_| Err("interrupted".to_string())),
                    None => Ok(()),
                }
            } else {
                Ok(())
            };
            if let Err(reason) = &shared {
                events.publish(Event::Toast(format!(
                    "the fetched refs did not reach Personal: {reason}"
                )));
            }
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            let (count, upstream_name) = behind.clone().unwrap_or((0, None));
            let upstream_name = upstream_name.unwrap_or_else(|| "the remote".to_string());
            // A click with commits of the branch's own to replay moves
            // nothing, and says the hold that would.
            let held_back = match (mode, plan) {
                // The branch's own old copies, not anyone's new commits: a
                // click has nothing to pull, and a hold rebasing onto them
                // replays the branch onto itself — how a rebase the agent
                // had done was undone (2026-10-19). Push is what replaces
                // them.
                (
                    Pull::FastForward | Pull::Rebase,
                    Some(taste_git::PullPlan::Rewritten { theirs, .. }),
                ) => Some(format!(
                    "Nothing to pull: the {theirs} commit{} on {upstream_name} this branch \
                     lacks {} its own, from before it was rewritten here. Hold Push for {} \
                     seconds to replace {}",
                    plural(theirs),
                    if theirs == 1 { "is" } else { "are" },
                    push_hold_for(on_canonical, true).as_secs(),
                    if theirs == 1 { "it" } else { "them" }
                )),
                (Pull::FastForward, Some(taste_git::PullPlan::Rebase { mine, theirs })) => {
                    Some(format!(
                        "Not pulled: this branch has {mine} commit{} of its own. Hold {} for \
                         {} seconds to rebase {} onto the {theirs} new commit{} on \
                         {upstream_name}",
                        plural(mine),
                        match from {
                            PullFrom::Own => "↓",
                            PullFrom::Canonical => "⇊",
                        },
                        crate::holdbutton::HOLD.as_secs(),
                        if mine == 1 { "it" } else { "them" },
                        plural(theirs)
                    ))
                }
                _ => None,
            };
            if let Some(note) = &held_back {
                events.publish(Event::Toast(note.clone()));
            }
            let moving = !failed
                && held_back.is_none()
                && !matches!(plan, Some(taste_git::PullPlan::UpToDate));
            let verb = if mode == Pull::Rebase {
                "rebase"
            } else {
                "fast-forward"
            };
            match move_command {
                Some((program, args)) if moving && worktree.is_local() => {
                    let handle =
                        crate::runtime::runtime().spawn(run_git_step(program, args, envs.clone()));
                    let failure = match handle.await {
                        Ok(Ok(_)) => None,
                        Ok(Err(reason)) => Some(reason),
                        Err(_) => Some("interrupted".to_string()),
                    };
                    if let Some(reason) = failure {
                        failed = true;
                        events.publish(Event::Toast(format!("{verb} failed: {reason}")));
                    }
                }
                Some(_) if moving => {
                    let worktree = worktree.clone();
                    let handle = crate::runtime::runtime().spawn_blocking(move || {
                        let Some(upstream) = upstream else {
                            return Err("this branch has no upstream to pull from".to_string());
                        };
                        shared?;
                        match mode {
                            Pull::Rebase => worktree.rebase_onto(&upstream),
                            _ => worktree.fast_forward_onto(&upstream),
                        }
                        .map_err(|e| e.to_string())
                    });
                    match handle.await {
                        Ok(Ok(())) => {}
                        Ok(Err(reason)) => {
                            failed = true;
                            events.publish(Event::Toast(format!("{verb} failed: {reason}")))
                        }
                        Err(_) => {
                            failed = true;
                            events.publish(Event::Toast(format!("{verb} failed: interrupted")))
                        }
                    }
                }
                _ => {}
            }
            if mode != Pull::Fetch && !failed && held_back.is_none() {
                events.publish(Event::Toast(if count == 0 {
                    format!("Already up to date with {upstream_name}")
                } else {
                    format!(
                        "Pulled {count} commit{} from {upstream_name}",
                        plural(count)
                    )
                }));
                // The branch moved under the agent: said with its next
                // prompt, so it reads the branch again rather than its
                // memory of it (2026-10-19: an agent that found its rebase
                // undone spent a turn working out why).
                if let (true, Some(branch), Some(note_agent)) =
                    (moving && count > 0, branch.as_deref(), note_agent.as_ref())
                {
                    note_agent(match mode {
                        Pull::Rebase => format!(
                            "The user's Pull rebased {branch} onto {upstream_name}, taking \
                             {count} commit{} from there: the branch's own commits were \
                             replayed on top, so their hashes changed.",
                            plural(count)
                        ),
                        _ => format!(
                            "The user's Pull fast-forwarded {branch} to {upstream_name}: \
                             {count} new commit{}.",
                            plural(count)
                        ),
                    });
                }
            }
            // The issues ref, second and quietly. A remote that has never
            // seen an issue makes this fail, which is the normal case
            // before the first push, so its failure is silence rather than
            // a toast about something the user did not ask for.
            let fetched = crate::runtime::runtime()
                .spawn(run_git_step(fetch_issues.0, fetch_issues.1, envs.clone()))
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
        if self.git.borrow().is_none() {
            return;
        }
        let worktree = self.worktree();
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let _ = tokio::task::spawn_blocking(move || worktree.rebase_abort()).await;
            events.publish(Event::GitStatusChanged);
        });
    }

    /// `git rebase --continue`: git itself checks the preconditions
    /// (everything resolved and marked), and its refusal — usually
    /// "unmerged files" — comes through as the toast.
    fn continue_rebase(self: &Rc<Self>) {
        if self.git.borrow().is_none() {
            return;
        }
        let worktree = self.worktree();
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let output = tokio::task::spawn_blocking(move || worktree.rebase_continue()).await;
            match output {
                Ok(Ok(())) => {
                    events.publish(Event::Toast("Rebase complete".into()));
                }
                // A failure also means "stopped at the NEXT conflict":
                // either way git's last line says what's up, and the
                // refreshed Conflicts view shows where.
                Ok(Err(e)) => events.publish(Event::Toast(format!("Rebase: {e}"))),
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
        let Some((program, args, remote)) = self
            .git
            .borrow()
            .as_ref()
            .map(|g| (g.fetch_all_command(), g.upstream_remote_url()))
            .map(|((program, args), remote)| (program, args, remote))
        else {
            return;
        };
        let observe_root = self.refs_root();
        let observe_branch = self.status_branch.borrow().clone();
        self.last_fetch.set(Some(std::time::Instant::now()));
        // First, whether fetching would ask the user to touch a key: a
        // background fetch must never pop a presence prompt, so a remote
        // over SSH whose key needs one is not fetched here at all — Pull is
        // the deliberate act — and the tooltip says so. Asked every time
        // rather than once, because the key comes and goes with the agent
        // (David, 2026-09-16: "I don't want background polling to pop open
        // modals telling me to tap the key").
        // Every remote `fetch --all` reaches, not only the branch's
        // upstream, which a detached HEAD does not even have.
        let check_root = self.refs_root();
        let check = crate::runtime::runtime().spawn_blocking(move || {
            let urls = taste_git::GitWorkspace::discover(&check_root)
                .map(|git| git.remote_urls())
                .unwrap_or_else(|| remote.into_iter().collect());
            taste_git::presence::any_needs_presence(urls.iter().map(String::as_str))
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let hold = check.await.ok().flatten();
            let Some(tree) = weak.upgrade() else { return };
            let was_held = tree.fetch_hold.borrow().is_some();
            if let Some(reason) = &hold {
                if !was_held {
                    tracing::info!("background fetch held: {reason}");
                }
            }
            *tree.fetch_hold.borrow_mut() = hold.clone();
            if hold.is_some() != was_held {
                // The tooltip carries the reason; redraw it.
                tree.workspace.events.publish(Event::GitStatusChanged);
            }
            if hold.is_some() {
                return;
            }
            let events = tree.workspace.events.clone();
            let share = tree.share_remotes.borrow().clone();
            crate::runtime::runtime().spawn(async move {
                // What the fetch moves, the agents' checkouts are given too
                // — Personal's as its remote-tracking refs, every other
                // environment's as `upstream/*` — so a fetch nobody pressed
                // still keeps what the agents compare against current. Only
                // when something moved: most of these fetch nothing.
                let fetched_refs = {
                    let root = observe_root.clone();
                    move || {
                        taste_git::GitWorkspace::discover(&root)
                            .and_then(|git| git.refs_under("refs/remotes/").ok())
                            .unwrap_or_default()
                    }
                };
                let before = tokio::task::spawn_blocking(fetched_refs.clone())
                    .await
                    .unwrap_or_default();
                observe_push_tip(
                    observe_root,
                    observe_branch,
                    taste_git::non_interactive_env(),
                )
                .await;
                match tokio::process::Command::new(&program)
                    .envs(taste_git::non_interactive_env())
                    .args(&args)
                    .output()
                    .await
                {
                    // The refresh this triggers is throttled by last_fetch,
                    // so fetch → refresh → fetch can't loop.
                    Ok(out) if out.status.success() => {
                        events.publish(Event::GitStatusChanged);
                        let after = tokio::task::spawn_blocking(fetched_refs)
                            .await
                            .unwrap_or_default();
                        if let (true, Some(share)) = (after != before, share) {
                            if let Ok(Err(reason)) =
                                tokio::task::spawn_blocking(move || share()).await
                            {
                                tracing::info!(
                                    "the fetched refs did not reach the checkouts: {reason}"
                                );
                            }
                        }
                    }
                    Ok(out) => tracing::debug!(
                        "background fetch: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(e) => tracing::debug!("background fetch failed: {e}"),
                }
            });
        });
    }

    fn repo_relative(&self, path: &Path) -> Option<PathBuf> {
        let workdir = self.worktree_root()?;
        path.strip_prefix(workdir).ok().map(Path::to_path_buf)
    }

    /// Whether a sync of the folder with Personal is pending or under way.
    /// A submodule's warning waits for the pass to end; when it does, the
    /// tree asks again, and a problem still there is shown.
    pub fn set_folder_syncing(self: &Rc<Self>, syncing: bool) {
        if self.folder_syncing.replace(syncing) == syncing {
            return;
        }
        let warned = self
            .submodule_heads
            .borrow()
            .values()
            .any(|head| head.pin.is_problem());
        if !syncing {
            self.refresh_status();
        }
        // The visible rows redrawn, as a status change redraws them — not
        // the tree rebuilt, which lists it again and selects the file in
        // front again, opening every folder above it: folders the user had
        // closed opened by themselves at every sync (David, 2026-10-06:
        // "Did the folders start auto-expanding?").
        if warned {
            self.rebuild_rows_in_place();
        }
    }

    /// Where the submodule at `path` is, when it is one.
    fn submodule_head(&self, path: &Path) -> Option<taste_devcontainer::worktree::SubmoduleHead> {
        let rel = self.repo_relative(path)?;
        self.submodule_heads.borrow().get(&rel).cloned()
    }

    fn state_of(&self, node: &FileNode) -> FileState {
        let Some(rel) = self.repo_relative(&node.path) else {
            return FileState::Clean;
        };
        synced_or(
            state_for(&self.status.borrow(), &rel, node.is_dir),
            &self.sync_ignored.borrow(),
            &rel,
            node.is_dir,
        )
    }

    /// Build the (fresh) tree model. Called on construction and when the
    /// ignored-files toggle flips. Git-status changes only restyle rows.
    fn rebuild(self: &Rc<Self>) {
        self.close_open_menu();
        self.remember_expanded();
        // A new list means new rows; the handles for the old one address
        // widgets that are about to be dropped (and, after an `aim_at`,
        // paths relative to a different repository).
        self.rows.borrow_mut().clear();
        let showing = self.showing.get();
        let synced = std::sync::Arc::new(self.sync_ignored.borrow().clone());
        // Search shapes the tree: matches-only filters to matching files
        // (autoexpanded); ghost mode keeps every file and dims the rest.
        let search = self.search_view.borrow().clone();
        let ghost_mode = self.query.borrow().ghost;
        let filter: Option<Rc<HashSet<PathBuf>>> = match (&search, ghost_mode) {
            (Some(view), false) => Some(view.visible.clone()),
            _ => None,
        };
        let view_root = self.view_root();
        let worktree = self.worktree();
        // Template suggestions are noise in search results — and an offer
        // to create a file is a lie in a read-only view.
        //
        // Worked out off the main thread, by the first listing that needs
        // them. The existence checks are stats of the checkout wherever it
        // is — for one in a VM, a round trip each to its files service —
        // and done here they held the window every time the tree was
        // re-aimed: switching back to Personal froze it for as long as the
        // service took to answer them all (David, 2026-09-23: "that
        // shouldn't block my UI").
        let ghosts =
            Ghosts::new((search.is_none() && !self.read_only()).then(|| view_root.clone()));
        // The root listing is handed to `TreeListModel` empty and filled
        // in once the walk lands on the blocking pool — so `rebuild()`
        // returns immediately and the list this call attaches never blocks
        // a frame on `.gitignore` parsing, however large the checkout.
        let root_store = gtk::gio::ListStore::new::<BoxedAnyObject>();
        let child_ghosts = ghosts.clone();
        let child_worktree = worktree.clone();
        // What syncs, read when a folder is OPENED, not when the model was
        // built: it arrives with the first status, after the first build,
        // and a copy taken here listed every synced folder empty (its
        // children ignored, and not yet known to sync).
        let child_tree = Rc::downgrade(self);
        fill_dir_store_async(
            &root_store,
            worktree,
            view_root,
            showing,
            synced,
            ghosts,
            filter.as_ref().map(|f| (**f).clone()),
        );
        let autoexpand = filter.is_some();
        let child_filter = filter.clone();
        let tree_model = gtk::TreeListModel::new(root_store, false, autoexpand, move |item| {
            let node = item.downcast_ref::<BoxedAnyObject>()?.borrow::<FileNode>();
            // A ghost folder opens like any other, onto its ghosts: there
            // is nothing on disk to list, and `scan_dir_nodes` places the
            // ghosts that belong in it.
            if node.is_dir {
                // Same deal one level down: expanding a folder returns a
                // store immediately (GTK splices children in fine as they
                // arrive) instead of walking that directory on the thread
                // driving the frame clock. The ghosts come along, placed by
                // `scan_dir_nodes` into the folder they belong to.
                let store = gtk::gio::ListStore::new::<BoxedAnyObject>();
                fill_dir_store_async(
                    &store,
                    child_worktree.clone(),
                    node.path.clone(),
                    showing,
                    child_tree
                        .upgrade()
                        .map(|tree| std::sync::Arc::new(tree.sync_ignored.borrow().clone()))
                        .unwrap_or_default(),
                    child_ghosts.clone(),
                    child_filter.as_ref().map(|f| (**f).clone()),
                );
                Some(store.upcast())
            } else {
                None
            }
        });
        // Reopen what was open, as the rows arrive. The listings land
        // asynchronously — the root's, then each expanded folder's — so
        // this cannot be one pass after construction: every splice is
        // looked at, and a folder the user had open is expanded the moment
        // its row exists, which is what makes its own children arrive and
        // get the same look. Not under a search, whose model autoexpands.
        if !autoexpand {
            let tree = Rc::downgrade(self);
            tree_model.connect_items_changed(move |model, position, _removed, added| {
                let Some(tree) = tree.upgrade() else { return };
                let reopen: Vec<gtk::TreeListRow> = {
                    let remembered = tree.expanded_dirs.borrow();
                    (position..position + added)
                        .filter_map(|index| model.row(index))
                        .filter(|row| {
                            !row.is_expanded()
                                && row
                                    .item()
                                    .and_downcast::<BoxedAnyObject>()
                                    .is_some_and(|item| {
                                        let node = item.borrow::<FileNode>();
                                        node.is_dir && remembered.contains(&node.path)
                                    })
                        })
                        .collect()
                };
                // Outside the borrow: expanding splices rows, which lands
                // back in this handler.
                for row in reopen {
                    row.set_expanded(true);
                }
                // A file the editor put in front may have just got its row.
                // Asked again when the idle runs: a tab without a row that
                // came to the front meanwhile withdrew the ask, and a row
                // lit for a tab no longer in front is a highlight that
                // does not map to what is open (David, 2026-09-23: "Blue
                // highlight for an item in a panel should map 1:1 with it
                // being open in a tab").
                let pending = tree.pending_select.borrow().clone();
                if let Some(path) = pending {
                    let tree = tree.clone();
                    glib::idle_add_local_once(move || {
                        let still = tree.pending_select.borrow().as_ref() == Some(&path);
                        if still {
                            tree.select_file(path);
                        }
                    });
                } else if !tree.selection_check_queued.replace(true) {
                    // Rows spliced in or out above the lit one leave the
                    // selection at its old position, on whatever row is
                    // there now: a folder lit while a PDF was in front
                    // (David, 2026-10-20: "The wrong thing is selected in
                    // files"). Once a burst of splices is over, the lit
                    // row is the file in front again, or none is.
                    let tree = tree.clone();
                    glib::idle_add_local_once(move || {
                        tree.selection_check_queued.set(false);
                        if tree.pending_select.borrow().is_none() && !tree.selection_held.get() {
                            tree.keep_selection_on_front();
                        }
                    });
                }
            });
        }
        let selection = gtk::SingleSelection::new(Some(tree_model));
        // No row is selected until one corresponds to something — the
        // editor's tab, or a step from the search box. Autoselect would
        // put the highlight on the first row for no reason.
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&gtk::TreeExpander::new()));
        });
        let tree = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let _doing = crate::stall::doing(|| "binding a file tree row".into());
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
            let built = tree.build_row(&node);
            // A folder's icon follows its row open and closed: bound to the
            // row rather than set once, because expanding does not rebind.
            // The binding goes with the image, which the next bind replaces.
            if node.is_dir && !node.ghost {
                if let Some(image) = built.first_child().and_downcast::<gtk::Image>() {
                    let name = node
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // A submodule is a folder with git's mark, whatever
                    // its name would have given it.
                    let submodule = tree.submodule_head(&node.path).is_some();
                    row.bind_property("expanded", &image, "icon-name")
                        .transform_to(move |_, expanded: bool| {
                            if submodule {
                                return Some(
                                    if expanded {
                                        "taste-mi-folder-git-open"
                                    } else {
                                        "taste-mi-folder-git"
                                    }
                                    .to_string(),
                                );
                            }
                            crate::file_icons::icon_name(
                                &name,
                                crate::file_icons::Kind::Folder { expanded },
                                adw::StyleManager::default().is_dark(),
                            )
                        })
                        .sync_create()
                        .build();
                }
            }
            expander.set_child(Some(&built));
        });

        let list = gtk::ListView::new(Some(selection), Some(factory));
        // The same row style the sections wear: libadwaita insets a
        // `navigation-sidebar` row from the list's edge and rounds its
        // corners, so a selection reads as a lozenge rather than as a
        // full-bleed band (David, 2026-09-08: "I want the highlight style
        // for the files list to match the one from the other panels: some
        // margin with the panel border, rounded corners"). The theme's
        // metrics are the ones to meet, not to argue with — the same
        // reason `ROW_INSET` is what it is.
        list.add_css_class("navigation-sidebar");
        // ...and `files-tree` to take its METRICS back. The style brings a
        // 36px row with it, and this listing's density is settled (23px):
        // what is wanted from the theme here is the lozenge — the inset,
        // the radius, the selected fill — not its spacing.
        list.add_css_class("files-tree");
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
            if let Some(create) = node.create {
                let dir = node
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let (title, folder) = match create {
                    Create::File => ("New File", false),
                    Create::Folder => ("New Folder", true),
                };
                // The row stays selected while its name is asked for: a
                // cancel gives the selection back to the file in front, and a
                // new file takes it, as it opens (David, 2026-10-20: "it
                // should go from 'New file...' selected to the newly created
                // file selected"). A folder opens no tab, so the selection
                // goes back to the file in front then too, as it does when
                // the create is refused or fails — and only once it exists
                // does the new file take it, or the row stayed lit over a
                // failure (review, 2026-10-20).
                tree.selection_held.set(true);
                tree.clone().prompt_name_or(
                    title,
                    "Create",
                    "",
                    move |tree, name| {
                        tree.selection_held.set(false);
                        let path = dir.join(name);
                        tree.create_file_then(&path.clone(), folder, move |tree, made| {
                            if made && !folder {
                                tree.select_file(path);
                            } else {
                                tree.restore_tree_selection();
                            }
                        });
                    },
                    |tree| {
                        tree.selection_held.set(false);
                        tree.restore_tree_selection();
                    },
                );
            } else if node.is_dir || node.ghost {
                // A ghost folder opens rather than appears: what it is for
                // is the files in it, and creating one of them makes it.
                if node.ghost && !node.is_dir {
                    tree.create_ghost(&node.path);
                } else {
                    row.set_expanded(!row.is_expanded());
                }
                // The click selected the row on its way to toggling it, and
                // a folder opens no tab: the blue goes back to the file in
                // front, or to nothing when the tab in front is not a file
                // (David, 2026-09-23: "Nothing should be blue in the files
                // area if I have a tab open that corresponds to a non-file
                // object"). After the click is done with the selection.
                // In place: no scroll to it and no folder opened for it,
                // which would fight the click that just toggled one.
                let tree = tree.clone();
                glib::idle_add_local_once(move || tree.restore_tree_selection());
            } else {
                // A file with content hits opens at the first of them; once
                // it is the file on screen, each further click steps to the
                // next (the editor's listing is that file's).
                let first_hit = tree.search_view.borrow().as_ref().and_then(|view| {
                    view.hits
                        .get(&node.path)
                        .and_then(|hits| hits.first())
                        .map(|hit| hit.line)
                        // A file found by meaning alone opens at its first
                        // such place.
                        .or_else(|| view.meaning.get(&node.path).map(|(_, line)| *line))
                });
                if first_hit.is_some() {
                    let stepped = tree
                        .on_step_file
                        .borrow()
                        .as_ref()
                        .is_some_and(|hook| hook(node.path.clone()));
                    if stepped {
                        return;
                    }
                }
                tree.open(node.path.clone(), first_hit);
            }
        });

        self.list_holder.set_child(Some(&list));
    }

    /// TASTE_PROBE_CHECK only: open one folder, the way a click does, so a
    /// frame can show what a rebuild does to an open folder.
    pub fn expand_for_probe(&self, relative: &str) {
        let Some(model) = self
            .list_holder
            .child()
            .and_downcast::<gtk::ListView>()
            .and_then(|list| list.model())
            .and_downcast::<gtk::SingleSelection>()
            .and_then(|selection| selection.model())
            .and_downcast::<gtk::TreeListModel>()
        else {
            return;
        };
        let wanted = self.view_root().join(relative);
        for index in 0..model.n_items() {
            let Some(row) = model.row(index) else {
                continue;
            };
            let Some(item) = row.item().and_downcast::<BoxedAnyObject>() else {
                continue;
            };
            if item.borrow::<FileNode>().path == wanted {
                row.set_expanded(true);
                return;
            }
        }
    }

    /// Read which folders are open off the model about to be replaced.
    ///
    /// Every folder row the model has says whether it is open, and its
    /// word is final: an open one is remembered, a closed one forgotten. A
    /// folder under a closed parent is not in the model at all, so it
    /// keeps whatever was remembered — reopening the parent later brings
    /// its children back the way they were, which the model on its own
    /// does not do. A search's model autoexpands everything and says
    /// nothing about what the user chose, so it is not read.
    fn remember_expanded(&self) {
        let Some(model) = self
            .list_holder
            .child()
            .and_downcast::<gtk::ListView>()
            .and_then(|list| list.model())
            .and_downcast::<gtk::SingleSelection>()
            .and_then(|selection| selection.model())
            .and_downcast::<gtk::TreeListModel>()
        else {
            return;
        };
        // A search's model autoexpands every folder with a hit, and what
        // the user sees expanded is what stays expanded — toggling the
        // ghost, or clearing the query, must not fold what was open (David,
        // 2026-09-06). So an autoexpanded model is read like any other.
        let mut remembered = self.expanded_dirs.borrow_mut();
        for index in 0..model.n_items() {
            let Some(row) = model.row(index) else {
                continue;
            };
            let Some(item) = row.item().and_downcast::<BoxedAnyObject>() else {
                continue;
            };
            let node = item.borrow::<FileNode>();
            if !node.is_dir {
                continue;
            }
            if row.is_expanded() {
                remembered.insert(node.path.clone());
            } else {
                remembered.remove(&node.path);
            }
        }
    }

    /// One row's content: icon, name, git badge, stage/unstage toggle.
    fn build_row(self: &Rc<Self>, node: &FileNode) -> gtk::Box {
        if node.ghost {
            return self.build_ghost_row(node);
        }
        let state = self.state_of(node);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The listing's own type: smaller, softer, and with the leading
        // the smaller face gives back spent on air instead
        // (`main.rs::theme_conditional_css`).
        row.add_css_class("file-row");
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
            .build()
            .full_text_on_hover();
        let submodule = node
            .is_dir
            .then(|| self.submodule_head(&node.path))
            .flatten();
        // A submodule says what its parent pins it to, after its name and
        // dimmed: the commit, whatever branch is there — "@ 37cf0ad (main)"
        // when it is at the pin on a branch, "@ 37cf0ad → main" when it
        // has moved off it (David, 2026-10-06: "I want to see what it's
        // pinned to, even if it's equivalent to a branch").
        if let Some(head) = &submodule {
            let place = if head.pin == taste_devcontainer::worktree::SubmodulePin::Same {
                if head.at == head.pinned {
                    head.pinned.clone()
                } else {
                    format!("{} ({})", head.pinned, head.at)
                }
            } else if head.at == head.pinned {
                head.pinned.clone()
            } else {
                format!("{} → {}", head.pinned, head.at)
            };
            label.set_use_markup(true);
            label.set_label(&format!(
                "{} <span alpha=\"55%\">@ {}</span>",
                glib::markup_escape_text(&name),
                glib::markup_escape_text(&place)
            ));
        }

        // A submodule's row does not wear the parent's "M" for its commit
        // differing from the one the parent records: past it is the way a
        // submodule is while it is worked in. It wears a warning only for
        // something to fix — short of it, diverged, the commit not fetched,
        // not checked out, conflicted — and says which, as its tooltip says
        // where it stands either way (David, 2026-10-06).
        let rel = self.repo_relative(&node.path);
        let own = policy_badge(
            &self.sync_ignored.borrow(),
            rel.as_deref(),
            state,
            node.is_dir,
        );
        let (badge, css) = match &submodule {
            Some(_) => ("", None),
            None => badge_style(state, own),
        };
        let badge_label = gtk::Label::builder().label(badge).build();
        if let Some(css) = css {
            badge_label.add_css_class(css);
            label.add_css_class(css);
        }
        if own && state != FileState::Synced {
            badge_label.add_css_class("dim-label");
        }
        badge_label
            .set_tooltip_text(own.then(|| policy_words(node.is_dir, state == FileState::Synced)));
        // A folder that syncs by policy says so on its icon too, and one
        // inside it names the folder whose rule it is.
        if node.is_dir && state == FileState::Synced {
            let said = match rel.as_deref().and_then(|rel| {
                self.sync_ignored
                    .borrow()
                    .rule_folder(rel)
                    .map(Path::to_path_buf)
            }) {
                Some(folder) if !own => format!(
                    "Synced with Personal by the policy on {}/",
                    folder.display()
                ),
                _ => policy_words(true, true).to_string(),
            };
            icon.set_tooltip_text(Some(&said));
        }

        let mark = crate::sync_mark::SyncMark::new();
        mark.set_row(&row);
        row.append(&icon);
        row.append(&label);
        row.append(&mark.widget);
        row.append(&badge_label);
        if let Some(head) = &submodule {
            let standing = format!("{name} is {}", head.pin.describe());
            row.set_tooltip_text(Some(&standing));
            if head.pin.is_problem() && !self.folder_syncing.get() {
                let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
                warning.add_css_class("warning");
                warning.set_tooltip_text(Some(&standing));
                row.append(&warning);
            }
        }

        // Search annotations: per-file match count; in ghost mode the
        // non-matching rows stay but fade.
        if let Some(view) = self.search_view.borrow().as_ref() {
            if !node.is_dir {
                let literal = view.counts.get(&node.path).copied().unwrap_or(0);
                let meaning = view
                    .meaning
                    .get(&node.path)
                    .map(|(places, _)| *places)
                    .unwrap_or(0);
                if let Some(pills) = crate::search::pills(crate::search::Counts {
                    literal,
                    meaning,
                    inside: 0,
                }) {
                    row.append(&pills);
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
        let watched = self.watching.borrow().as_ref().map(|aim| aim.env.clone());
        let safe_mode = !self.workspace.exec.is_container();
        let lock_reason = match watched {
            Some(env) => Some(format!(
                "Read-only: {env}'s checkout. Its agent is working here — review \
                 what it publishes, or take over its chat."
            )),
            None if safe_mode
                && !taste_core::policy::write_allowed(&self.policy_root(), true, &node.path) =>
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
            let handle = RowHandle {
                is_dir: node.is_dir,
                state: std::cell::Cell::new(state),
                locked,
                label: label.downgrade(),
                badge: badge_label.downgrade(),
                own: std::cell::Cell::new(own),
                mark,
            };
            // A submodule's row wears no state of its own here.
            if submodule.is_none() {
                if let Some(rel) = rel.as_deref() {
                    let moving = self.moving.borrow();
                    let rest = self.status_at_rest(rel, state, node.is_dir);
                    handle.show_mark(moving.get(rel), rest.as_deref());
                }
            }
            rows.insert(node.path.clone(), handle);
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

        // Dragged out, to a browser's upload, a file manager, or the
        // composer (`file_drag`): the file on this host where the folder
        // has it, a copy out of the VM made at the drop where it does not.
        {
            let source = gtk::DragSource::builder()
                .actions(gtk::gdk::DragAction::COPY)
                .build();
            let tree = Rc::downgrade(self);
            let node = node.clone();
            source.connect_prepare(move |_, _, _| {
                let tree = tree.upgrade()?;
                Some(crate::file_drag::provider(tree.drag_source_of(&node)?))
            });
            let dragged = row.downgrade();
            source.connect_drag_begin(move |source, _| {
                if let Some(row) = dragged.upgrade() {
                    source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 0);
                }
            });
            row.add_controller(source);
        }
        row
    }

    /// What dragging `node` out carries: the path on this host when there
    /// is one — the folder itself on a checkout that is local, the folder's
    /// mirror of the primary's checkout for what git does not ignore — or
    /// the file in the checkout, to be copied at the drop. `None` for a
    /// folder this host does not have, which a drop would have to copy
    /// whole.
    fn drag_source_of(&self, node: &FileNode) -> Option<crate::file_drag::Source> {
        use crate::file_drag::Source;
        if node.ghost || node.create.is_some() {
            return None;
        }
        let watching = self.watching.borrow().is_some();
        if !watching && self.workspace.files().is_local() {
            return Some(Source::Host(node.path.clone()));
        }
        if !watching && self.state_of(node) != FileState::Ignored {
            let rel = node.path.strip_prefix(self.view_root()).ok()?;
            return Some(Source::Host(self.workspace.root().join(rel)));
        }
        (!node.is_dir).then(|| Source::Remote {
            files: self.worktree().files(),
            path: node.path.clone(),
        })
    }

    /// A ghost: config the workspace could have, drawn faint, one activation
    /// away from existing.
    fn build_ghost_row(self: &Rc<Self>, node: &FileNode) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // The listing's own type, like every other row: a ghost at the
        // panel's size stood a point taller than the files around it
        // (David, 2026-09-16: "Are these the same font size?").
        row.add_css_class("file-row");
        if let Some(create) = node.create {
            let (icon, words) = match create {
                Create::File => ("document-new-symbolic", "New file…"),
                Create::Folder => ("folder-new-symbolic", "New folder…"),
            };
            let icon = gtk::Image::from_icon_name(icon);
            icon.add_css_class("dim-label");
            row.append(&icon);
            row.append(
                &gtk::Label::builder()
                    .label(words)
                    .xalign(0.0)
                    .hexpand(true)
                    .css_classes(["dim-label", "ghost-text"])
                    .build(),
            );
            return row;
        }
        let icon = gtk::Image::from_icon_name(if node.is_dir {
            "folder-new-symbolic"
        } else {
            "list-add-symbolic"
        });
        icon.add_css_class("dim-label");
        // Named as its neighbours are: a ghost inside a folder is a row of
        // that folder and shows its file name; one at the root shows its
        // path from the root, which for a root file is the same thing.
        let rel = node
            .path
            .strip_prefix(self.view_root())
            .unwrap_or(&node.path)
            .display()
            .to_string();
        let rel = match node.path.parent() {
            Some(parent) if parent != self.view_root() => node
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(rel),
            _ => rel,
        };
        let rel = if node.is_dir { format!("{rel}/") } else { rel };
        let label = gtk::Label::builder()
            .label(&rel)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "ghost-text"])
            .build()
            .full_text_on_hover();
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
        if self.git.borrow().is_none() {
            return;
        }
        let worktree = self.worktree();
        // Index writes are IO: off the main thread like every other git op.
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            let toggle_rel = rel.clone();
            let result = tokio::task::spawn_blocking(move || {
                if currently_staged {
                    worktree.unstage(&toggle_rel).map_err(|e| e.to_string())
                } else {
                    worktree.stage(&toggle_rel).map_err(|e| e.to_string())
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

    /// Commit the index with `message` — the universal composer's Commit.
    /// `Err` is a refusal before anything is written; the write itself is
    /// off the main thread, and a failure there is a toast.
    pub fn commit_staged(self: &Rc<Self>, message: &str) -> Result<(), String> {
        if self.refuse_read_only() {
            return Err("this checkout is read-only".into());
        }
        if self.git.borrow().is_none() {
            return Err("this workspace is not a git repository".into());
        }
        let worktree = self.worktree();
        // Writing the commit is IO: off the main thread; the entry clears
        // only once the commit actually exists.
        let events = self.workspace.events.clone();
        let message = message.to_string();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                worktree
                    .commit(&message)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            });
            let Ok(result) = handle.await else { return };
            match result {
                Ok(()) => events.publish(Event::GitStatusChanged),
                Err(e) => events.publish(Event::Toast(format!("Commit failed: {e}"))),
            }
        });
        Ok(())
    }

    /// TASTE_PROBE_CHECK only: `TASTE_PROBE_CARD=push` (or `pull`,
    /// `canon`) shows that button's hover card once the counts have
    /// landed, as a hover would: a shot has no pointer to hover with.
    fn probe_card(self: &Rc<Self>) {
        if std::env::var("TASTE_PROBE_CHECK").is_err() {
            return;
        }
        // `TASTE_PROBE_ALIGN=1`: where the header rows' icons and text
        // start, once — the folder's, the branch's, and All's, buttons all
        // three, which stand in one column.
        static ALIGN: std::sync::Once = std::sync::Once::new();
        if std::env::var("TASTE_PROBE_ALIGN").is_ok() && !ALIGN.is_completed() {
            ALIGN.call_once(|| {});
            let pane = self.widget.clone();
            let (root, branch, all) = (
                self.root_label.clone(),
                self.branch_child.clone(),
                self.all_face.clone(),
            );
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                for (name, label) in [("root", root), ("branch", branch), ("all", all)] {
                    let icon = label.prev_sibling();
                    let x = |w: &gtk::Widget| w.compute_bounds(&pane).map(|b| b.x());
                    println!(
                        "align {name}: icon {:?} text {:?}",
                        icon.as_ref().and_then(&x),
                        x(label.upcast_ref())
                    );
                }
            });
        }
        // `TASTE_PROBE_BRANCHES=1`: the branch switcher open, once, as a
        // click on the branch leaves it — a popover, which a shot has no
        // pointer to open.
        static BRANCHES: std::sync::Once = std::sync::Once::new();
        if std::env::var("TASTE_PROBE_BRANCHES").is_ok() && !BRANCHES.is_completed() {
            BRANCHES.call_once(|| {});
            let button = self.branch_label.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                button.popup();
            });
        }
        // `TASTE_PROBE_NAME_PROMPT=1`: the New file dialog opened as its
        // row opens it, and a second later what holds the keyboard — the
        // name field, or something a typed name would not reach.
        static NAME_PROMPT: std::sync::Once = std::sync::Once::new();
        if std::env::var("TASTE_PROBE_NAME_PROMPT").is_ok() && !NAME_PROMPT.is_completed() {
            NAME_PROMPT.call_once(|| {});
            let tree = self.clone();
            let root = self.widget.root();
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                tree.prompt_name_or("New File", "Create", "", |_, _| {}, |_| {});
                glib::timeout_add_local_once(std::time::Duration::from_millis(1000), move || {
                    let focus = root.and_then(|root| root.focus());
                    println!(
                        "name prompt: focus on {}",
                        focus
                            .map(|w| {
                                let in_entry = w.ancestor(gtk::Entry::static_type()).is_some()
                                    || w.is::<gtk::Entry>();
                                format!("{} (in the name field: {in_entry})", w.type_().name())
                            })
                            .unwrap_or_else(|| "nothing".to_string())
                    );
                });
            });
        }
        // `TASTE_PROBE_SELECTION=1`: README.md in front, then the first
        // folder above it opened — rows spliced in over the lit one — and
        // the lit row against the file in front, before and after.
        static SELECTION: std::sync::Once = std::sync::Once::new();
        if std::env::var("TASTE_PROBE_SELECTION").is_ok() && !SELECTION.is_completed() {
            SELECTION.call_once(|| {});
            let tree = self.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                let readme = tree.view_root().join("README.md");
                tree.select_for_editor(crate::editor::Focused::File(readme));
                let lit = |tree: &Rc<FileTree>| {
                    tree.tree_selection()
                        .and_then(|(_, selection, _)| selection.selected_item())
                        .and_downcast::<gtk::TreeListRow>()
                        .and_then(|row| row.item().and_downcast::<BoxedAnyObject>())
                        .map(|item| item.borrow::<FileNode>().path.display().to_string())
                };
                println!("selection probe: before, lit {:?}", lit(&tree));
                let folder = tree.tree_selection().and_then(|(_, _, model)| {
                    (0..model.n_items())
                        .filter_map(|i| model.row(i))
                        .find(|row| {
                            row.item()
                                .and_downcast::<BoxedAnyObject>()
                                .is_some_and(|item| item.borrow::<FileNode>().is_dir)
                        })
                });
                if let Some(folder) = folder {
                    folder.set_expanded(true);
                }
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    println!(
                        "selection probe: after a folder above opened, lit {:?}, in front {:?}",
                        lit(&tree),
                        tree.file_in_front
                            .borrow()
                            .as_ref()
                            .map(|p| p.display().to_string())
                    );
                    // Files made above it, and the tree brought current in
                    // place, as a build writing beside the file does.
                    let root = tree.view_root();
                    for name in ["AA.md", "AB.md", "AC.md"] {
                        let _ = std::fs::write(root.join(name), "x");
                    }
                    tree.refresh_tree();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(2500),
                        move || {
                            println!(
                            "selection probe: after files landed above it, lit {:?}, in front {:?}",
                            lit(&tree),
                            tree.file_in_front.borrow().as_ref().map(|p| p.display().to_string())
                        );
                            // A folder lit in the list, as a click on its
                            // arrow leaves it, and the focus taken elsewhere.
                            if let Some((list, selection, model)) = tree.tree_selection() {
                                list.grab_focus();
                                if let Some(at) = (0..model.n_items()).find(|&i| {
                                    model
                                        .row(i)
                                        .and_then(|row| row.item().and_downcast::<BoxedAnyObject>())
                                        .is_some_and(|item| item.borrow::<FileNode>().is_dir)
                                }) {
                                    selection.set_selected(at);
                                }
                            }
                            println!("selection probe: a folder lit, lit {:?}", lit(&tree));
                            tree.sync_button.grab_focus();
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(500),
                                move || {
                                    println!(
                                "selection probe: focus left the tree, lit {:?}, in front {:?}",
                                lit(&tree),
                                tree.file_in_front
                                    .borrow()
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                            );
                                    // An ignored file not shown, in front: its folder opened
                                    // once to look for it, then closed, and the tree refreshed
                                    // — the folder stays closed.
                                    let root = tree.view_root();
                                    let _ = std::fs::create_dir_all(root.join("hidden"));
                                    let _ =
                                        std::fs::write(root.join("hidden/.gitignore"), "*.log\n");
                                    let _ = std::fs::write(root.join("hidden/x.log"), "x");
                                    tree.refresh_tree();
                                    let tree = tree.clone();
                                    glib::timeout_add_local_once(
                                        std::time::Duration::from_millis(1500),
                                        move || {
                                            let hidden = tree.view_root().join("hidden");
                                            tree.select_for_editor(crate::editor::Focused::File(
                                                hidden.join("x.log"),
                                            ));
                                            let open = move |tree: &Rc<FileTree>| {
                                                tree.tree_selection().and_then(|(_, _, model)| {
                                                    (0..model.n_items())
                                                        .filter_map(|i| model.row(i))
                                                        .find(|row| {
                                                            row.item()
                                                                .and_downcast::<BoxedAnyObject>()
                                                                .is_some_and(|item| {
                                                                    item.borrow::<FileNode>().path
                                                                        == hidden
                                                                })
                                                        })
                                                })
                                            };
                                            let tree = tree.clone();
                                            glib::timeout_add_local_once(
                                                std::time::Duration::from_millis(1500),
                                                move || {
                                                    if let Some(row) = open(&tree) {
                                                        println!(
                "selection probe: hidden file in front, its folder open {}",
                row.is_expanded()
            );
                                                        row.set_expanded(false);
                                                    }
                                                    tree.refresh_tree();
                                                    let tree = tree.clone();
                                                    glib::timeout_add_local_once(
                                                        std::time::Duration::from_millis(2000),
                                                        move || {
                                                            println!(
                "selection probe: closed and refreshed, open again {:?}",
                open(&tree).map(|row| row.is_expanded())
            );
                                                        },
                                                    );
                                                },
                                            );
                                        },
                                    );
                                },
                            );
                        },
                    );
                });
            });
        }
        // `TASTE_PROBE_TREE_END=1`: the listing scrolled to its end, where
        // New file… and New folder… are, which a short pane hides.
        if std::env::var("TASTE_PROBE_TREE_END").is_ok() {
            let adjustment = self.list_holder.vadjustment();
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                adjustment.set_value(adjustment.upper());
            });
        }
        let Ok(which) = std::env::var("TASTE_PROBE_CARD") else {
            return;
        };
        // `push:detail` also opens the first commit's detail card.
        let (which, detail) = match which.split_once(':') {
            Some((which, "detail")) => (which.to_string(), true),
            _ => (which, false),
        };
        let card = match which.as_str() {
            "pull" => &self.pull_card,
            "canon" => &self.canon_card,
            "push" => &self.push_card,
            _ => return,
        };
        let Some(card) = card.borrow().clone() else {
            return;
        };
        // Drawn again only when it says something new, as a hovered card
        // is: the counts refresh many times a second under a probe, and a
        // card drawn again at each lost the detail opened beside it.
        thread_local! {
            static LAST: RefCell<Option<SyncCard>> = const { RefCell::new(None) };
        }
        if LAST.with(|last| last.replace(Some(card.clone()))) == Some(card.clone()) {
            return;
        }
        let button: &gtk::Button = match which.as_str() {
            "pull" => &self.pull_button,
            "canon" => &self.canon_button,
            _ => &self.push_button,
        };
        if let Some(layer) = crate::sync_card::Layer::current() {
            let (button, layer) = (button.clone(), layer.clone());
            // After the frame that lays the header out, so the card is
            // placed against where the button is.
            glib::idle_add_local_once(move || {
                layer.show(button.upcast_ref(), &card);
                // After the card has been laid out, as a hover comes after
                // it: the detail stands level with its row.
                if detail {
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(300),
                        move || layer.probe_first_detail(&card),
                    );
                }
            });
        }
    }

    /// Hand the sync row to `button`'s spinner until the operation ends.
    fn begin_sync_op(&self, button: &gtk::Button) {
        self.sync_busy.set(true);
        button_busy(button);
        // The others are meaningless until this one lands.
        for other in [
            &self.push_button,
            &self.pull_button,
            &self.canon_button,
            &self.sync_button,
        ] {
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

    /// Push, from a click or, `held`, a hold: the hold is what a push that
    /// replaces commits on the remote needs.
    fn push(self: &Rc<Self>, held: bool) {
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
        // The branch the working tree is on — for a checkout in a VM, not
        // this repository's HEAD, which a plain `git push` here would have
        // pushed instead — pushed by name, to its upstream or, when it has
        // none, to the same name on the remote.
        let branch = self.status_branch.borrow().clone();
        // Read off the main thread: matching a rewrite compares the
        // commits on both sides change by change (review, 2026-10-20).
        let root = self.refs_root();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let read_branch = branch.clone();
            let sync = crate::runtime::runtime()
                .spawn_blocking(move || {
                    let git = taste_git::GitWorkspace::discover(&root)?;
                    match &read_branch {
                        Some(branch) => git.sync_status_of(branch).ok(),
                        None => git.sync_status().ok(),
                    }
                })
                .await
                .ok()
                .flatten();
            if let Some(tree) = weak.upgrade() {
                tree.push_as(held, sync);
            }
        });
    }

    /// [`Self::push`] once the branch's standing is read: held for, refused
    /// with the hold that would do it, or pushed.
    fn push_as(self: &Rc<Self>, held: bool, sync: Option<taste_git::SyncStatus>) {
        // A branch rebased onto a base its push target is not — a fork's
        // pull request after Sync — replaces what the push target has.
        // That is the push the workflow wants, and it overwrites commits on
        // a remote, so it is held for, as every destructive button here is
        // (David, 2026-10-15: "It should change to hold semantics"), a
        // click saying so by name; and it is leased on the tip last seen
        // there so commits someone pushed since are refused rather than
        // lost. Without a base the same divergence means the push target
        // moved: Pull first, and git's own refusal says so.
        // A branch rewritten here — rebased, reworded, amended — whose
        // push target still has the old copies is the same push without a
        // base (`SyncStatus::rewritten`): its divergence is its own, and
        // Pull would only replay it onto itself (2026-10-19).
        let replacing = sync.as_ref().and_then(replacing_push);
        // The default branch's checkout: every push of it is held for, a
        // fast-forward too, and one replacing history there for longer
        // (David, 2026-10-19).
        if !held && sync.as_ref().is_some_and(|s| s.on_canonical) {
            let target = sync
                .as_ref()
                .and_then(|s| s.upstream.clone())
                .unwrap_or_else(|| "the remote".to_string());
            self.workspace.events.publish(Event::Toast(format!(
                "Not pushed: {target} is the default branch. Hold Push for {} seconds to {}",
                self.push_hold.hold().as_secs(),
                if replacing.is_some() {
                    "replace its history there"
                } else {
                    "push to it"
                }
            )));
            return;
        }
        match replacing {
            Some((_, replaced)) if !held => {
                let target = sync
                    .as_ref()
                    .and_then(|s| s.upstream.clone())
                    .unwrap_or_else(|| "the remote".to_string());
                let what = match sync.as_ref().and_then(|s| s.rewritten) {
                    Some(dropped) => rewrite_verdict(replaced, dropped, &target),
                    None => format!(
                        "{target} has {replaced} commit{} with no match in this branch: from \
                         before a rebase onto the base, or someone else's",
                        if replaced == 1 { "" } else { "s" }
                    ),
                };
                self.workspace.events.publish(Event::Toast(format!(
                    "Not pushed: {what}. Hold Push for {} seconds to replace {}; it is \
                     refused if {target} has changed since it was last fetched",
                    self.push_hold.hold().as_secs(),
                    if replaced == 1 { "it" } else { "them" },
                )));
            }
            Some((expected, _)) => {
                self.begin_sync_op(&self.push_button);
                self.push_now(Some(expected), sync);
            }
            None => {
                self.begin_sync_op(&self.push_button);
                self.push_now(None, sync);
            }
        }
    }

    /// The push itself, replacing what the push target has when `expected`
    /// is its tip (the lease), an ordinary push otherwise.
    fn push_now(self: &Rc<Self>, expected: Option<String>, sync: Option<taste_git::SyncStatus>) {
        let branch = self.status_branch.borrow().clone();
        let Some((program, args, remote)) = self.git.borrow().as_ref().map(|git| {
            let (program, args) = match &branch {
                Some(branch) => {
                    git.push_branch_command_including_issues(branch, expected.as_deref())
                }
                None => git.push_command_including_issues(),
            };
            let remote = match &branch {
                Some(branch) => git.push_url_of(branch),
                None => git.upstream_remote_url(),
            };
            (program, args, remote)
        }) else {
            return;
        };
        // What the toast will say: the count and the upstream, read before
        // the push moves them (David, 2026-09-22: "Should say 'Pushed <x>
        // commits to <remote>'").
        let pushed = {
            let ahead = sync.as_ref().map_or(0, |s| s.ahead);
            let upstream = sync
                .as_ref()
                .and_then(|s| s.upstream.clone())
                .unwrap_or_else(|| "the remote".to_string());
            format!(
                "Pushed {ahead} commit{} to {upstream}",
                if ahead == 1 { "" } else { "s" }
            )
        };
        let events = self.workspace.events.clone();
        // The agent is told its branch is published, and its checkout given
        // the remote-tracking ref the push moved: an agent that did not know
        // rewrote the branch and told the user to push it plainly, which
        // the remote refuses (2026-10-19).
        let published = branch.clone().map(|branch| {
            let upstream = sync
                .as_ref()
                .and_then(|s| s.upstream.clone())
                .unwrap_or_else(|| "the remote".to_string());
            format!(
                "The user pushed {branch} to {upstream}. It is published: rewriting its \
                 commits now (rebase, amend, reword, reset) means replacing them on \
                 {upstream}, which only the user's hold-to-replace Push does. Say so before \
                 rewriting it, and never tell them to run a plain git push."
            )
        });
        let note_agent = self.note_agent.borrow().clone();
        let share_remotes = self.share_remotes.borrow().clone();
        // Push runs on the host with the user's own credential helpers.
        let spec = {
            let exec = taste_core::ExecContext::host();
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            exec.resolve(&program, &arg_refs, false)
        };
        let root = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // The submodule commits the branch records go first, each on
            // its own branch (`GitWorkspace::submodule_pushes`); the
            // branch's push only checks they arrived.
            if let Some(branch) = branch.clone() {
                let pushes = {
                    let root = root.clone();
                    crate::runtime::runtime()
                        .spawn_blocking(move || {
                            taste_git::GitWorkspace::discover(&root)
                                .map(|git| git.submodule_pushes(&branch))
                                .unwrap_or(Ok(Vec::new()))
                        })
                        .await
                        .unwrap_or_else(|_| Err("interrupted".to_string()))
                };
                let failed = match pushes {
                    Err(reason) => Some(reason),
                    Ok(pushes) => {
                        let mut failed = None;
                        for push in pushes {
                            let spec = {
                                let exec = taste_core::ExecContext::host();
                                let arg_refs: Vec<&str> =
                                    push.args.iter().map(String::as_str).collect();
                                exec.resolve(&push.program, &arg_refs, false)
                            };
                            let envs = crate::ssh_agent::network_env(root.clone(), push.url).await;
                            let handle = crate::runtime::runtime().spawn(run_git_step(
                                spec.program,
                                spec.args,
                                envs,
                            ));
                            let reason = match handle.await {
                                Ok(Ok(_)) => continue,
                                Ok(Err(reason)) => reason,
                                Err(_) => "interrupted".to_string(),
                            };
                            failed = Some(format!(
                                "the submodule {}'s branch {}: {reason}",
                                push.path.display(),
                                push.branch
                            ));
                            break;
                        }
                        failed
                    }
                };
                if let Some(reason) = failed {
                    events.publish(Event::Toast(format!("Push failed: {reason}")));
                    if let Some(tree) = weak.upgrade() {
                        tree.end_sync_op();
                    }
                    events.publish(Event::GitStatusChanged);
                    return;
                }
            }
            let envs = crate::ssh_agent::network_env(root.clone(), remote).await;
            let handle =
                crate::runtime::runtime().spawn(run_git_step(spec.program, spec.args, envs));
            match handle.await {
                Ok(Ok(_)) => {
                    events.publish(Event::Toast(pushed));
                    if let (Some(note), Some(note_agent)) = (published, note_agent) {
                        note_agent(note);
                    }
                    if let Some(share) = share_remotes {
                        let shared = crate::runtime::runtime()
                            .spawn_blocking(move || share())
                            .await;
                        if let Ok(Err(reason)) = shared {
                            tracing::info!("the pushed ref did not reach the checkouts: {reason}");
                        }
                    }
                }
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
        self.aim_at(Some(Aim {
            env,
            checkout: taste_core::environment::Checkout::Local(root.clone()),
            files: taste_core::files::Files::Local,
            peer: root,
        }));
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
        let _doing = crate::stall::doing(|| "restyling the file tree's rows".into());
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
        let sync_ignored = self.sync_ignored.borrow();
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
            let state = match rel {
                Some(rel) => synced_or(state, &sync_ignored, rel, handle.is_dir),
                None => state,
            };
            let painted = handle.state.get();
            let own = policy_badge(&sync_ignored, rel.as_deref(), state, handle.is_dir);
            if state == painted && own == handle.own.get() {
                continue;
            }
            let (Some(label), Some(badge)) = (handle.label.upgrade(), handle.badge.upgrade())
            else {
                continue;
            };
            // Undone as it was painted: an ignored row synced by its own
            // rule was dimmed by `badge_style`, which `state_style` knows
            // nothing of, and its name stayed dim after (review, 2026-10-20).
            if let (_, Some(old)) = badge_style(painted, handle.own.get()) {
                badge.remove_css_class(old);
                // The lock painted dim-label for its own reason; leaving
                // Ignored must not un-dim a locked row.
                if !(handle.locked && old == "dim-label") {
                    label.remove_css_class(old);
                }
            }
            if handle.own.get() {
                badge.remove_css_class("dim-label");
            }
            let (text, css) = badge_style(state, own);
            badge.set_label(text);
            if own && state != FileState::Synced {
                badge.add_css_class("dim-label");
            }
            badge.set_tooltip_text(
                own.then(|| policy_words(handle.is_dir, state == FileState::Synced)),
            );
            if let Some(css) = css {
                badge.add_css_class(css);
                label.add_css_class(css);
            }
            handle.state.set(state);
            handle.own.set(own);
            if let Some(rel) = rel.as_deref() {
                let moving = self.moving.borrow();
                let rest = status_at_rest_in(&sync_ignored, rel, state, handle.is_dir);
                handle.show_mark(moving.get(rel), rest.as_deref());
            }
        }
    }

    /// The files on their way now: each bound row's mark follows, and
    /// nothing else on the row is touched.
    pub fn on_sync_files(
        &self,
        files: std::sync::Arc<std::collections::BTreeMap<PathBuf, taste_core::SyncFile>>,
    ) {
        let _doing =
            crate::stall::doing(|| format!("the tree's sync marks, {} files", files.len()));
        let before = std::mem::replace(&mut *self.moving.borrow_mut(), files.clone());
        if before.is_empty() && files.is_empty() {
            return;
        }
        let rows = self.rows.borrow();
        for (path, handle) in rows.iter() {
            if handle.is_dir {
                continue;
            }
            let Some(rel) = self.repo_relative(path) else {
                continue;
            };
            let (was, now) = (before.get(&rel), files.get(&rel));
            if was.is_some() || now.is_some() {
                let rest = self.status_at_rest(&rel, handle.state.get(), false);
                handle.show_mark(now, rest.as_deref());
            }
        }
        // ...and the changed files' lists, where the same files are rows.
        for (rel, row) in self.changed_rows.borrow().iter() {
            let (was, now) = (before.get(rel), files.get(rel));
            if was.is_some() || now.is_some() {
                row.mark.show(now, None);
            }
        }
    }

    /// What a file's status says when nothing is moving: in step, for one
    /// that syncs by policy — its own, or a folder's, which is named.
    fn status_at_rest(&self, rel: &Path, state: FileState, is_dir: bool) -> Option<String> {
        status_at_rest_in(&self.sync_ignored.borrow(), rel, state, is_dir)
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

/// Which filter views are on: the shape of the changed-files list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChangedFlags {
    dirty_on: bool,
    staged_on: bool,
    stashed_on: bool,
    conflicts_on: bool,
}

/// A row of the changed-files list, kept so a status tick can leave it be.
struct ChangedRow {
    row: adw::ActionRow,
    check: gtk::CheckButton,
    state: FileState,
    in_stash: bool,
    /// Its sync status, as the tree's row has (`crate::sync_mark`).
    mark: crate::sync_mark::SyncMark,
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
        // In the letters' place: an ignored path wears none of them, and
        // a real state of its own (staged by force, say) is shown instead.
        // The policy mark, when the path's own rule is the reason
        // (`badge_style`).
        FileState::Synced => ("⇅", None),
    }
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_SYNCING=README.md:up:0.4,Cargo.toml:down`
/// poses those files on their way — up to Personal or down to the folder,
/// that far through, or waiting when no fraction is given — since a probe's
/// folder has no VM to sync with. Up is 24 MiB of its own; down is 30 MiB
/// riding a shared fetch, as git's does.
fn syncing_for_probe() -> std::sync::Arc<std::collections::BTreeMap<PathBuf, taste_core::SyncFile>>
{
    let mut files = std::collections::BTreeMap::new();
    if std::env::var_os("TASTE_PROBE_CHECK").is_some() {
        for posed in std::env::var("TASTE_PROBE_SYNCING")
            .unwrap_or_default()
            .split(',')
            .filter(|p| !p.is_empty())
        {
            let mut parts = posed.split(':');
            let path = PathBuf::from(parts.next().unwrap_or_default());
            let up = parts.next() != Some("down");
            let fraction: Option<f64> = parts.next().and_then(|f| f.parse().ok());
            let file = match fraction {
                None => taste_core::SyncFile {
                    up,
                    fraction: None,
                    left: None,
                    moved: 0,
                    seconds_left: None,
                    shared: false,
                },
                Some(f) if up => taste_core::SyncFile {
                    up,
                    fraction: Some(f),
                    left: Some(((1.0 - f) * f64::from(24u32 << 20)) as u64),
                    moved: (f * f64::from(24u32 << 20)) as u64,
                    seconds_left: Some(((1.0 - f) * 20.0).ceil() as u64),
                    shared: false,
                },
                Some(f) => taste_core::SyncFile {
                    up,
                    fraction: Some(f),
                    left: Some(((1.0 - f) * f64::from(30u32 << 20)) as u64),
                    moved: (f * f64::from(30u32 << 20)) as u64,
                    seconds_left: Some(((1.0 - f) * 12.0).ceil() as u64),
                    shared: true,
                },
            };
            files.insert(path, file);
        }
    }
    std::sync::Arc::new(files)
}

/// TASTE_PROBE_CHECK only: `TASTE_PROBE_SYNCED=docs/,README.md` poses those
/// paths as ignored and synced anyway (a trailing slash for a folder),
/// since a folder on this host, which is what a probe opens, never syncs.
fn synced_for_probe() -> Option<taste_devcontainer::worktree::SyncIgnored> {
    std::env::var_os("TASTE_PROBE_CHECK")?;
    let posed = std::env::var("TASTE_PROBE_SYNCED").ok()?;
    let mut shown = taste_devcontainer::worktree::SyncIgnored::default();
    for path in posed.split(',').filter(|p| !p.is_empty()) {
        match path.strip_suffix('/') {
            Some(dir) => {
                shown.toggled.dirs.insert(PathBuf::from(dir));
                shown.ignored_dirs.insert(PathBuf::from(dir));
            }
            None => {
                shown.toggled.files.insert(PathBuf::from(path));
                shown.synced.insert(PathBuf::from(path));
            }
        }
    }
    Some(shown)
}

/// What the synced mark means, on the mark; the letters say it themselves.
/// The badge a row wears: its git letter, or ⇅ for a path that syncs by a
/// rule of its own — the policy mark, rightmost. A path that syncs by its
/// folder's rule leaves the policy to the folder's row.
fn badge_style(state: FileState, own: bool) -> (&'static str, Option<&'static str>) {
    match state {
        FileState::Synced if own => ("⇅", Some("dim-label")),
        FileState::Synced => ("", None),
        // A folder git tracks whose rule syncs the ignored files in it: the
        // policy is set here, and with no letter of its own the folder
        // wears it — dim in the badge alone (`policy_badge`), the name
        // being no ignored thing's.
        FileState::Clean if own => ("⇅", None),
        other => state_style(other),
    }
}

/// Whether a row wears the policy mark ⇅: its own rule is why it syncs
/// (`SyncIgnored::own_rule`) — an ignored file or folder, or a folder git
/// tracks with nothing to say, whose rule syncs the ignored files in it.
/// The rows those sync by it wear the status (`sync_mark.rs`), not this
/// (David, 2026-10-19: "The straight up/down should show where the policy
/// is set, which is the folder in this case").
fn policy_badge(
    sync_ignored: &taste_devcontainer::worktree::SyncIgnored,
    rel: Option<&Path>,
    state: FileState,
    is_dir: bool,
) -> bool {
    let at_rest = state == FileState::Synced || (is_dir && state == FileState::Clean);
    at_rest && rel.is_some_and(|rel| sync_ignored.own_rule(rel, is_dir))
}

/// What the policy mark means: on an ignored file or folder, that its
/// rule syncs it anyway; on a folder git tracks, that its rule syncs the
/// ignored files in it.
fn policy_words(is_dir: bool, ignored: bool) -> &'static str {
    match (is_dir, ignored) {
        (true, true) => {
            "Synced by policy: git ignores this folder, and its taste-sync rule in \
             .gitattributes syncs it with Personal anyway"
        }
        (true, false) => {
            "Sync policy: this folder's taste-sync rule in .gitattributes syncs the \
             ignored files in it with Personal"
        }
        _ => {
            "Synced by policy: git ignores this file, and its taste-sync rule in \
             .gitattributes syncs it with Personal anyway"
        }
    }
}

/// [`FileTree::status_at_rest`], against `sync_ignored`.
fn status_at_rest_in(
    sync_ignored: &taste_devcontainer::worktree::SyncIgnored,
    rel: &Path,
    state: FileState,
    is_dir: bool,
) -> Option<String> {
    if is_dir || state != FileState::Synced {
        return None;
    }
    Some(match sync_ignored.rule_folder(rel) {
        Some(folder) if !sync_ignored.own_rule(rel, false) => format!(
            "In step with Personal: synced by the policy on {}/",
            folder.display()
        ),
        _ => "In step with Personal".to_string(),
    })
}

/// A row's state, or the synced mark when git has nothing to say about it
/// and it is ignored but syncs anyway (`taste_git::sync_ignored`).
fn synced_or(
    state: FileState,
    sync_ignored: &taste_devcontainer::worktree::SyncIgnored,
    rel: &Path,
    is_dir: bool,
) -> FileState {
    if state == FileState::Clean && sync_ignored.shows(rel, is_dir) {
        FileState::Synced
    } else {
        state
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
        FileState::Clean | FileState::Ignored | FileState::Synced => 0,
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

/// How many files a query reached: those it reached by the word, plus —
/// while the meaning toggle is on — those the semantic index found,
/// counted once each.
///
/// A union rather than a sum, because the lozenge counts FILES and the two
/// answers overlap: a file whose name matches and whose contents do is one
/// file, and so is a file the word and the idea both reach. The number was
/// `content.len() + by_name.len()` and left the index's answer out
/// entirely.
fn files_reached(
    matched: &HashSet<PathBuf>,
    meaning: &[crate::search::MeaningHit],
    with_meaning: bool,
) -> usize {
    if !with_meaning {
        return matched.len();
    }
    matched
        .iter()
        .chain(meaning.iter().map(|hit| &hit.path))
        .collect::<HashSet<&PathBuf>>()
        .len()
}

/// List one directory as a ListStore of `FileNode`s, honoring .gitignore
/// (as `showing` says), directories first, then case-insensitive alpha.
/// `ghosts` (workspace root only) are appended last as creation suggestions.
/// Active search results shaped for the tree: per-file hits, and the set
/// of paths (matching files + their ancestor directories) that stay
/// visible in matches-only mode.
/// What one content search found, for the editor's listing: the files with
/// their capped lines and complete counts, the definitions whose names
/// match, and the commits whose messages do.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchView {
    /// Per file, the first `MATCH_LINES_PER_FILE` matching lines.
    hits: HashMap<PathBuf, Vec<taste_core::search::SearchHit>>,
    /// Per file, EVERY match — the number the row wears, which `hits`
    /// alone would understate for a file with more than it keeps.
    counts: HashMap<PathBuf, usize>,
    /// Per file found by MEANING only: how many chunks, and the first
    /// one's line — where a click opens it.
    meaning: HashMap<PathBuf, (usize, u32)>,
    visible: Rc<HashSet<PathBuf>>,
    /// The editor's active file: always listed (even with zero hits), so
    /// project search doubles as search-within-the-current-file.
    pinned: Option<PathBuf>,
}

/// The directory walk itself: `ignore::WalkBuilder` reads `.gitignore` and
/// touches the filesystem for every entry, so this is the part that must
/// run off the main thread — plain data in, plain data (`Send`) out, no
/// GTK type anywhere near it. [`FileTree::rebuild`] and the
/// `TreeListModel` create-func both call this from `spawn_blocking` and
/// splice the result into a `ListStore` back on the main thread, because
/// the `ListStore` itself, like every GTK object, cannot be touched off
/// it.
fn scan_dir_nodes(
    worktree: &Worktree,
    dir: &Path,
    showing: Showing,
    synced: &taste_devcontainer::worktree::SyncIgnored,
    ghosts: &[Ghost],
    creators: bool,
    filter: Option<&HashSet<PathBuf>>,
) -> Vec<FileNode> {
    let mut nodes: Vec<FileNode> = if worktree.is_local() {
        let mut walk = ignore::WalkBuilder::new(dir);
        walk.max_depth(Some(1)).hidden(false);
        if showing == Showing::All {
            walk.git_ignore(false).git_exclude(false).parents(false);
        }
        walk.build()
            .flatten()
            .filter(|entry| entry.path() != dir)
            .filter(|entry| entry.file_name() != ".git")
            .map(|entry| FileNode {
                is_dir: entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
                path: entry.into_path(),
                ghost: false,
                create: None,
            })
            .collect()
    } else {
        // Beside the files: one listing, and one `git check-ignore` for
        // the names in it, which is git's own answer to what the walk
        // above decides from `.gitignore` here.
        let entries = worktree.files().list(dir).unwrap_or_default();
        let names: Vec<PathBuf> = entries
            .iter()
            .filter(|entry| entry.name != ".git")
            .map(|entry| dir.join(&entry.name))
            .collect();
        let ignored = if showing == Showing::All {
            HashSet::new()
        } else {
            worktree.ignored(&names).unwrap_or_default()
        };
        entries
            .into_iter()
            .filter(|entry| entry.name != ".git")
            .map(|entry| FileNode {
                is_dir: entry.kind == taste_core::files::Kind::Dir,
                path: dir.join(&entry.name),
                ghost: false,
                create: None,
            })
            // Ignored and hidden — unless it syncs anyway, which makes it
            // part of both working copies: listed, wearing its mark
            // (`taste_git::sync_ignored`; David, 2026-10-11: "Shouldn't I
            // see the data folder?").
            .filter(|node| {
                !ignored.contains(&node.path)
                    || (showing == Showing::TrackedAndSynced
                        && node
                            .path
                            .strip_prefix(worktree.root())
                            .is_ok_and(|rel| synced.shows(rel, node.is_dir)))
            })
            .collect()
    };
    nodes.retain(|node| filter.map(|f| f.contains(&node.path)).unwrap_or(true));
    nodes.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            a.path
                .file_name()
                .map(|n| n.to_ascii_lowercase())
                .cmp(&b.path.file_name().map(|n| n.to_ascii_lowercase()))
        })
    });
    // Only the ghosts that belong HERE: `.devcontainer/devcontainer.json`
    // is a row inside the folder once the folder exists, not a row at the
    // root spelling its path.
    nodes.extend(
        ghosts
            .iter()
            .filter(|ghost| ghost.path.parent() == Some(dir))
            .map(|ghost| FileNode {
                path: ghost.path.clone(),
                is_dir: ghost.is_dir,
                ghost: true,
                create: None,
            }),
    );
    // Last of all, in every folder the tree can write in: a new file and a
    // new folder, named when chosen. Not over a search or a filter, whose
    // listings are of what is there.
    if creators && filter.is_none() {
        nodes.extend([Create::File, Create::Folder].map(|create| FileNode {
            path: dir.join(create.placeholder()),
            is_dir: false,
            ghost: true,
            create: Some(create),
        }));
    }
    nodes
}

/// Bring a store that already backs rows up to a fresh listing, touching
/// only what changed: entries that are gone (or changed kind — a ghost
/// that became a file, a file that became a folder) are removed, entries
/// that are new are inserted where the listing's order puts them, and
/// everything else is left as the very same object — which is what keeps
/// a `TreeListRow`'s expansion, and the row on screen, exactly where it
/// was. Both sides are in [`scan_dir_nodes`]'s order, so after the
/// removals one forward merge places every insertion.
fn reconcile_store(store: &gtk::gio::ListStore, nodes: Vec<FileNode>) {
    let wanted: HashMap<PathBuf, (bool, bool)> = nodes
        .iter()
        .map(|node| (node.path.clone(), (node.is_dir, node.ghost)))
        .collect();
    let mut index = store.n_items();
    while index > 0 {
        index -= 1;
        let keep = store
            .item(index)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|item| {
                let node = item.borrow::<FileNode>();
                wanted.get(&node.path) == Some(&(node.is_dir, node.ghost))
            });
        if !keep {
            store.remove(index);
        }
    }
    for (at, node) in nodes.into_iter().enumerate() {
        let at = at as u32;
        let present = store
            .item(at)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|item| item.borrow::<FileNode>().path == node.path);
        if !present {
            store.insert(at, &BoxedAnyObject::new(node));
        }
    }
}

/// Kick off [`scan_dir_nodes`] on the blocking pool and splice its result
/// into `store` when it lands. `store` is handed back to the caller EMPTY
/// and immediately usable — the model it backs can be attached to a
/// `ListView` before a single entry has been read off disk — and is
/// addressed weakly here so a store that stopped being anyone's model
/// (superseded by a newer `rebuild()`, or a `TreeListRow` collapsed and
/// dropped before its children loaded) is just skipped rather than kept
/// alive by a stray filesystem read.
fn fill_dir_store_async(
    store: &gtk::gio::ListStore,
    worktree: Worktree,
    dir: PathBuf,
    showing: Showing,
    synced: std::sync::Arc<taste_devcontainer::worktree::SyncIgnored>,
    ghosts: Ghosts,
    filter: Option<HashSet<PathBuf>>,
) {
    let weak = store.downgrade();
    let handle = crate::runtime::runtime().spawn_blocking(move || {
        // A pane that can write — the ghosts' root is set — offers the
        // create rows too.
        let creators = ghosts.writable();
        let ghosts = ghosts.get(&worktree);
        scan_dir_nodes(
            &worktree,
            &dir,
            showing,
            &synced,
            ghosts,
            creators,
            filter.as_ref(),
        )
    });
    glib::spawn_future_local(async move {
        let Ok(nodes) = handle.await else { return };
        let Some(store) = weak.upgrade() else { return };
        // In one splice: an append a row was a change a row, each passed up
        // through the tree's model, its selection, the list, and the
        // tree's own handler, which took a folder of thousands of files
        // seconds to open with the window held (David, 2026-10-13:
        // "Expanding large directories in the file view also hangs the
        // GUI").
        let items: Vec<BoxedAnyObject> = nodes.into_iter().map(BoxedAnyObject::new).collect();
        store.splice(store.n_items(), 0, &items);
    });
}

/// Why a status query came back empty-handed.
enum StatusFail {
    /// A checkout on this host that is not a repository.
    NotRepo,
    /// A checkout in a VM whose files service does not answer.
    Unreachable,
    /// git, beside a checkout in a VM, refused; what it said.
    Git(String),
}

/// How long git may refuse a VM checkout's status before the header says
/// so: past a container hook's moment, short of a real fault going unsaid.
const GIT_REFUSAL_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// git's refusal as one line for the header: its own words, the `fatal:`
/// it opens on dropped, cut to a header's length.
fn single_line_note(said: &str) -> String {
    let line = said.lines().next().unwrap_or(said).trim();
    let line = line.strip_prefix("fatal: ").unwrap_or(line);
    let mut short: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        short.push('…');
    }
    short
}

/// The ghost rows of one rebuild, found once and shared by every listing
/// it makes — the root's and each folder's as it opens — on whichever
/// blocking thread asks first (`fill_dir_store_async`), never on the main
/// one. `None` for a view that offers none.
#[derive(Clone)]
struct Ghosts(std::sync::Arc<(Option<PathBuf>, std::sync::OnceLock<Vec<Ghost>>)>);

impl Ghosts {
    fn new(root: Option<PathBuf>) -> Self {
        Self(std::sync::Arc::new((root, std::sync::OnceLock::new())))
    }

    /// Whether the pane these ghosts are for can write: a read-only view
    /// has no root for them.
    fn writable(&self) -> bool {
        self.0 .0.is_some()
    }

    fn get(&self, worktree: &Worktree) -> &[Ghost] {
        let (root, found) = &*self.0;
        found.get_or_init(|| match root {
            Some(root) => ghost_candidates(&worktree.files(), root),
            None => Vec::new(),
        })
    }
}

/// A conventional path the workspace lacks, offered as a faint row where
/// it belongs: a file, or the `.devcontainer/` folder itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ghost {
    path: PathBuf,
    is_dir: bool,
}

/// Allowlisted config paths the workspace doesn't have yet — shown as
/// ghosts, each in the directory it belongs to. All of them are within the
/// safe-mode writable scope, so creation is legitimate in either mode.
fn ghost_candidates(files: &Files, root: &Path) -> Vec<Ghost> {
    // Shared with the MCP `ide_conventions` tool: one source of truth for
    // the conventional locations — asked of the checkout wherever it is.
    taste_core::conventions::conventions_via(files, root)
        .into_iter()
        .filter(|c| c.ghost && !c.exists)
        .map(|c| Ghost {
            path: c.path,
            is_dir: c.is_dir,
        })
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
        // taskfile.dev: the IDE lists these under Tasks and runs them in
        // the environment, which needs `task` installed there.
        Some("Taskfile.yml") => {
            "# The project's named commands (https://taskfile.dev). taste-ide lists them\n\
             # under Tasks and runs them in the environment, where `task` must be\n\
             # installed (see https://taskfile.dev/installation).\n\
             version: '3'\n\
             \n\
             tasks:\n  \
               default:\n    \
                 desc: Build the project\n    \
                 cmds:\n      \
                   - echo \"say what building means here\"\n"
        }
        Some("devcontainer.json") => {
            "{\n    // Define the project's devcontainer. taste-ide validates this\n    \
             // config (no privileged flags, mounts stay in the workspace).\n    \
             \"name\": \"dev\",\n    \
             \"image\": \"registry.fedoraproject.org/fedora:44\",\n    \
             // Services: forwardPorts publishes on localhost (ports >= 1024).\n    \
             // \"forwardPorts\": [8080],\n    \
             // Caches: named volumes only.\n    \
             // \"mounts\": [\"source=build-cache,target=/cache,type=volume\"],\n    \
             // Background services: a systemd-capable image plus\n    \
             // \"runArgs\": [\"--systemd=always\"] and\n    \
             // \"overrideCommand\": false\n    \
             // To build the image instead of pulling one, replace \"image\" with\n    \
             // \"build\": { \"dockerfile\": \"Containerfile\" }\n}\n"
        }
        Some("Containerfile") => {
            "# The devcontainer's image. Referenced from devcontainer.json as\n\
             #   \"build\": { \"dockerfile\": \"Containerfile\" }\n\
             # in place of \"image\". Rootless podman builds it; keep it to the\n\
             # tools the project needs, and let the IDE's baseline cover the rest.\n\
             FROM registry.fedoraproject.org/fedora:44\n\n\
             RUN dnf install -y git && dnf clean all\n"
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

    /// A folded heading's lights are what is under it — every task in the
    /// namespace and deeper, the heading's own run aside — in a traffic
    /// light's order, each counted.
    #[test]
    fn a_folded_heading_badges_what_it_hides() {
        use crate::tasks::RunState;
        let names: Vec<String> = ["serve", "serve:e2e", "serve:api", "serve:deep:x", "other"]
            .map(String::from)
            .to_vec();
        let states = [
            RunState::Failed,
            RunState::Succeeded,
            RunState::Running,
            RunState::Succeeded,
            RunState::Failed,
        ];
        let outline = crate::tasks::outline(&names);
        let at = outline
            .iter()
            .position(|entry| entry.folds && entry.path == "serve")
            .unwrap();
        let below = super::subtree_states(&outline, at, &states);
        assert_eq!(
            below,
            super::FoldStates {
                failed: 0,
                running: 1,
                succeeded: 2
            }
        );
        assert_eq!(
            below
                .lights()
                .into_iter()
                .map(|(class, _)| class)
                .collect::<Vec<_>>(),
            ["amber", "green"]
        );
        assert_eq!(
            super::fold_words(false, below),
            " · 1 running · 2 succeeded"
        );
        assert_eq!(super::fold_words(true, below), "");
    }

    /// A rewrite whose every change is in the branch says nothing is
    /// lost; one that drops changes says how many.
    #[test]
    fn a_rewrite_says_whether_anything_is_lost() {
        let kept = super::rewrite_verdict(2, 0, "origin/datum-call");
        assert!(kept.contains("nothing is lost"), "{kept}");
        assert!(
            kept.contains("the 2 commits on origin/datum-call are"),
            "{kept}"
        );
        let lost = super::rewrite_verdict(2, 1, "origin/topic");
        assert!(lost.contains("1 of them makes a change"), "{lost}");
        assert!(!lost.contains("nothing is lost"), "{lost}");
    }

    /// Ten seconds only to replace the default branch's history; a pull
    /// request's own branch is replaced with the ordinary hold.
    #[test]
    fn only_the_default_branch_holds_long() {
        use crate::holdbutton::{HOLD, LONG_HOLD};
        assert_eq!(super::push_hold_for(true, true), LONG_HOLD);
        assert_eq!(super::push_hold_for(true, false), HOLD);
        assert_eq!(super::push_hold_for(false, true), HOLD);
        assert_eq!(super::push_hold_for(false, false), HOLD);
    }

    use super::files_reached;

    /// git's refusal reads as git's, one line, its `fatal:` dropped.
    #[test]
    fn a_git_refusal_is_one_line_in_its_own_words() {
        assert_eq!(
            super::single_line_note(
                "fatal: not a git repository: /workspace/.git/modules/amutable-typst-template\nmore"
            ),
            "not a git repository: /workspace/.git/modules/amutable-typst-template"
        );
        assert!(super::single_line_note(&"x".repeat(200)).ends_with('…'));
    }

    /// A ghost `.devcontainer/` opens onto ghosts of the files it is for:
    /// listed as the tree lists a folder it opens, the folder that is not
    /// there yet holds `devcontainer.json` and the Containerfile, and the
    /// root holds the folder (2026-10-05).
    /// An ignored folder that syncs anyway is listed though ignored files
    /// are hidden, and so is what syncs inside it; another ignored folder
    /// stays hidden. Through the files service's git, as a checkout in a VM
    /// is listed.
    #[test]
    fn an_ignored_folder_that_syncs_is_listed() {
        let root =
            std::env::temp_dir().join(format!("taste-synced-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "data/\nbuild/\n").unwrap();
        std::fs::create_dir_all(root.join("data/raw")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("data/raw/a.csv"), "1").unwrap();
        std::fs::write(root.join("README.md"), "r").unwrap();
        let worktree = super::Worktree::Remote {
            files: taste_core::files::Files::Local,
            path: root.clone(),
            after_ref_change: None,
        };
        let mut synced = taste_devcontainer::worktree::SyncIgnored::default();
        synced.toggled.dirs.insert(std::path::PathBuf::from("data"));
        synced.ignored_dirs.insert(std::path::PathBuf::from("data"));
        let names = |dir: &std::path::Path| -> Vec<String> {
            let mut names: Vec<String> = super::scan_dir_nodes(
                &worktree,
                dir,
                super::Showing::TrackedAndSynced,
                &synced,
                &[],
                false,
                None,
            )
            .into_iter()
            .map(|node| {
                node.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
            names.sort();
            names
        };
        assert_eq!(names(&root), [".gitignore", "README.md", "data"]);
        assert_eq!(names(&root.join("data")), ["raw"]);
        // Nothing syncs: the folder is hidden, as any ignored one is.
        let none = taste_devcontainer::worktree::SyncIgnored::default();
        let listed: Vec<String> = super::scan_dir_nodes(
            &worktree,
            &root,
            super::Showing::TrackedAndSynced,
            &none,
            &[],
            false,
            None,
        )
        .into_iter()
        .map(|node| {
            node.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
        assert!(!listed.contains(&"data".to_string()), "{listed:?}");
        // The listing menu's other two: git's files alone leave out even
        // what syncs; everything lists what nothing syncs too.
        let named = |showing: super::Showing, sync: &taste_devcontainer::worktree::SyncIgnored| {
            super::scan_dir_nodes(&worktree, &root, showing, sync, &[], false, None)
                .into_iter()
                .map(|node| {
                    node.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<String>>()
        };
        assert!(!named(super::Showing::Tracked, &synced).contains(&"data".to_string()));
        assert!(named(super::Showing::All, &none).contains(&"data".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every folder a writable pane lists ends with New file… and New
    /// folder…; a read-only pane's, and a search's, do not.
    #[test]
    fn every_folder_ends_with_a_new_file_and_a_new_folder() {
        let root = std::env::temp_dir().join(format!("taste-creators-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let worktree = super::Worktree::Local(root.clone());
        let list = |creators: bool, filter: Option<&HashSet<PathBuf>>| {
            super::scan_dir_nodes(
                &worktree,
                &root,
                super::Showing::TrackedAndSynced,
                &Default::default(),
                &[],
                creators,
                filter,
            )
        };
        let nodes = list(true, None);
        let tail: Vec<_> = nodes.iter().rev().take(2).map(|n| n.create).collect();
        assert_eq!(
            tail,
            [Some(super::Create::Folder), Some(super::Create::File)]
        );
        assert!(nodes.iter().rev().take(2).all(|n| n.ghost && !n.is_dir));
        assert!(list(false, None).iter().all(|n| n.create.is_none()));
        let only = HashSet::from([root.join("a.txt")]);
        assert!(list(true, Some(&only)).iter().all(|n| n.create.is_none()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_ghost_devcontainer_folder_opens_onto_its_files() {
        let root = std::env::temp_dir().join(format!("taste-ghosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "# r\n").unwrap();
        let worktree = super::Worktree::Local(root.clone());
        let ghosts = super::ghost_candidates(&taste_core::files::Files::Local, &root);
        let names = |dir: &std::path::Path| -> Vec<(String, bool, bool)> {
            super::scan_dir_nodes(
                &worktree,
                dir,
                super::Showing::TrackedAndSynced,
                &Default::default(),
                &ghosts,
                false,
                None,
            )
            .into_iter()
            .map(|node| {
                (
                    node.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    node.is_dir,
                    node.ghost,
                )
            })
            .collect()
        };
        assert!(names(&root).contains(&(".devcontainer".into(), true, true)));
        let inside = names(&root.join(".devcontainer"));
        assert!(
            inside.contains(&("devcontainer.json".into(), false, true)),
            "{inside:?}"
        );
        assert!(
            inside.contains(&("Containerfile".into(), false, true)),
            "{inside:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignoring_anchors_escapes_and_does_not_repeat() {
        use super::with_ignored;
        use std::path::PathBuf;
        let a = PathBuf::from("services/management/management");
        let b = PathBuf::from("notes #1.txt");
        let out = with_ignored("target/\n/services/management/management\n", &[&a, &b]);
        assert_eq!(
            out,
            "target/\n/services/management/management\n/notes \\#1.txt\n"
        );
        assert_eq!(with_ignored("", &[&a]), "/services/management/management\n");
        assert_eq!(
            with_ignored("dist", &[&a]),
            "dist\n/services/management/management\n"
        );
    }

    fn meaning(paths: &[&str]) -> Vec<crate::search::MeaningHit> {
        paths
            .iter()
            .map(|p| crate::search::MeaningHit {
                path: std::path::PathBuf::from(p),
                start_line: 1,
                end_line: 2,
                score: 0.7,
                text: String::new(),
            })
            .collect()
    }

    #[test]
    fn the_files_count_is_a_union_of_both_ways_of_reaching_a_file() {
        let matched: std::collections::HashSet<std::path::PathBuf> = ["a.rs", "b.rs"]
            .iter()
            .map(std::path::PathBuf::from)
            .collect();

        // The index reaches one the word did not: it counts.
        assert_eq!(files_reached(&matched, &meaning(&["c.rs"]), true), 3);
        // ...and one the word already did: it does not count twice.
        assert_eq!(files_reached(&matched, &meaning(&["a.rs"]), true), 2);
        // Two chunks of one file are still one file.
        assert_eq!(
            files_reached(&matched, &meaning(&["c.rs", "c.rs"]), true),
            3
        );
        // With meaning off the index's answer is not counted at all, even
        // when it has one in hand.
        assert_eq!(files_reached(&matched, &meaning(&["c.rs"]), false), 2);
    }

    use super::*;

    /// The in-place refresh: what stays is the same object, what went is
    /// gone, what came is where the listing puts it, and a changed kind is
    /// a new object. GIO only — no display needed.
    #[test]
    fn reconcile_keeps_rows_that_are_still_there() {
        let node = |name: &str, is_dir: bool, ghost: bool| FileNode {
            create: None,
            path: PathBuf::from("/w").join(name),
            is_dir,
            ghost,
        };
        let store = gtk::gio::ListStore::new::<BoxedAnyObject>();
        for n in [
            node("src", true, false),
            node("a.rs", false, false),
            node("b.rs", false, false),
            node(".editorconfig", false, true),
        ] {
            store.append(&BoxedAnyObject::new(n));
        }
        let kept_dir = store.item(0).unwrap();
        let kept_file = store.item(2).unwrap();

        // b.rs stays, a.rs is deleted, c.rs appears, the ghost became real
        // (so it moves out of the ghosts' tail into the files), and a new
        // folder arrives.
        reconcile_store(
            &store,
            vec![
                node("docs", true, false),
                node("src", true, false),
                node(".editorconfig", false, false),
                node("b.rs", false, false),
                node("c.rs", false, false),
            ],
        );
        let names: Vec<String> = (0..store.n_items())
            .map(|i| {
                let item = store.item(i).and_downcast::<BoxedAnyObject>().unwrap();
                let n = item.borrow::<FileNode>();
                format!(
                    "{}{}",
                    n.path.file_name().unwrap().to_string_lossy(),
                    if n.ghost { "~" } else { "" }
                )
            })
            .collect();
        assert_eq!(names, ["docs", "src", ".editorconfig", "b.rs", "c.rs"]);
        // Identity: the survivors are the objects the rows already hold.
        assert!(store.item(1).unwrap() == kept_dir, "src was replaced");
        assert!(store.item(3).unwrap() == kept_file, "b.rs was replaced");
    }

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
        crate::gtk_test::on_gtk_thread("row widget churn: no display — skipped", || {
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
                    .build()
                    .full_text_on_hover();
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
        });
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-app perf_ -- --ignored --nocapture`
    ///
    /// `rebuild()` and the `TreeListModel` create-func used to run
    /// `ignore::WalkBuilder` (via the old, synchronous `build_dir_store`)
    /// directly on the GTK thread — every rebuild, and every folder
    /// expansion, walked a directory's `.gitignore` state on the frame
    /// clock. `scan_dir_nodes` is that walk, unchanged; `fill_dir_store_async`
    /// is what replaced both call sites, and the number that matters is how
    /// long IT takes to return — the part that still runs inline, before
    /// the walk itself lands on `spawn_blocking`.
    #[test]
    #[ignore]
    fn perf_dir_walk_moved_off_the_create_func() {
        crate::gtk_test::on_gtk_thread("dir walk offload: no display — skipped", || {
            // A directory shaped like the worst case for a single scan: many
            // siblings at one level (`scan_dir_nodes` never recurses — depth
            // is 1 — so this is exactly what one `rebuild()` or one folder
            // expansion pays for). Under the OS temp dir rather than a crate
            // dependency: no fixture-repo tooling is needed for a flat pile of
            // files.
            let dir = std::env::temp_dir().join(format!(
                "taste-filetree-perf-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).expect("create fixture dir");
            const ENTRIES: usize = 4000;
            for i in 0..ENTRIES {
                std::fs::write(dir.join(format!("f{i:05}.rs")), b"fn x() {}\n")
                    .expect("write fixture");
            }

            // Before: what used to run inline in `rebuild()` / the create-func.
            let start = std::time::Instant::now();
            let nodes = scan_dir_nodes(
                &Worktree::Local(dir.clone()),
                &dir,
                Showing::TrackedAndSynced,
                &Default::default(),
                &[],
                false,
                None,
            );
            let synchronous_walk = start.elapsed();
            assert_eq!(
                nodes.len(),
                ENTRIES,
                "the fixture's own files, and nothing else"
            );

            // After: what runs inline now — schedule the blocking walk and an
            // apply-later future, then return. The walk itself happens off
            // this measurement entirely. Warmed up once first: the tokio
            // blocking pool spins up its first worker thread lazily, and that
            // one-time cost belongs to process startup, not to this call —
            // by the time a real IDE reaches its first `rebuild()`, plenty of
            // other `spawn_blocking` calls (status refresh alone) have already
            // paid it.
            let _ =
                crate::runtime::runtime().block_on(crate::runtime::runtime().spawn_blocking(|| ()));
            let store = gtk::gio::ListStore::new::<BoxedAnyObject>();
            let start = std::time::Instant::now();
            fill_dir_store_async(
                &store,
                Worktree::Local(dir.clone()),
                dir.clone(),
                Showing::TrackedAndSynced,
                Default::default(),
                Ghosts::new(None),
                None,
            );
            let create_func_returns_in = start.elapsed();

            println!(
                "dir walk: {ENTRIES} entries → synchronous walk (the old cost) {:>8.1?}; \
             create-func now returns in {:>8.1?}",
                synchronous_walk, create_func_returns_in,
            );
            assert!(
                create_func_returns_in.as_micros() < 500,
                "the create-func must return in microseconds, not participate in the walk \
             (took {create_func_returns_in:?})"
            );
            assert!(
                create_func_returns_in < synchronous_walk / 10,
                "the offloaded path should be an order of magnitude faster on the caller's \
             own thread than the walk it used to run inline"
            );

            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
