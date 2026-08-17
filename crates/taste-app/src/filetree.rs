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
    git: RefCell<Option<GitWorkspace>>,
    status: Rc<RefCell<HashMap<PathBuf, FileState>>>,
    list_holder: gtk::ScrolledWindow,
    branch_label: gtk::MenuButton,
    branch_popover: gtk::Popover,
    sync_label: gtk::Label,
    sync_button: gtk::Button,
    pull_button: gtk::Button,
    push_button: gtk::Button,
    abort_button: gtk::Button,
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
    /// Paths (repo-relative) touched by stash entries.
    stashed: RefCell<HashSet<PathBuf>>,
    syncing_filters: std::cell::Cell<bool>,
    /// Per-file cursor into its change hunks: each dirty-list click jumps
    /// to the next changed area.
    hunk_cycle: RefCell<HashMap<PathBuf, usize>>,
    /// Checked files in the changed list, awaiting a bulk action.
    selection: RefCell<HashSet<PathBuf>>,
    show_ignored: Rc<RefCell<bool>>,
    on_open: RefCell<Option<OpenCallback>>,
    /// Routes a staged diff to the chat agent, reply → commit entry.
    commit_suggester: RefCell<Option<SuggestCallback>>,
    /// The open context menu, closed before row rebinds dispose its anchor.
    open_menu: RefCell<Option<glib::WeakRef<gtk::PopoverMenu>>>,
}

type OpenCallback = Box<dyn Fn(PathBuf, Option<u32>)>;
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

