//! Center pane: tabbed GtkSourceView editors.
//!
//! One buffer per open file, held in tabs — switching files never discards
//! anything, and closing a dirty tab asks first. Each page carries its own
//! .editorconfig policies and markdown mode. External changes (agents,
//! container builds) reload clean buffers in place; dirty buffers are
//! flagged, never clobbered.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

struct EditorPage {
    page: adw::TabPage,
    view: sourceview5::View,
    buffer: sourceview5::Buffer,
    /// Save-time .editorconfig policies for this file.
    trim_trailing_ws: Cell<bool>,
    final_newline: Cell<bool>,
    /// Line endings detected at load (true = CRLF); buffers are always LF
    /// internally and saves restore this unless .editorconfig overrides.
    crlf: Cell<bool>,
    /// .editorconfig end_of_line override (Some(true) = CRLF, Some(false)
    /// = LF, None = preserve what the file had).
    eol_override: Cell<Option<bool>>,
    /// UTF-8 BOM present at load (or demanded by charset = utf-8-bom).
    bom: Cell<bool>,
    /// Markdown raw-source mode (false = WYSIWYG) for this page.
    raw_source: Cell<bool>,
    /// Performance guard: very long lines (minified files, data blobs) make
    /// syntax regexes and per-keystroke restyling pathological, so such
    /// files render as plain text.
    plain: Cell<bool>,
    /// Pending AI ghost-text suggestion (offset where it starts, text).
    suggestion: RefCell<Option<(i32, String)>>,
    /// A markdown restyle is scheduled (typing-burst coalescing).
    restyle_queued: Cell<bool>,
    /// The native minimap (GtkSourceMap), hidden for plain-guard files.
    map: sourceview5::Map,
    /// Changes view (HEAD ↔ buffer, removals visible) instead of the
    /// editable buffer.
    changes_view: Cell<bool>,
    stack: gtk::Stack,
    diff_buffer: gtk::TextBuffer,
    /// The rendered markdown preview face (rebuilt on refresh).
    preview_holder: gtk::Box,
    /// Revealed when the file changed on disk UNDER unsaved edits.
    conflict_bar: adw::Banner,
    /// Hash of the bytes the last save wrote: the watcher echoes our own
    /// writes back as FileChanged, and matching content here (not buffer
    /// cleanliness — the user may have typed since) is what tells an echo
    /// from a real external change.
    saved_hash: Cell<Option<u64>>,
    /// A warning icon (conflict, failed save) occupies the tab's indicator
    /// slot, which the git dirty-dot also uses; this flag keeps the two
    /// from clobbering each other.
    warned: Cell<bool>,
}

const MAX_HIGHLIGHT_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_HIGHLIGHT_LINE_BYTES: usize = 4096;
const MAX_SELECTION_CAPTURE_CHARS: usize = 8192;
/// WYSIWYG re-parses the whole document on (coalesced) edits; measured at
/// ~100ms/512KiB in debug builds, so cap it — larger markdown opens as raw
/// source with incremental syntax highlighting instead.
const MAX_WYSIWYG_CHARS: i32 = 256 * 1024;
/// Changes view: cap rendered diff lines (same spirit as the transcript).
const MAX_DIFF_LINES: usize = 4000;

/// Uncommitted-change dot in the tab's indicator slot (the icon slot
/// holds the file-type icon).
fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn set_dirty_dot(tab: &adw::TabPage, dirty: bool) {
    // Idempotent: resetting the same indicator forces TabBar redraws.
    if tab.indicator_icon().is_some() == dirty {
        return;
    }
    if dirty {
        tab.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new("media-record-symbolic")));
        tab.set_indicator_tooltip("Unstaged changes");
    } else {
        tab.set_indicator_icon(gtk::gio::Icon::NONE);
    }
}

/// The file-type icon GNOME associates with this file name.
pub(crate) fn file_type_icon(path: &Path) -> gtk::gio::Icon {
    let (content_type, _) = gtk::gio::functions::content_type_guess(Some(path), None::<&[u8]>);
    gtk::gio::functions::content_type_get_symbolic_icon(&content_type)
}

/// Whether a file is safe to highlight (and, for markdown, to restyle per
/// keystroke) without pathological layout/regex costs.
fn highlighting_ok(content: &str) -> bool {
    content.len() <= MAX_HIGHLIGHT_FILE_BYTES
        && content
            .split('\n')
            .all(|line| line.len() <= MAX_HIGHLIGHT_LINE_BYTES)
}

pub struct Editor {
    pub widget: gtk::Box,
    workspace: taste_core::Workspace,
    tabs: adw::TabView,
    mode_menu: gtk::MenuButton,
    mode_popover: gtk::Popover,
    pages: RefCell<HashMap<PathBuf, Rc<EditorPage>>>,
    /// Guards toggle updates driven by page switches from re-triggering.
    /// Edit ↔ changes-view toggle for the selected tab.
    /// Latest git states of uncommitted files (absolute paths), for the
    /// tabs' dirty dots.
    git_dirty: RefCell<HashMap<PathBuf, taste_git::FileState>>,
    /// Browser-style file navigation: visited files and the cursor into
    /// that history.
    nav_history: RefCell<Vec<PathBuf>>,
    nav_pos: Cell<usize>,
    pub back_button: gtk::Button,
    pub forward_button: gtk::Button,
}

const MAX_NAV_HISTORY: usize = 100;

impl Editor {
    pub fn new(workspace: taste_core::Workspace) -> Rc<Self> {
        let tabs = adw::TabView::new();
        tabs.set_vexpand(true);
        // Natural-width tabs: sized by their title, so opening another
        // file never resizes the existing tabs. Reordering is TabBar's
        // built-in drag behavior.
        let tab_bar = adw::TabBar::builder()
            .view(&tabs)
            .autohide(false)
            .hexpand(true)
            .expand_tabs(false)
            .build();

        // One display-mode dropdown (canonical flat style, like the
        // console's + button): Edit / Preview / Changes, icon + name each.
        let mode_popover = gtk::Popover::new();
        let mode_menu = gtk::MenuButton::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Display mode for this file")
            .css_classes(["flat"])
            .sensitive(false)
            .popover(&mode_popover)
            .build();
        // Canonical placement (the console's + button): an action widget
        // INSIDE the tab bar, so heights always match.
        tab_bar.set_end_action_widget(Some(&mode_menu));
        let back_button = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back to the previously viewed file")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let forward_button = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Forward again")
            .css_classes(["flat"])
            .sensitive(false)
            .build();

        let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        top_row.append(&tab_bar);

        let empty = adw::StatusPage::builder()
            .icon_name("taste-wilted-folder")
            .title("No Files Open")
            .description("Open a file from the sidebar, or ask the agent.")
            .build();
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&tabs, Some("tabs"));
        {
            let stack = stack.clone();
            tabs.connect_n_pages_notify(move |tabs| {
                stack.set_visible_child_name(if tabs.n_pages() == 0 { "empty" } else { "tabs" });
            });
        }

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&top_row);
        widget.append(&stack);

        let editor = Rc::new(Self {
            widget,
            workspace,
            tabs,
            mode_menu: mode_menu.clone(),
            mode_popover: mode_popover.clone(),
            pages: RefCell::new(HashMap::new()),
            git_dirty: RefCell::new(HashMap::new()),
            nav_history: RefCell::new(Vec::new()),
            nav_pos: Cell::new(0),
            back_button: back_button.clone(),
            forward_button: forward_button.clone(),
        });

        let weak = Rc::downgrade(&editor);
        back_button.connect_clicked(move |_| {
            if let Some(editor) = weak.upgrade() {
                editor.navigate(-1);
            }
        });
        let weak = Rc::downgrade(&editor);
        forward_button.connect_clicked(move |_| {
            if let Some(editor) = weak.upgrade() {
                editor.navigate(1);
            }
        });

        let weak = Rc::downgrade(&editor);
        mode_popover.connect_show(move |_| {
            if let Some(editor) = weak.upgrade() {
                editor.populate_mode_menu();
            }
        });

        // Keep the toggles reflecting the selected page; every selection
        // is a visit for back/forward.
        let weak = Rc::downgrade(&editor);
        editor.tabs.connect_selected_page_notify(move |_| {
            let Some(editor) = weak.upgrade() else { return };
            editor.sync_toggle_to_selection();
            if let Some((path, _)) = editor.selected() {
                editor.record_visit(path);
            }
        });

        // Dirty tabs ask before closing; that is the whole point of tabs.
        let weak = Rc::downgrade(&editor);
        editor.tabs.connect_close_page(move |tabs, page| {
            let Some(editor) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let Some((path, entry)) = editor.page_by_tab(page) else {
                return glib::Propagation::Proceed;
            };
            if !entry.buffer.is_modified() {
                editor.pages.borrow_mut().remove(&path);
                tabs.close_page_finish(page, true);
                editor.publish_state();
                return glib::Propagation::Stop;
            }
            let dialog = adw::AlertDialog::new(
                Some("Discard unsaved changes?"),
                Some(&format!(
                    "“{}” has unsaved changes.",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                )),
            );
            dialog.add_responses(&[
                ("cancel", "Keep Editing"),
                ("save", "Save and Close"),
                ("discard", "Discard"),
            ]);
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let weak = Rc::downgrade(&editor);
            let tabs = tabs.clone();
            let page = page.clone();
            dialog.connect_response(None, move |_, response| {
                let Some(editor) = weak.upgrade() else { return };
                match response {
                    "save" => {
                        // Close ONLY if the save succeeded: a refused or
                        // failed save closing the tab would be data loss.
                        let saved = match editor.page_by_tab(&page) {
                            Some((path, entry)) => editor.save_page(&path, &entry).is_ok(),
                            None => true,
                        };
                        if saved {
                            if let Some((path, _)) = editor.page_by_tab(&page) {
                                editor.pages.borrow_mut().remove(&path);
                            }
                            tabs.close_page_finish(&page, true);
                            editor.publish_state();
                        } else {
                            tabs.close_page_finish(&page, false);
                            let error = adw::AlertDialog::new(
                                Some("Could not save"),
                                Some(&page.tooltip().unwrap_or_default()),
                            );
                            error.add_responses(&[("close", "Close")]);
                            error.present(Some(&editor.widget));
                        }
                    }
                    "discard" => {
                        if let Some((path, _)) = editor.page_by_tab(&page) {
                            editor.pages.borrow_mut().remove(&path);
                        }
                        tabs.close_page_finish(&page, true);
                        editor.publish_state();
                    }
                    _ => tabs.close_page_finish(&page, false),
                }
            });
            dialog.present(Some(&editor.widget));
            glib::Propagation::Stop
        });

        editor
    }

    /// Record a file visit (selection change). Arriving somewhere via
    /// back/forward is recognized by position and not re-recorded, so the
    /// two directions stay stable.
    fn record_visit(&self, path: PathBuf) {
        {
            let mut history = self.nav_history.borrow_mut();
            let pos = self.nav_pos.get();
            if history.get(pos) == Some(&path) {
                return;
            }
            let keep = (pos + 1).min(history.len());
            history.truncate(keep);
            history.push(path);
            if history.len() > MAX_NAV_HISTORY {
                history.remove(0);
            }
            self.nav_pos.set(history.len() - 1);
        }
        self.sync_nav_buttons();
    }

    /// Move through visited files; closed files reopen.
    fn navigate(self: &Rc<Self>, direction: i64) {
        let target = {
            let history = self.nav_history.borrow();
            let pos = self.nav_pos.get() as i64 + direction;
            if pos < 0 || pos as usize >= history.len() {
                return;
            }
            self.nav_pos.set(pos as usize);
            history[pos as usize].clone()
        };
        self.sync_nav_buttons();
        self.open_at(&target, None);
    }

    fn sync_nav_buttons(&self) {
        let len = self.nav_history.borrow().len();
        let pos = self.nav_pos.get();
        self.back_button.set_sensitive(pos > 0);
        self.forward_button.set_sensitive(pos + 1 < len);
    }

    /// Close the focused tab (Ctrl+W); the dirty-close dialog applies.
    pub fn close_current(self: &Rc<Self>) {
        if let Some(page) = self.tabs.selected_page() {
            self.tabs.close_page(&page);
        }
    }

    fn selected(&self) -> Option<(PathBuf, Rc<EditorPage>)> {
        let selected = self.tabs.selected_page()?;
        self.page_by_tab(&selected)
    }

    fn page_by_tab(&self, tab: &adw::TabPage) -> Option<(PathBuf, Rc<EditorPage>)> {
        self.pages
            .borrow()
            .iter()
            .find(|(_, p)| p.page == *tab)
            .map(|(path, p)| (path.clone(), p.clone()))
    }

    fn sync_toggle_to_selection(self: &Rc<Self>) {
        let selected = self.selected();
        // The dropdown's own icon names the current mode.
        let icon = selected
            .as_ref()
            .map(|(path, page)| {
                if page.changes_view.get() {
                    "view-dual-symbolic"
                } else if is_markdown(path) && !page.raw_source.get() {
                    "x-office-document-symbolic"
                } else {
                    "document-edit-symbolic"
                }
            })
            .unwrap_or("document-edit-symbolic");
        self.mode_menu.set_icon_name(icon);
        // Disabled (never hidden) when nothing is selected.
        self.mode_menu.set_sensitive(selected.is_some());
        self.publish_state();
    }

    /// The display-mode menu: Edit / Preview / Changes, each icon + name,
    /// checkmark on the active one. Preview stays listed (disabled) for
    /// non-markdown files.
    fn populate_mode_menu(self: &Rc<Self>) {
        let Some((path, page)) = self.selected() else {
            return;
        };
        let is_md = is_markdown(&path);
        let changes = page.changes_view.get();
        let preview = is_md && !page.raw_source.get() && !changes;
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        for (label, icon, mode, enabled, current) in [
            (
                "Edit",
                "document-edit-symbolic",
                "edit",
                true,
                !changes && !preview,
            ),
            (
                "Preview",
                "x-office-document-symbolic",
                "preview",
                is_md,
                preview,
            ),
            ("Changes", "view-dual-symbolic", "changes", true, changes),
        ] {
            let row = adw::ActionRow::builder()
                .title(label)
                .activatable(enabled)
                .sensitive(enabled)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            if current {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            let weak = Rc::downgrade(self);
            let mode: &'static str = mode;
            row.connect_activated(move |_| {
                let Some(editor) = weak.upgrade() else { return };
                editor.mode_popover.popdown();
                let Some((path, page)) = editor.selected() else {
                    return;
                };
                match mode {
                    "edit" => {
                        page.raw_source.set(true);
                        editor.set_changes_view(&path, &page, false);
                        editor.refresh_markdown_mode(&path, &page);
                    }
                    "preview" => {
                        page.raw_source.set(false);
                        editor.set_changes_view(&path, &page, false);
                        editor.refresh_markdown_mode(&path, &page);
                    }
                    "changes" => editor.set_changes_view(&path, &page, true),
                    _ => {}
                }
                editor.sync_toggle_to_selection();
            });
            list.append(&row);
        }
        list.set_width_request(200);
        self.mode_popover.set_child(Some(&list));
    }

    /// Open (or focus) a file, optionally jumping to a 1-based line. Never
    /// discards anything: each file keeps its own buffer until its tab is
    /// explicitly closed.
    pub fn open_at(self: &Rc<Self>, path: &Path, line: Option<u32>) {
        self.open_with(path, line, false);
    }

    /// Open (or focus) a file with its Changes face showing: the
    /// changed-files list opens diffs, not buffers.
    pub fn open_changes(self: &Rc<Self>, path: &Path) {
        self.open_with(path, None, true);
    }

    fn open_with(self: &Rc<Self>, path: &Path, line: Option<u32>, changes: bool) {
        let existing = self.pages.borrow().get(path).cloned();
        if let Some(existing) = existing {
            self.tabs.set_selected_page(&existing.page);
            if let Some(line) = line {
                jump_to_line(&existing.view, &existing.buffer, line);
            }
            if changes {
                self.set_changes_view(path, &existing, true);
                self.sync_toggle_to_selection();
            }
            return;
        }
        // File IO never runs on the main thread: a large file must not
        // freeze the UI between click and tab.
        let editor_events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        let path = path.to_path_buf();
        glib::spawn_future_local(async move {
            let read_path = path.clone();
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || std::fs::read_to_string(&read_path));
            let content = match handle.await {
                Ok(Ok(content)) => content,
                Ok(Err(e)) => {
                    // Failures speak: silence here cost a confused click.
                    editor_events.publish(taste_core::Event::Toast(format!(
                        "Cannot open {}: {e}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    )));
                    return;
                }
                Err(_) => return,
            };
            let Some(editor) = weak.upgrade() else { return };
            // Re-check: another path may have opened it while we read.
            let already = editor.pages.borrow().get(&path).cloned();
            match already {
                Some(existing) => {
                    editor.tabs.set_selected_page(&existing.page);
                    if let Some(line) = line {
                        jump_to_line(&existing.view, &existing.buffer, line);
                    }
                }
                None => editor.create_page(&path, content, line),
            }
            if changes {
                let page = editor.pages.borrow().get(&path).cloned();
                if let Some(page) = page {
                    editor.set_changes_view(&path, &page, true);
                    editor.sync_toggle_to_selection();
                }
            }
        });
    }

    /// Open `path` as an UNSAVED buffer prefilled with `content` — the
    /// ghost-file flow. The file does not exist until the user saves. A
    /// second request for the same path refocuses the open tab.
    pub fn open_unsaved(self: &Rc<Self>, path: &Path, content: String) {
        if let Some(existing) = self.pages.borrow().get(path) {
            self.tabs.set_selected_page(&existing.page);
            return;
        }
        self.create_page(path, content, None);
        if let Some(page) = self.pages.borrow().get(path) {
            page.buffer.set_modified(true);
        }
    }

    fn create_page(self: &Rc<Self>, path: &Path, content: String, line: Option<u32>) {
        let (content, had_crlf, had_bom) = normalize_load(&content);
        let buffer = sourceview5::Buffer::new(None);
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_monospace(true);
        view.set_show_line_numbers(true);
        view.set_highlight_current_line(true);
        view.set_hexpand(true);
        view.set_vexpand(true);
        apply_scheme_for_style(&buffer);
        adw::StyleManager::default().connect_dark_notify(glib::clone!(
            #[weak]
            buffer,
            move |_| apply_scheme_for_style(&buffer)
        ));

        // The scroller must exist before Map::set_view: GtkSourceMap grabs
        // the view's vadjustment once, at set-view time, and never re-hooks.
        // Binding before the ScrolledWindow installs its adjustment left the
        // slider tracking a dead adjustment — no viewport highlight.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hexpand(true)
            .build();

        // Native minimap: GtkSourceView's own overview strip (as in
        // Builder), scheme- and scroll-synced automatically.
        let map = sourceview5::Map::new();
        map.set_view(&view);
        // The minimap repainting its own current-line highlight in sync
        // with the cursor is what made the editor's highlight flicker.
        map.set_highlight_current_line(false);
        // GtkSourceMap's own pointer handling lands jumps at positions
        // that don't match the click. Map and view share one buffer, so
        // the map-local y converts to an exact buffer line: take over
        // clicks AND slider drags with that math (capture + claim beats
        // the internal gestures) and center the view on the hit line.
        {
            let jump = {
                let map = map.clone();
                let view = view.clone();
                std::rc::Rc::new(move |y: f64| {
                    let (_, by) =
                        map.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, y as i32);
                    let (iter, _) = map.line_at_y(by);
                    let mut iter = iter;
                    view.scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.5);
                })
            };
            let drag = gtk::GestureDrag::new();
            drag.set_propagation_phase(gtk::PropagationPhase::Capture);
            let jump_on_begin = jump.clone();
            drag.connect_drag_begin(move |gesture, _x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                jump_on_begin(y);
            });
            drag.connect_drag_update(move |gesture, _dx, dy| {
                if let Some((_, start_y)) = gesture.start_point() {
                    jump(start_y + dy);
                }
            });
            map.add_controller(drag);
        }
        let edit_body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        edit_body.append(&scroller);
        edit_body.append(&map);

        // The tab's second face: changes since the last commit, with
        // removed lines visible. Filled lazily on first switch.
        let diff_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(6)
            .right_margin(6)
            .build();
        let diff_buffer = diff_view.buffer();
        let diff_scroller = gtk::ScrolledWindow::builder()
            .child(&diff_view)
            .hexpand(true)
            .vexpand(true)
            .build();
        // Third face: the full-quality markdown preview (rendered
        // widgets, not styled source).
        let preview_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let preview_scroller = gtk::ScrolledWindow::builder()
            .child(&preview_holder)
            .hexpand(true)
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.add_named(&edit_body, Some("edit"));
        stack.add_named(&diff_scroller, Some("changes"));
        stack.add_named(&preview_scroller, Some("preview"));

        // Conflict banner: disk changed under unsaved edits. Reload takes
        // the disk version; doing nothing keeps yours (save overwrites).
        let conflict_bar = adw::Banner::builder().button_label("Reload File").build();
        {
            let weak = Rc::downgrade(self);
            let path = path.to_path_buf();
            conflict_bar.connect_button_clicked(move |_| {
                if let Some(editor) = weak.upgrade() {
                    editor.force_reload(&path);
                }
            });
        }
        let page_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page_box.append(&conflict_bar);
        page_box.append(&stack);
        let tab = self.tabs.append(&page_box);
        tab.set_title(
            &path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        tab.set_tooltip(&path.display().to_string());
        tab.set_icon(Some(&file_type_icon(path)));
        set_dirty_dot(&tab, self.git_dirty.borrow().contains_key(path));

        let page = Rc::new(EditorPage {
            page: tab.clone(),
            view,
            buffer: buffer.clone(),
            trim_trailing_ws: Cell::new(false),
            final_newline: Cell::new(true),
            crlf: Cell::new(false),
            eol_override: Cell::new(None),
            bom: Cell::new(false),
            // Markdown defaults to the editing interface (raw source);
            // the preview is opt-in and read-only.
            raw_source: Cell::new(true),
            plain: Cell::new(!highlighting_ok(&content)),
            suggestion: RefCell::new(None),
            restyle_queued: Cell::new(false),
            map,
            changes_view: Cell::new(false),
            stack,
            diff_buffer,
            preview_holder,
            conflict_bar,
            saved_hash: Cell::new(None),
            warned: Cell::new(false),
        });
        self.apply_editorconfig(path, &page);
        self.install_page_keys(path.to_path_buf(), &page);

        // Modified marker on the tab title; dirty state mirrors into the
        // shared IDE state agents read over MCP.
        {
            let tab = tab.clone();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let weak = Rc::downgrade(self);
            buffer.connect_modified_changed(move |buffer| {
                // Native dirty affordance: TabPage's attention indicator,
                // never a title prefix.
                tab.set_title(&name);
                tab.set_needs_attention(buffer.is_modified());
                if let Some(editor) = weak.upgrade() {
                    editor.publish_state();
                }
            });
        }

        // Selection context for agents (capped; cleared when collapsed).
        {
            let workspace = self.workspace.clone();
            let path = path.to_path_buf();
            buffer.connect_mark_set(move |buffer, _iter, mark| {
                let Some(name) = mark.name() else { return };
                if name != "insert" && name != "selection_bound" {
                    return;
                }
                let selection = buffer.selection_bounds().map(|(start, end)| {
                    taste_core::ide_state::Selection {
                        path: path.clone(),
                        start_line: start.line() as u32 + 1,
                        end_line: end.line() as u32 + 1,
                        text: buffer
                            .text(&start, &end, false)
                            .chars()
                            .take(MAX_SELECTION_CAPTURE_CHARS)
                            .collect(),
                    }
                });
                workspace.ide.set_selection(selection);
            });
        }

        // Live WYSIWYG restyle as the user types (styling only).
        {
            let weak = Rc::downgrade(self);
            let path = path.to_path_buf();
            let weak_page = Rc::downgrade(&page);
            buffer.connect_changed(move |_| {
                let (Some(editor), Some(page)) = (weak.upgrade(), weak_page.upgrade()) else {
                    return;
                };
                if !editor.wysiwyg_active(&path, &page) || page.restyle_queued.get() {
                    return;
                }
                // Coalesce typing bursts: restyle at most ~12×/sec instead
                // of on every keystroke — input latency stays flat no
                // matter how large the document.
                page.restyle_queued.set(true);
                let weak = weak.clone();
                let weak_page = Rc::downgrade(&page);
                let path = path.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                    let (Some(editor), Some(page)) = (weak.upgrade(), weak_page.upgrade()) else {
                        return;
                    };
                    page.restyle_queued.set(false);
                    if editor.wysiwyg_active(&path, &page) {
                        editor.refresh_markdown_mode(&path, &page);
                    }
                });
            });
        }

        self.pages
            .borrow_mut()
            .insert(path.to_path_buf(), page.clone());
        page.crlf.set(had_crlf);
        // .editorconfig charset (applied above) wins over detection.
        if !page.bom.get() {
            page.bom.set(had_bom);
        }
        self.refresh_markdown_mode(path, &page);
        page.buffer.set_text(&content);
        page.buffer.set_modified(false);
        self.tabs.set_selected_page(&tab);
        self.sync_toggle_to_selection();
        if let Some(line) = line {
            jump_to_line(&page.view, &page.buffer, line);
        }
        self.publish_state();
    }

    /// Take the disk version: reload over the buffer (the conflict
    /// banner's explicit, user-chosen data loss).
    fn force_reload(self: &Rc<Self>, path: &Path) {
        let Some(page) = self.pages.borrow().get(path).cloned() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let path = path.to_path_buf();
        glib::spawn_future_local(async move {
            let read_path = path.clone();
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || std::fs::read_to_string(&read_path));
            let Ok(Ok(content)) = handle.await else {
                return;
            };
            let Some(editor) = weak.upgrade() else { return };
            // The buffer now mirrors these bytes (see on_file_changed).
            page.saved_hash.set(Some(content_hash(&content)));
            let (content, crlf, bom) = normalize_load(&content);
            page.crlf.set(crlf);
            page.bom.set(bom);
            page.buffer.set_text(&content);
            page.buffer.set_modified(false);
            page.conflict_bar.set_revealed(false);
            page.warned.set(false);
            page.page.set_indicator_icon(gtk::gio::Icon::NONE);
            set_dirty_dot(&page.page, editor.git_dirty.borrow().contains_key(&path));
            page.page.set_tooltip(&path.display().to_string());
            editor.refresh_markdown_mode(&path, &page);
        });
    }

    /// Track uncommitted files (git status, off-thread) and refresh the
    /// tabs' dirty dots.
    pub fn sync_git_state(self: &Rc<Self>) {
        let root = self.workspace.root().to_path_buf();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let git = taste_git::GitWorkspace::discover(&root)?;
                let status = git.status().ok()?;
                let workdir = git.workdir().to_path_buf();
                Some(
                    status
                        .into_iter()
                        .filter(|(_, state)| state.stageable())
                        .map(|(rel, state)| (workdir.join(rel), state))
                        .collect::<HashMap<PathBuf, taste_git::FileState>>(),
                )
            });
            let Ok(Some(dirty)) = handle.await else {
                return;
            };
            let Some(editor) = weak.upgrade() else { return };
            for (path, entry) in editor.pages.borrow().iter() {
                // A warning (conflict, failed save) owns the indicator slot
                // until it is resolved; the dot must not clobber it.
                if entry.warned.get() {
                    continue;
                }
                set_dirty_dot(&entry.page, dirty.contains_key(path));
            }
            *editor.git_dirty.borrow_mut() = dirty;
        });
    }

    /// Flip a tab between its editable face and the changes face.
    fn set_changes_view(self: &Rc<Self>, path: &Path, page: &Rc<EditorPage>, on: bool) {
        page.changes_view.set(on);
        if on {
            page.stack.set_visible_child_name("changes");
            self.refresh_changes(path, page);
        } else {
            page.stack
                .set_visible_child_name(if self.wysiwyg_active(path, page) {
                    "preview"
                } else {
                    "edit"
                });
        }
    }

    /// Rebuild the changes face: HEAD content ↔ current buffer, diffed on
    /// a blocking thread.
    fn refresh_changes(self: &Rc<Self>, path: &Path, page: &Rc<EditorPage>) {
        let now = page
            .buffer
            .text(&page.buffer.start_iter(), &page.buffer.end_iter(), false)
            .to_string();
        let root = self.workspace.root().to_path_buf();
        let file = path.to_path_buf();
        let weak_page = Rc::downgrade(page);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn_blocking(move || {
                let old = taste_git::GitWorkspace::discover(&root)
                    .and_then(|git| {
                        let rel = file.strip_prefix(git.workdir()).ok()?.to_path_buf();
                        git.head_content(&rel)
                    })
                    .unwrap_or_default();
                diff_lines(&old, &now)
            });
            let Ok(lines) = handle.await else { return };
            let Some(page) = weak_page.upgrade() else {
                return;
            };
            if page.changes_view.get() {
                apply_diff_lines(&page.diff_buffer, &lines);
            }
        });
    }

    /// Mirror open files + dirty + active into the shared IDE state.
    fn publish_state(&self) {
        let active = self
            .tabs
            .selected_page()
            .and_then(|tab| self.page_by_tab(&tab).map(|(path, _)| path));
        let files = self
            .pages
            .borrow()
            .iter()
            .map(|(path, page)| taste_core::ide_state::OpenFile {
                path: path.clone(),
                dirty: page.buffer.is_modified(),
                active: Some(path) == active.as_ref(),
            })
            .collect();
        self.workspace.ide.set_open_files(files);
    }

    /// A file changed on disk (agent, container build, terminal). Clean
    /// buffers reload in place; dirty buffers are flagged, never clobbered.
    /// The disk is read BEFORE deciding anything: our own saves echo back
    /// through the watcher, and only the content can tell an echo (not a
    /// conflict, even if the user typed since) from a real external change.
    pub fn on_file_changed(self: &Rc<Self>, path: &Path) {
        if !self.pages.borrow().contains_key(path) {
            return;
        }
        let weak = Rc::downgrade(self);
        let path = path.to_path_buf();
        glib::spawn_future_local(async move {
            let read_path = path.clone();
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || std::fs::read_to_string(&read_path));
            let Ok(Ok(raw)) = handle.await else {
                return; // deleted or unreadable; the tree reflects that
            };
            let Some(editor) = weak.upgrade() else { return };
            let Some(page) = editor.pages.borrow().get(&path).cloned() else {
                return; // closed meanwhile
            };
            if page.saved_hash.get() == Some(content_hash(&raw)) {
                // Our own last save echoing back — or an external write that
                // restored exactly those bytes. Either way disk equals the
                // last save, so a conflict warning no longer applies.
                if page.warned.replace(false) {
                    page.conflict_bar.set_revealed(false);
                    page.page.set_indicator_icon(gtk::gio::Icon::NONE);
                    set_dirty_dot(&page.page, editor.git_dirty.borrow().contains_key(&path));
                    page.page.set_tooltip(&path.display().to_string());
                }
                return;
            }
            if page.buffer.is_modified() {
                page.warned.set(true);
                page.page.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(
                    "dialog-warning-symbolic",
                )));
                page.page.set_tooltip(&format!(
                    "{} changed on disk while you have unsaved edits",
                    path.display()
                ));
                page.conflict_bar.set_title(
                    "Changed on disk under your unsaved edits — \
                     Reload takes the disk version, Save keeps yours",
                );
                page.conflict_bar.set_revealed(true);
                return;
            }
            // The buffer will now mirror these bytes: a later write of the
            // very same content (a revert) must not read as "our echo".
            page.saved_hash.set(Some(content_hash(&raw)));
            let (content, crlf, bom) = normalize_load(&raw);
            // Identical content (a touch, a same-bytes rewrite): a set_text
            // would force a full re-highlight — comments flash white. Skip.
            let current =
                page.buffer
                    .text(&page.buffer.start_iter(), &page.buffer.end_iter(), true);
            if current == content {
                return;
            }
            page.crlf.set(crlf);
            page.bom.set(bom);
            let mark = page.buffer.get_insert();
            let offset = page.buffer.iter_at_mark(&mark).offset();
            dismiss_suggestion(&page);
            // Re-evaluate the performance guard: an agent may have rewritten
            // a small file into a huge one (or vice versa).
            let was_plain = page.plain.get();
            page.plain.set(!highlighting_ok(&content));
            page.buffer.set_text(&content);
            page.buffer.set_modified(false);
            // The reload resolves any earlier conflict warning.
            if page.warned.replace(false) {
                page.conflict_bar.set_revealed(false);
                page.page.set_indicator_icon(gtk::gio::Icon::NONE);
                page.page.set_tooltip(&path.display().to_string());
            }
            let end = page.buffer.char_count();
            page.buffer
                .place_cursor(&page.buffer.iter_at_offset(offset.min(end)));
            if was_plain != page.plain.get() || editor.wysiwyg_active(&path, &page) {
                editor.refresh_markdown_mode(&path, &page);
            }
            if page.changes_view.get() {
                editor.refresh_changes(&path, &page);
            }
        });
    }

    /// Save; on failure the tab is flagged AND the caller learns about it —
    /// a failed save must never let a close path discard the buffer.
    fn save_page(&self, path: &Path, page: &EditorPage) -> Result<(), String> {
        // The write policy binds the user too: in safe mode (devcontainer
        // not running) only the safe-mode scope is editable.
        let safe_mode = !self.workspace.exec.is_container();
        if !taste_core::policy::write_allowed(self.workspace.root(), safe_mode, path) {
            let message = format!(
                "{} is read-only in safe mode — only devcontainer setup and \
                 workspace dotfiles are editable until the devcontainer runs",
                path.display()
            );
            self.flag_save_failure(page, &message);
            return Err(message);
        }
        let (start, end) = page.buffer.bounds();
        let mut text = page.buffer.text(&start, &end, true).to_string();
        if page.trim_trailing_ws.get() {
            text = text
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n");
        }
        if page.final_newline.get() && !text.ends_with('\n') {
            text.push('\n');
        }
        // Restore the file's line endings (or what .editorconfig demands)
        // and its BOM — buffers are LF-only, BOM-free internally.
        if page.eol_override.get().unwrap_or(page.crlf.get()) {
            text = text.replace('\n', "\r\n");
        }
        if page.bom.get() {
            text.insert(0, '\u{feff}');
        }
        // Ghost-flow buffers save into directories that may not exist yet.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, &text) {
            Ok(()) => {
                page.buffer.set_modified(false);
                page.saved_hash.set(Some(content_hash(&text)));
                // Saving resolves a disk conflict in your favor.
                page.conflict_bar.set_revealed(false);
                if page.warned.replace(false) {
                    // The warning covered the git dot; the file was just
                    // written with new content, so dirty is the safe bet
                    // until the next status sync corrects it.
                    page.page.set_indicator_icon(gtk::gio::Icon::NONE);
                    set_dirty_dot(&page.page, true);
                    page.page.set_tooltip(&path.display().to_string());
                }
                // Own changes are announced, not just watched for: the
                // Dirty filter and status badges update immediately.
                self.workspace
                    .events
                    .publish(taste_core::Event::GitStatusChanged);
                Ok(())
            }
            Err(e) => {
                let message = format!("Save failed — {}: {e}", path.display());
                self.flag_save_failure(page, &message);
                Err(message)
            }
        }
    }

    fn flag_save_failure(&self, page: &EditorPage, message: &str) {
        page.warned.set(true);
        page.page.set_tooltip(message);
        page.page
            .set_indicator_icon(Some(&gtk::gio::ThemedIcon::new("dialog-warning-symbolic")));
    }

    /// Apply .editorconfig: indentation on the view now, whitespace policy
    /// recorded for save time.
    fn apply_editorconfig(&self, path: &Path, page: &EditorPage) {
        use ec4rs::property::*;
        let Ok(mut props) = ec4rs::properties_of(path) else {
            return;
        };
        props.use_fallbacks();
        if let Ok(style) = props.get::<IndentStyle>() {
            page.view
                .set_insert_spaces_instead_of_tabs(style == IndentStyle::Spaces);
        }
        match props.get::<IndentSize>() {
            Ok(IndentSize::Value(size)) => page.view.set_indent_width(size as i32),
            // indent_size = tab: follow tab_width.
            Ok(IndentSize::UseTabWidth) => page.view.set_indent_width(-1),
            Err(_) => {}
        }
        if let Ok(TabWidth::Value(width)) = props.get::<TabWidth>() {
            page.view.set_tab_width(width as u32);
        }
        page.trim_trailing_ws
            .set(props.get::<TrimTrailingWs>() == Ok(TrimTrailingWs::Value(true)));
        page.final_newline
            .set(props.get::<FinalNewline>() != Ok(FinalNewline::Value(false)));
        // end_of_line: an explicit setting overrides the detected ending
        // on save (cr-only files are museum pieces; treated as lf).
        page.eol_override.set(match props.get::<EndOfLine>() {
            Ok(EndOfLine::CrLf) => Some(true),
            Ok(EndOfLine::Lf) | Ok(EndOfLine::Cr) => Some(false),
            Err(_) => None,
        });
        // charset: utf-8 vs utf-8-bom decides the BOM on save; other
        // charsets are not re-encoded (buffers are UTF-8).
        match props.get::<Charset>() {
            Ok(Charset::Utf8Bom) => page.bom.set(true),
            Ok(Charset::Utf8) => page.bom.set(false),
            _ => {}
        }
        // max_line_length: the right-margin guide line.
        if let Ok(MaxLineLen::Value(width)) = props.get::<MaxLineLen>() {
            page.view.set_right_margin_position(width as u32);
            page.view.set_show_right_margin(true);
        }
    }

    fn wysiwyg_active(&self, path: &Path, page: &EditorPage) -> bool {
        is_markdown(path)
            && !page.raw_source.get()
            && !page.plain.get()
            && page.buffer.char_count() <= MAX_WYSIWYG_CHARS
    }

    /// Reconcile language + styling with the page's markdown mode.
    fn refresh_markdown_mode(&self, path: &Path, page: &EditorPage) {
        // Minimapping a plain-guard giant file would cost what the guard
        // exists to avoid.
        page.map.set_visible(!page.plain.get());
        if page.plain.get() {
            // Performance guard: no syntax regexes, no restyling.
            page.buffer.set_language(None);
            page.view.set_show_line_numbers(true);
            page.view.set_editable(true);
            return;
        }
        // The edit face is always a real, highlighted source editor.
        let language = sourceview5::LanguageManager::default()
            .guess_language(Some(path.to_string_lossy().as_ref()), None);
        page.buffer.set_language(language.as_ref());
        page.view.set_show_line_numbers(true);
        page.view.set_editable(true);
        if self.wysiwyg_active(path, page) {
            // Full-quality preview: pulldown-cmark rendered into native
            // widgets, with copy affordances on code spans and blocks.
            while let Some(child) = page.preview_holder.first_child() {
                page.preview_holder.remove(&child);
            }
            let text = page
                .buffer
                .text(&page.buffer.start_iter(), &page.buffer.end_iter(), true)
                .to_string();
            let events = self.workspace.events.clone();
            let on_link: std::rc::Rc<dyn Fn(&str)> = std::rc::Rc::new(move |url: &str| {
                events.publish(taste_core::Event::OpenUrlRequested(url.to_string()));
            });
            page.preview_holder
                .append(&crate::markdown_view::render(&text, on_link));
        }
        if !page.changes_view.get() {
            page.stack
                .set_visible_child_name(if self.wysiwyg_active(path, page) {
                    "preview"
                } else {
                    "edit"
                });
        }
    }

    // --- AI ghost text (Tab accepts, Esc dismisses) ---------------------

    /// Show a suggestion in the selected page. Not yet wired to ACP.
    #[allow(dead_code)]
    pub fn show_suggestion(self: &Rc<Self>, text: &str) {
        let Some((_, page)) = self.selected() else {
            return;
        };
        dismiss_suggestion(&page);
        let mark = page.buffer.get_insert();
        let mut iter = page.buffer.iter_at_mark(&mark);
        let offset = iter.offset();
        page.buffer.insert(&mut iter, text);
        let tag = suggestion_tag(&page.buffer);
        let start = page.buffer.iter_at_offset(offset);
        let end = page
            .buffer
            .iter_at_offset(offset + text.chars().count() as i32);
        page.buffer.apply_tag(&tag, &start, &end);
        page.buffer
            .place_cursor(&page.buffer.iter_at_offset(offset));
        *page.suggestion.borrow_mut() = Some((offset, text.to_string()));
    }

    fn install_page_keys(self: &Rc<Self>, path: PathBuf, page: &Rc<EditorPage>) {
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        let weak_page = Rc::downgrade(page);
        controller.connect_key_pressed(move |_, key, _, modifier| {
            let (Some(editor), Some(page)) = (weak.upgrade(), weak_page.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            if key == gtk::gdk::Key::s && modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                // Failure feedback lands on the tab (⚠ + tooltip).
                let _ = editor.save_page(&path, &page);
                return glib::Propagation::Stop;
            }
            if page.suggestion.borrow().is_some() {
                match key {
                    gtk::gdk::Key::Tab => {
                        accept_suggestion(&page);
                        return glib::Propagation::Stop;
                    }
                    gtk::gdk::Key::Escape => {
                        dismiss_suggestion(&page);
                        return glib::Propagation::Stop;
                    }
                    _ => dismiss_suggestion(&page),
                }
            }
            glib::Propagation::Proceed
        });
        page.view.add_controller(controller);
    }
}