impl FileTree {
    pub fn new(workspace: Workspace) -> Rc<Self> {
        // The branch is a dropdown: switch to any local branch, or type a
        // name to create one.
        let branch_label = gtk::MenuButton::builder()
            .css_classes(["flat"])
            .direction(gtk::ArrowType::Down)
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
        // search_delay debounces keystrokes; run_search additionally drops
        // stale results, so typing never staggers the UI.
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in project")
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
        // State-driven: you can't commit nothing, so the box only exists
        // when something is staged (or the Staged view is open). Its
        // appearance IS the signal.
        commit_row.set_visible(false);

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
            .css_classes(["destructive-action"])
            .visible(false)
            .build();
        // The branch dropdown lives with its consequences: pull/push
        // counts and the fetch button share this row.
        branch_label.set_halign(gtk::Align::Start);
        branch_label.set_hexpand(true);
        let sync_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sync_row.append(&branch_label);
        sync_row.append(&sync_label);
        sync_row.append(&abort_button);
        sync_row.append(&pull_button);
        sync_row.append(&push_button);
        sync_row.append(&sync_button);

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
        filter_box.append(&all_toggle);
        filter_box.append(&stashed_toggle);
        filter_box.append(&dirty_toggle);
        filter_box.append(&staged_toggle);
        branch_row.append(&filter_box);
        let filter_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        filter_spacer.set_hexpand(true);
        branch_row.append(&filter_spacer);
        branch_row.append(&ignored_toggle);
        // Index progress occludes exactly the place you would search.
        let index_bar = gtk::ProgressBar::builder()
            .show_text(true)
            .text("Indexing…")
            .visible(false)
            .can_target(false)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .css_classes(["osd"])
            .build();
        let search_overlay = gtk::Overlay::new();
        search_overlay.set_child(Some(&search_entry));
        search_overlay.add_overlay(&index_bar);
        // Section one: version control (branch, counts, commit).
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        search_overlay.set_hexpand(true);
        search_row.append(&search_overlay);
        search_row.append(&search_ghosts_toggle);
        header.append(&sync_row);
        header.append(&commit_row);
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
        root_row.append(
            &gtk::Label::builder()
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
                .build(),
        );
        widget.append(&root_row);
        widget.append(&list_holder);
        widget.append(&intervention);

        let tree = Rc::new(Self {
            widget,
            workspace: workspace.clone(),
            git: RefCell::new(GitWorkspace::discover(workspace.root())),
            status: Rc::new(RefCell::new(HashMap::new())),
            list_holder,
            intervention: intervention.clone(),
            branch_label,
            branch_popover: branch_popover.clone(),
            sync_label,
            sync_button: sync_button.clone(),
            pull_button: pull_button.clone(),
            push_button: push_button.clone(),
            abort_button: abort_button.clone(),
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
            stashed: RefCell::new(HashSet::new()),
            syncing_filters: std::cell::Cell::new(false),
            hunk_cycle: RefCell::new(HashMap::new()),
            selection: RefCell::new(HashSet::new()),
            show_ignored: Rc::new(RefCell::new(false)),
            on_open: RefCell::new(None),
            commit_suggester: RefCell::new(None),
            open_menu: RefCell::new(None),
        });

        let weak = Rc::downgrade(&tree);
        commit_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
                tree.commit();
            }
        });
        let weak = Rc::downgrade(&tree);
        push_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
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
        pull_button.connect_clicked(move |_| {
            if let Some(tree) = weak_pull.upgrade() {
                tree.sync();
            }
        });
        sync_button.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
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
        search_entry.connect_search_changed(move |entry| {
            let Some(tree) = weak.upgrade() else { return };
            let query = entry.text().to_string();
            if query.trim().is_empty() {
                *tree.search_view.borrow_mut() = None;
                tree.search_ghosts_toggle.set_sensitive(false);
                if tree.filters_active() {
                    tree.render_changed_list();
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
        for toggle in [&dirty_toggle, &staged_toggle, &stashed_toggle] {
            let weak = Rc::downgrade(&tree);
            toggle.connect_toggled(move |_| {
                let Some(tree) = weak.upgrade() else { return };
                if tree.syncing_filters.get() {
                    return;
                }
                tree.syncing_filters.set(true);
                tree.all_toggle.set_active(!tree.filters_active());
                tree.syncing_filters.set(false);
                tree.selection.borrow_mut().clear();
                tree.sync_filter_counts();
                if tree.filters_active() {
                    tree.search_entry.set_text("");
                    tree.render_changed_list();
                } else {
                    tree.rebuild();
                }
            });
        }
        {
            let weak = Rc::downgrade(&tree);
            all_toggle.connect_toggled(move |toggle| {
                let Some(tree) = weak.upgrade() else { return };
                if tree.syncing_filters.get() {
                    return;
                }
                tree.syncing_filters.set(true);
                if toggle.is_active() {
                    for other in [
                        &tree.stashed_toggle,
                        &tree.dirty_toggle,
                        &tree.staged_toggle,
                    ] {
                        other.set_active(false);
                    }
                } else if !tree.filters_active() {
                    // All can't be turned off into nothing.
                    toggle.set_active(true);
                }
                tree.syncing_filters.set(false);
                tree.selection.borrow_mut().clear();
                if tree.filters_active() {
                    tree.render_changed_list();
                } else {
                    tree.rebuild();
                }
            });
        }
        let weak = Rc::downgrade(&tree);
        ignored_toggle.connect_toggled(move |button| {
            if let Some(tree) = weak.upgrade() {
                *tree.show_ignored.borrow_mut() = button.is_active();
                // Never clobber active search results with the tree.
                if tree.search_entry.text().trim().is_empty() {
                    tree.rebuild();
                }
            }
        });

        tree.refresh_status();
        tree.rebuild();
        tree.rebuild_index();
        tree
    }

    /// (Re)build the search index in the background, progress shown over
    /// the search entry. Kept fresh by structural workspace changes.
    pub fn rebuild_index(self: &Rc<Self>) {
        if self.index_building.get() {
            return;
        }
        self.index_building.set(true);
        self.index_bar.set_fraction(0.0);
        self.index_bar.set_text(Some("Indexing…"));
        self.index_bar.set_visible(true);
        let root = self.workspace.root().to_path_buf();
        let (tx, rx) = async_channel::unbounded::<usize>();
        let handle = crate::runtime::runtime().spawn_blocking(move || {
            taste_core::search::collect_files(&root, |count| {
                let _ = tx.try_send(count);
            })
        });
        {
            let bar = self.index_bar.clone();
            glib::spawn_future_local(async move {
                while let Ok(count) = rx.recv().await {
                    bar.pulse();
                    bar.set_text(Some(&format!("Indexing… {count} files")));
                }
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
        let root = self.workspace.root().to_path_buf();
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
            let root = tree.workspace.root().to_path_buf();
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
        let safe_mode = !self.workspace.exec.is_container();
        taste_core::policy::write_allowed(self.workspace.root(), safe_mode, path)
    }

    fn op_denied_dialog(&self) {
        self.workspace.events.publish(Event::Toast(
            "Read-only in safe mode — only devcontainer setup and workspace dotfiles are editable"
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
        let add_action = |name: &str, callback: Box<dyn Fn() + 'static>| {
            let action = gio::SimpleAction::new(name, None);
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
                .unwrap_or_else(|| self.workspace.root().to_path_buf())
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
    pub fn refresh_status(self: &Rc<Self>) {
        // Full-status computation runs off the main thread: with an agent
        // or build churning files, doing this synchronously would stutter
        // every interaction (window drags included). Results apply — and
        // rows restyle — when ready.
        let root = self.workspace.root().to_path_buf();
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
                })
            });
            let Ok(snapshot) = handle.await else { return };
            let Some(tree) = weak.upgrade() else { return };
            tree.apply_status(snapshot);
        });
    }

    fn apply_status(self: &Rc<Self>, snapshot: Option<StatusSnapshot>) {
        let mut unchanged = false;
        match snapshot {
            Some(snapshot) => {
                // Unchanged status must not churn row widgets: factory
                // resets during agent/build activity are what made hovering
                // feel laggy.
                unchanged = *self.status.borrow() == snapshot.status
                    && *self.stashed.borrow() == snapshot.stashed;
                *self.status.borrow_mut() = snapshot.status;
                *self.stashed.borrow_mut() = snapshot.stashed;
                self.ignore_rules.set(snapshot.ignore_rules);
                self.sync_filter_counts();
                self.branch_label
                    .set_label(&snapshot.branch.unwrap_or_else(|| "(no branch)".into()));
                self.abort_button.set_visible(snapshot.rebasing);
                self.sync_button.set_sensitive(!snapshot.rebasing);
                if snapshot.rebasing {
                    self.sync_label
                        .set_label("rebase in progress — resolve conflicts or abort");
                } else {
                    match snapshot.sync {
                        Some(sync) => match sync.upstream {
                            Some(upstream) => {
                                // The upstream name lives in the button
                                // tooltips; the label is for exceptions.
                                self.sync_label.set_label("");
                                self.push_button.set_label(&format!("↑ {}", sync.ahead));
                                self.push_button.set_sensitive(sync.ahead > 0);
                                self.push_button.set_tooltip_text(Some(&format!(
                                    "Push {} commit{} to {upstream}",
                                    sync.ahead,
                                    if sync.ahead == 1 { "" } else { "s" }
                                )));
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
            None => self.branch_label.set_label("not a git repository"),
        }
        // Views refresh only now, with the fresh map in place — and only
        // if something actually changed.
        if unchanged {
            return;
        }
        if self.filters_active() && self.search_entry.text().trim().is_empty() {
            self.render_changed_list();
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
        drop(status);
        let stashed = self.stashed.borrow().len();
        self.dirty_toggle.set_label(&format!("Dirty {dirty}"));
        self.staged_toggle.set_label(&format!("Staged {staged}"));
        self.commit_box
            .set_visible(staged > 0 || self.staged_toggle.is_active());
        if staged > 0 {
            self.staged_toggle.add_css_class("accent");
        } else {
            self.staged_toggle.remove_css_class("accent");
        }
        self.stashed_toggle.set_label(&format!("Stashed {stashed}"));
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

    /// True when any git filter is on: the list shows the OR of the
    /// active categories instead of the tree.
    fn filters_active(&self) -> bool {
        self.dirty_toggle.is_active()
            || self.staged_toggle.is_active()
            || self.stashed_toggle.is_active()
    }

    fn render_changed_list(self: &Rc<Self>) {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let dirty_on = self.dirty_toggle.is_active();
        let staged_on = self.staged_toggle.is_active();
        let stashed_on = self.stashed_toggle.is_active();
        // OR of the active categories; a path in several shows once.
        let mut matched: std::collections::BTreeMap<PathBuf, (FileState, bool)> =
            std::collections::BTreeMap::new();
        {
            let stashed = self.stashed.borrow();
            for (path, state) in self.status.borrow().iter() {
                if *state == FileState::Ignored {
                    continue;
                }
                if (dirty_on && state.stageable()) || (staged_on && *state == FileState::Staged) {
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
            let empty = adw::StatusPage::builder()
                .icon_name("object-select-symbolic")
                .title("No Matching Files")
                .description("Nothing in the selected categories right now")
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
        for (rel, state, in_stash) in entries {
            let check = gtk::CheckButton::new();
            check.set_tooltip_text(Some("Select for stage/stash/unstage actions"));
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
                    tree.selection_intervention();
                });
            }
            let row = adw::ActionRow::builder()
                .title(
                    rel.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
                .subtitle(rel.display().to_string())
                .activatable(true)
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
                .sensitive(matches!(state, FileState::Modified | FileState::Conflicted))
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
                        tree.open_next_change(abs.clone());
                    }
                });
            }
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
        let selected: Vec<PathBuf> = self.selection.borrow().iter().cloned().collect();
        if selected.is_empty() {
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
        drop(status);
        drop(stashed);

        let content = self.open_intervention(&format!(
            "{} file{} selected",
            selected.len(),
            if selected.len() == 1 { "" } else { "s" }
        ));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for (label, count, op) in [
            ("Stage", stageable, "stage"),
            ("Unstage", staged, "unstage"),
            ("Stash", stageable + staged, "stash"),
            ("Unstash", in_stash, "unstash"),
            // The lazy path: whatever it takes (unstash, stage), then the
            // commit box. Opinionated about the stack, not your commits.
            ("Commit…", selected.len(), "commit"),
        ] {
            let button = gtk::Button::builder()
                .label(if count > 0 {
                    format!("{label} {count}")
                } else {
                    label.to_string()
                })
                .sensitive(count > 0)
                .build();
            if op == "commit" {
                button.add_css_class("suggested-action");
            }
            let weak = Rc::downgrade(self);
            let op: &'static str = op;
            button.connect_clicked(move |_| {
                if let Some(tree) = weak.upgrade() {
                    tree.run_selection_op(op);
                }
            });
            row.append(&button);
        }
        content.append(&row);
    }

    /// Apply one bulk op to the eligible selected files, off-thread.
    fn run_selection_op(self: &Rc<Self>, op: &'static str) {
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
                    "commit" => {
                        // Get every selected file staged, unstashing where
                        // that's what it takes; the commit box finishes it.
                        let entries = git.stash_entries().map_err(|e| e.to_string())?;
                        let status = git.status().map_err(|e| e.to_string())?;
                        for rel in &rels {
                            let stash_only = !status.contains_key(rel)
                                && entries.iter().any(|paths| paths.contains(rel));
                            if stash_only {
                                if let Some(index) =
                                    entries.iter().position(|paths| paths.contains(rel))
                                {
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
                            git.stage(rel).map_err(|e| e.to_string())?;
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
            match result {
                Ok(()) => events.publish(Event::Toast(format!("{op}: done"))),
                Err(e) => events.publish(Event::Toast(format!("{op} failed: {e}"))),
            }
            if let Some(tree) = weak.upgrade() {
                tree.selection.borrow_mut().clear();
                tree.close_intervention();
                if op == "stage" || op == "commit" {
                    // Staging leads to committing: land on the Staged view
                    // with the commit box waiting.
                    tree.staged_toggle.set_active(true);
                }
                if op == "commit" {
                    tree.commit_entry.grab_focus();
                }
                tree.refresh_status();
            }
        });
    }

    /// Open a dirty file at its next change hunk: first click lands on
    /// the first change, further clicks walk the rest, wrapping around.
    fn open_next_change(self: &Rc<Self>, abs: PathBuf) {
        let weak = Rc::downgrade(self);
        let file = abs.clone();
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let text = std::fs::read_to_string(&file).ok()?;
                let old = GitWorkspace::discover(&file).and_then(|git| {
                    let rel = file.strip_prefix(git.workdir()).ok()?.to_path_buf();
                    git.head_content(&rel)
                });
                // Untracked (no baseline): the whole file is one change.
                let Some(old) = old else {
                    return Some(vec![1u32]);
                };
                let diff = similar::TextDiff::from_lines(&old, &text);
                let mut starts: Vec<u32> = Vec::new();
                for group in diff.grouped_ops(0) {
                    if let Some(op) = group.first() {
                        starts.push(op.new_range().start as u32 + 1);
                    }
                }
                if starts.is_empty() {
                    starts.push(1);
                }
                Some(starts)
            });
            let Ok(Some(starts)) = handle.await else {
                return;
            };
            let Some(tree) = weak.upgrade() else { return };
            let index = {
                let mut cycle = tree.hunk_cycle.borrow_mut();
                let slot = cycle.entry(abs.clone()).or_insert(0);
                let index = *slot % starts.len();
                *slot = index + 1;
                index
            };
            tree.open(abs.clone(), starts.get(index).copied());
        });
    }

    /// Open the intervention panel with a title; returns the content box.
    /// Replaces any previous intervention. Closing cancels.
    fn open_intervention(self: &Rc<Self>, title: &str) -> gtk::Box {
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
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .css_classes(["flat", "circular"])
            .build();
        let weak = Rc::downgrade(self);
        close.connect_clicked(move |_| {
            if let Some(tree) = weak.upgrade() {
                tree.close_intervention();
            }
        });
        header.append(&label);
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
        self.intervention_file.borrow_mut().take();
        self.intervention.set_visible(false);
        while let Some(child) = self.intervention.first_child() {
            self.intervention.remove(&child);
        }
    }

    /// Discard = destructive, so it confirms — in the panel, not a dialog.
    fn discard_intervention(self: &Rc<Self>, abs: PathBuf) {
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
                    tree.close_intervention();
                    tree.refresh_status();
                }
            });
        });
        content.append(&button);
    }

    /// Stash one file, with an editable stash message.
    fn stash_intervention(self: &Rc<Self>, abs: PathBuf) {
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
                    tree.close_intervention();
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
                    tree.close_intervention();
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

    /// Fetch, then rebase onto the remote tip. Both remote-read-only and
    /// local operations; push stays a separate, deliberate user action.
    fn sync(self: &Rc<Self>) {
        let Some((fetch, rebase)) = self
            .git
            .borrow()
            .as_ref()
            .map(|git| (git.fetch_command(), git.rebase_command()))
        else {
            return;
        };
        self.sync_button.set_sensitive(false);
        self.sync_label.set_label("syncing…");
        let events = self.workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            for (label, (program, args)) in [("fetch", fetch), ("rebase", rebase)] {
                let output = tokio::process::Command::new(&program)
                    .args(&args)
                    .output()
                    .await;
                match output {
                    Ok(out) if out.status.success() => {}
                    Ok(out) => {
                        events.publish(Event::Toast(format!(
                            "{label} failed: {}",
                            String::from_utf8_lossy(&out.stderr)
                                .lines()
                                .next()
                                .unwrap_or("")
                        )));
                        break;
                    }
                    Err(e) => {
                        events.publish(Event::Toast(format!("{label} failed: {e}")));
                        break;
                    }
                }
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    fn abort_rebase(self: &Rc<Self>) {
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

    fn repo_relative(&self, path: &Path) -> Option<PathBuf> {
        let git = self.git.borrow();
        let workdir = git.as_ref()?.workdir().to_path_buf();
        path.strip_prefix(workdir).ok().map(Path::to_path_buf)
    }

    fn state_of(&self, node: &FileNode) -> FileState {
        let Some(rel) = self.repo_relative(&node.path) else {
            return FileState::Clean;
        };
        let status = self.status.borrow();
        if node.is_dir {
            // Directories aggregate: any interesting child state wins.
            let mut aggregate = FileState::Clean;
            for (path, state) in status.iter() {
                if path.starts_with(&rel) {
                    match state {
                        FileState::Conflicted => return FileState::Conflicted,
                        FileState::Staged => aggregate = FileState::Staged,
                        FileState::Modified | FileState::Untracked
                            if aggregate == FileState::Clean =>
                        {
                            aggregate = FileState::Modified
                        }
                        _ => {}
                    }
                }
            }
            aggregate
        } else {
            status.get(&rel).copied().unwrap_or(FileState::Clean)
        }
    }

    /// Build the (fresh) tree model. Called on construction and when the
    /// ignored-files toggle flips. Git-status changes only restyle rows.
    fn rebuild(self: &Rc<Self>) {
        self.close_open_menu();
        let show_ignored = *self.show_ignored.borrow();
        // Search shapes the tree: matches-only filters to matching files
        // (autoexpanded); ghost mode keeps every file and dims the rest.
        let search = self.search_view.borrow().clone();
        let ghost_mode = self.search_ghosts_toggle.is_active();
        let filter: Option<Rc<HashSet<PathBuf>>> = match (&search, ghost_mode) {
            (Some(view), false) => Some(view.visible.clone()),
            _ => None,
        };
        let ghosts = if search.is_some() {
            Vec::new() // template suggestions are noise in search results
        } else {
            ghost_candidates(self.workspace.root())
        };
        let root_store = build_dir_store(
            self.workspace.root(),
            show_ignored,
            &ghosts,
            filter.as_ref(),
        );
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

        let (badge, css) = match state {
            FileState::Clean => ("", None),
            FileState::Modified => ("M", Some("warning")),
            FileState::Staged => ("S", Some("success")),
            FileState::Untracked => ("U", Some("accent")),
            FileState::Conflicted => ("!", Some("error")),
            FileState::Ignored => ("·", Some("dim-label")),
        };
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

        // Safe mode: everything outside the devcontainer scope is read-only
        // (for the user and the AI alike) until the container runs.
        let safe_mode = !self.workspace.exec.is_container();
        if safe_mode && !taste_core::policy::write_allowed(self.workspace.root(), true, &node.path)
        {
            let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
            lock.add_css_class("dim-label");
            lock.set_tooltip_text(Some(
                "Read-only in safe mode: only the devcontainer setup is editable",
            ));
            row.append(&lock);
            label.add_css_class("dim-label");
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
            .strip_prefix(self.workspace.root())
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
    fn create_ghost(self: &Rc<Self>, path: &Path) {
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
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("creating {}: {e}", parent.display());
                return;
            }
        }
        if !path.exists() {
            if let Err(e) = std::fs::write(path, content) {
                tracing::warn!("creating {}: {e}", path.display());
                return;
            }
        }
        self.workspace.events.publish(Event::GitStatusChanged);
        self.refresh_status();
        self.rebuild();
        self.open(path.to_path_buf(), None);
    }

    fn toggle_stage(self: &Rc<Self>, path: &Path, currently_staged: bool) {
        let Some(rel) = self.repo_relative(path) else {
            return;
        };
        {
            let git = self.git.borrow();
            let Some(git) = git.as_ref() else { return };
            let result = if currently_staged {
                git.unstage(&rel)
            } else {
                git.stage(&rel)
            };
            if let Err(e) = result {
                self.workspace.events.publish(Event::Toast(format!(
                    "Staging {} failed: {e}",
                    rel.display()
                )));
                return;
            }
        }
        self.workspace.events.publish(Event::GitStatusChanged);
    }

    fn commit(self: &Rc<Self>) {
        let message = self.commit_entry.text().to_string();
        if message.trim().is_empty() {
            return;
        }
        {
            let git = self.git.borrow();
            let Some(git) = git.as_ref() else { return };
            match git.commit(&message) {
                Ok(_) => self.commit_entry.set_text(""),
                Err(e) => {
                    self.workspace
                        .events
                        .publish(Event::Toast(format!("Commit failed: {e}")));
                    return;
                }
            }
        }
        self.workspace.events.publish(Event::GitStatusChanged);
    }

    fn push(self: &Rc<Self>) {
        let Some((program, args)) = self.git.borrow().as_ref().map(|git| git.push_command()) else {
            return;
        };
        let events = self.workspace.events.clone();
        // Push runs on the host with the user's own credential helpers.
        let spec = {
            let exec = taste_core::ExecContext::host();
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            exec.resolve(&program, &arg_refs, false)
        };
        crate::runtime::runtime().spawn(async move {
            let output = tokio::process::Command::new(&spec.program)
                .args(&spec.args)
                .output()
                .await;
            match output {
                Ok(out) if out.status.success() => {
                    events.publish(Event::Toast("Pushed".into()));
                }
                Ok(out) => events.publish(Event::Toast(format!(
                    "Push failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .next()
                        .unwrap_or("")
                ))),
                Err(e) => events.publish(Event::Toast(format!("Push failed: {e}"))),
            }
            events.publish(Event::GitStatusChanged);
        });
    }

    /// Restyle rows after a status change without rebuilding the tree
    /// (expansion state is preserved; rows re-bind lazily on redraw).
    pub fn on_git_status_changed(self: &Rc<Self>) {
        // apply_status restyles the rows once the fresh map lands.
        self.refresh_status();
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
    let mut ghosts = Vec::new();
    // The devcontainer setup, if none exists in any spec'd location.
    let has_devcontainer =
        root.join(".devcontainer").exists() || root.join(".devcontainer.json").exists();
    if !has_devcontainer {
        ghosts.push(root.join(".devcontainer/devcontainer.json"));
    }
    for name in [".editorconfig", ".gitignore", ".gitattributes"] {
        let path = root.join(name);
        if !path.exists() {
            ghosts.push(path);
        }
    }
    ghosts
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