/// Compute display lines for the changes face (blocking side).
/// Kinds: '+' added, '-' removed, ' ' context, '@' separator/meta.
fn diff_lines(old: &str, new: &str) -> Vec<(char, String)> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for (index, group) in diff.grouped_ops(3).iter().enumerate() {
        if index > 0 {
            out.push(('@', "⋯".to_string()));
        }
        for op in group {
            for change in diff.iter_changes(op) {
                if out.len() >= MAX_DIFF_LINES {
                    out.push(('@', "… (diff truncated)".to_string()));
                    return out;
                }
                let kind = match change.tag() {
                    ChangeTag::Insert => '+',
                    ChangeTag::Delete => '-',
                    ChangeTag::Equal => ' ',
                };
                out.push((kind, change.value().trim_end_matches('\n').to_string()));
            }
        }
    }
    if out.is_empty() {
        out.push(('@', "No changes since the last commit.".to_string()));
    }
    out
}

fn apply_diff_lines(buffer: &gtk::TextBuffer, lines: &[(char, String)]) {
    buffer.set_text("");
    let table = buffer.tag_table();
    // Translucent backgrounds read on light and dark themes alike.
    let ensure = |name: &str, fg: Option<&str>, bg: Option<&str>| {
        if table.lookup(name).is_some() {
            return;
        }
        let mut builder = gtk::TextTag::builder().name(name);
        if let Some(fg) = fg {
            builder = builder.foreground(fg);
        }
        if let Some(bg) = bg {
            builder = builder.paragraph_background(bg);
        }
        table.add(&builder.build());
    };
    ensure("ws-diff-add", None, Some("rgba(46,194,126,0.18)"));
    ensure("ws-diff-del", None, Some("rgba(192,28,40,0.18)"));
    ensure("ws-diff-meta", Some("#888888"), None);
    let mut end = buffer.end_iter();
    for (kind, text) in lines {
        let start_offset = end.offset();
        let line = match kind {
            '@' => format!("{text}\n"),
            k => format!("{k} {text}\n"),
        };
        buffer.insert(&mut end, &line);
        let tag = match kind {
            '+' => "ws-diff-add",
            '-' => "ws-diff-del",
            '@' => "ws-diff-meta",
            _ => continue,
        };
        let start = buffer.iter_at_offset(start_offset);
        buffer.apply_tag_by_name(tag, &start, &end);
    }
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}

fn jump_to_line(view: &sourceview5::View, buffer: &sourceview5::Buffer, line: u32) {
    let mut iter = buffer
        .iter_at_line(line.saturating_sub(1) as i32)
        .unwrap_or_else(|| buffer.end_iter());
    buffer.place_cursor(&iter);
    view.scroll_to_iter(&mut iter, 0.1, false, 0.0, 0.3);
}

fn accept_suggestion(page: &EditorPage) {
    let Some((offset, text)) = page.suggestion.borrow_mut().take() else {
        return;
    };
    let tag = suggestion_tag(&page.buffer);
    let start = page.buffer.iter_at_offset(offset);
    let end = page
        .buffer
        .iter_at_offset(offset + text.chars().count() as i32);
    page.buffer.remove_tag(&tag, &start, &end);
    page.buffer.place_cursor(&end);
}

fn dismiss_suggestion(page: &EditorPage) {
    let Some((offset, text)) = page.suggestion.borrow_mut().take() else {
        return;
    };
    let mut start = page.buffer.iter_at_offset(offset);
    let mut end = page
        .buffer
        .iter_at_offset(offset + text.chars().count() as i32);
    page.buffer.delete(&mut start, &mut end);
}

fn suggestion_tag(buffer: &sourceview5::Buffer) -> gtk::TextTag {
    let table = buffer.tag_table();
    match table.lookup("ai-suggestion") {
        Some(tag) => tag,
        None => {
            let tag = gtk::TextTag::builder()
                .name("ai-suggestion")
                .foreground("#888888")
                .style(gtk::pango::Style::Italic)
                .build();
            table.add(&tag);
            tag
        }
    }
}

/// Follow the libadwaita dark/light preference with matching Adwaita schemes.
fn apply_scheme_for_style(buffer: &sourceview5::Buffer) {
    let dark = adw::StyleManager::default().is_dark();
    let scheme_id = if dark { "Adwaita-dark" } else { "Adwaita" };
    // Re-setting the same scheme forces a full re-highlight (comments
    // flash unstyled); only act on a real change.
    if buffer.style_scheme().map(|s| s.id().to_string()) == Some(scheme_id.to_string()) {
        return;
    }
    if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(scheme_id) {
        buffer.set_style_scheme(Some(&scheme));
    }
}

/// Buffers hold LF-only text without a BOM; what the file actually had is
/// remembered per page and restored on save.
fn normalize_load(raw: &str) -> (String, bool, bool) {
    let bom = raw.starts_with('\u{feff}');
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let crlf = text.contains("\r\n");
    let clean = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    };
    (clean, crlf, bom)
}

#[cfg(test)]
mod tests {
    use super::highlighting_ok;

    #[test]
    fn normal_source_files_highlight() {
        assert!(highlighting_ok("fn main() {\n    println!(\"hi\");\n}\n"));
    }

    #[test]
    fn very_long_lines_disable_highlighting() {
        let minified = format!("var x = [{}];", "1,".repeat(10_000));
        assert!(!highlighting_ok(&minified));
    }

    #[test]
    fn very_large_files_disable_highlighting() {
        let big = "short line\n".repeat(1_000_000);
        assert!(!highlighting_ok(&big));
    }
}
