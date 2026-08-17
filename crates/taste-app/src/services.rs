//! Services tab: the devcontainer's systemd units — browse them, read and
//! tail their journals, drive their lifecycle, and jump to the unit files
//! (including the activating `.socket` unit) without opening a terminal.
//!
//! All systemctl/journalctl work happens off the main thread; the tail is
//! a runtime-side process feeding a channel. When the container is down
//! the pane stays fully drawn and merely disables itself.

use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use taste_core::Workspace;
use taste_devcontainer::services::{self, JournalTail, ServiceAction, ServiceUnit, UnitFile};

const SNAPSHOT_LINES: u32 = 300;
const MAX_LOG_LINES: i32 = 2000;
const MAX_SERVICE_ROWS: usize = 400;

pub struct ServicesPane {
    pub widget: gtk::Box,
    workspace: Workspace,
    list: gtk::ListBox,
    status: gtk::Label,
    search: gtk::SearchEntry,
    unit_label: gtk::Label,
    socket_label: gtk::Label,
    log_view: gtk::TextView,
    end_mark: gtk::TextMark,
    follow: gtk::ToggleButton,
    action_buttons: Vec<gtk::Button>,
    unit_files_button: gtk::Button,
    units: RefCell<Vec<ServiceUnit>>,
    /// `None` = the whole system journal (the pinned first row).
    selected: RefCell<Option<String>>,
    tail: RefCell<Option<JournalTail>>,
    /// Bumped on every view change; stale tail loops see it and stop.
    generation: Cell<u64>,
    connected: Cell<bool>,
    syncing: Cell<bool>,
}

impl ServicesPane {
    pub fn new(workspace: Workspace) -> Rc<Self> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(6);
        header.set_margin_end(6);
        let heading = gtk::Label::builder()
            .label("Services")
            .css_classes(["heading"])
            .xalign(0.0)
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter services")
            .hexpand(true)
            .build();
        let refresh_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh the service list")
            .css_classes(["flat"])
            .build();
        header.append(&heading);
        header.append(&search);
        header.append(&refresh_button);

        let status = gtk::Label::builder()
            .label("")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(4)
            .build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        let list_scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .width_request(220)
            .vexpand(true)
            .build();

        // Right side: unit header with lifecycle actions, journal below.
        let unit_label = gtk::Label::builder()
            .label("System journal")
            .css_classes(["heading"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let socket_label = gtk::Label::builder()
            .label("")
            .css_classes(["caption", "success"])
            .xalign(0.0)
            .build();
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        titles.set_hexpand(true);
        titles.append(&unit_label);
        titles.append(&socket_label);

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        // Icon buttons: this bar sets the console's minimum width, and
        // labeled buttons would make the whole window unable to shrink.
        let mut action_buttons = Vec::new();
        for (action, icon) in [
            (ServiceAction::Start, "media-playback-start-symbolic"),
            (ServiceAction::Stop, "media-playback-stop-symbolic"),
            (ServiceAction::Restart, "view-refresh-symbolic"),
            (ServiceAction::Reload, "emblem-synchronizing-symbolic"),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(action.label())
                .build();
            actions.append(&button);
            action_buttons.push((action, button));
        }
        let unit_files_button = gtk::Button::builder()
            .icon_name("document-properties-symbolic")
            .tooltip_text("View unit files (service and its socket)")
            .build();
        let follow = gtk::ToggleButton::builder()
            .icon_name("go-bottom-symbolic")
            .tooltip_text("Follow the journal live")
            .active(true)
            .build();

        let unit_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        unit_bar.set_margin_top(6);
        unit_bar.set_margin_bottom(6);
        unit_bar.set_margin_start(12);
        unit_bar.set_margin_end(6);
        unit_bar.append(&titles);
        unit_bar.append(&actions);
        unit_bar.append(&unit_files_button);
        unit_bar.append(&follow);

        let log_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(6)
            .right_margin(6)
            .build();
        let end_mark = log_view
            .buffer()
            .create_mark(None, &log_view.buffer().end_iter(), false);
        let log_scroller = gtk::ScrolledWindow::builder()
            .child(&log_view)
            .vexpand(true)
            .hexpand(true)
            .build();

        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.append(&unit_bar);
        right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        right.append(&log_scroller);

        let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        split.set_vexpand(true);
        split.append(&list_scroller);
        split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        split.append(&right);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&header);
        widget.append(&status);
        widget.append(&split);

        let pane = Rc::new(Self {
            widget,
            workspace,
            list,
            status,
            search,
            unit_label,
            socket_label,
            log_view,
            end_mark,
            follow,
            action_buttons: action_buttons.iter().map(|(_, b)| b.clone()).collect(),
            unit_files_button,
            units: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            tail: RefCell::new(None),
            generation: Cell::new(0),
            connected: Cell::new(false),
            syncing: Cell::new(false),
        });

        // Filter rows by name/description (stored as the row widget name);
        // the journal row (index 0) always shows.
        let search_ref = pane.search.clone();
        pane.list.set_filter_func(move |row| {
            let query = search_ref.text().to_lowercase();
            query.is_empty() || row.index() == 0 || row.widget_name().contains(&query)
        });
        let weak = Rc::downgrade(&pane);
        pane.search.connect_search_changed(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.list.invalidate_filter();
            }
        });

        let weak = Rc::downgrade(&pane);
        refresh_button.connect_clicked(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.refresh();
            }
        });

        let weak = Rc::downgrade(&pane);
        pane.list.connect_row_selected(move |_, row| {
            let Some(pane) = weak.upgrade() else { return };
            if pane.syncing.get() {
                return;
            }
            let selected = row.and_then(|row| {
                let index = row.index();
                (index > 0)
                    .then(|| {
                        pane.units
                            .borrow()
                            .get(index as usize - 1)
                            .map(|u| u.name.clone())
                    })
                    .flatten()
            });
            *pane.selected.borrow_mut() = selected;
            pane.sync_unit_header();
            pane.load_view();
        });

        let weak = Rc::downgrade(&pane);
        pane.follow.connect_toggled(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.load_view();
            }
        });

        for (action, button) in &action_buttons {
            let weak = Rc::downgrade(&pane);
            let action = *action;
            button.connect_clicked(move |_| {
                if let Some(pane) = weak.upgrade() {
                    pane.run_action(action);
                }
            });
        }

        let weak = Rc::downgrade(&pane);
        pane.unit_files_button.connect_clicked(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.show_unit_files();
            }
        });

        pane.set_connected(false, "Start the devcontainer to browse its services.");
        pane
    }

    fn set_connected(&self, connected: bool, note: &str) {
        self.connected.set(connected);
        self.status.set_label(note);
        // Disable, never hide: the pane keeps its full shape when the
        // container is down or systemd is absent.
        self.list.set_sensitive(connected);
        self.search.set_sensitive(connected);
        self.follow.set_sensitive(connected);
        self.sync_unit_header();
    }

    fn sync_unit_header(&self) {
        let selected = self.selected.borrow();
        let connected = self.connected.get();
        match selected.as_deref() {
            Some(unit) => {
                self.unit_label.set_label(unit);
                let socket = self
                    .units
                    .borrow()
                    .iter()
                    .find(|u| u.name == unit)
                    .and_then(|u| u.socket.clone());
                self.socket_label.set_label(
                    &socket
                        .map(|s| format!("socket-activated · {s}"))
                        .unwrap_or_default(),
                );
            }
            None => {
                self.unit_label.set_label("System journal");
                self.socket_label.set_label("");
            }
        }
        let unit_selected = connected && selected.is_some();
        for button in &self.action_buttons {
            button.set_sensitive(unit_selected);
        }
        self.unit_files_button.set_sensitive(unit_selected);
    }

    /// Re-query the container's units. Cheap to call on any devcontainer
    /// state change; failures degrade to a disabled pane with the reason.
    pub fn refresh(self: &Rc<Self>) {
        if !self.workspace.exec.is_container() {
            self.set_connected(false, "Start the devcontainer to browse its services.");
            return;
        }
        let exec = self.workspace.exec.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle =
                crate::runtime::runtime().spawn_blocking(move || services::list_services(&exec));
            let Ok(result) = handle.await else { return };
            let Some(pane) = weak.upgrade() else { return };
            match result {
                Ok(units) => {
                    let first_load = !pane.connected.get();
                    pane.set_connected(true, "");
                    pane.workspace
                        .events
                        .publish(taste_core::Event::ServiceSummary {
                            total: units.len(),
                            failed: units.iter().filter(|u| u.is_failed()).count(),
                        });
                    pane.render_units(units);
                    if first_load {
                        pane.load_view();
                    }
                }
                Err(e) => {
                    let first_line = e.to_string();
                    let first_line = first_line.lines().next().unwrap_or("error").to_string();
                    pane.set_connected(
                        false,
                        &format!(
                            "systemd is not available here ({first_line}) — run a systemd image \
                             with overrideCommand false to manage services"
                        ),
                    );
                }
            }
        });
    }

    fn render_units(self: &Rc<Self>, units: Vec<ServiceUnit>) {
        let previous = self.selected.borrow().clone();
        self.syncing.set(true);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let journal_row = gtk::ListBoxRow::new();
        let journal_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        journal_box.append(&gtk::Image::from_icon_name("format-justify-left-symbolic"));
        journal_box.append(
            &gtk::Label::builder()
                .label("System journal")
                .xalign(0.0)
                .build(),
        );
        journal_row.set_child(Some(&journal_box));
        self.list.append(&journal_row);

        let shown = units.len().min(MAX_SERVICE_ROWS);
        for unit in units.iter().take(shown) {
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&format!("{} {}", unit.name, unit.description).to_lowercase());
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let dot = gtk::Image::from_icon_name("media-record-symbolic");
            dot.add_css_class(if unit.is_failed() {
                "error"
            } else if unit.is_running() {
                "success"
            } else {
                "dim-label"
            });
            let names = gtk::Box::new(gtk::Orientation::Vertical, 0);
            names.set_hexpand(true);
            let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            name_line.append(
                &gtk::Label::builder()
                    .label(&unit.name)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
            if let Some(socket) = &unit.socket {
                let badge = gtk::Image::from_icon_name("network-transmit-receive-symbolic");
                badge.add_css_class("accent");
                badge.set_tooltip_text(Some(&format!("Socket-activated by {socket}")));
                name_line.append(&badge);
            }
            names.append(&name_line);
            names.append(
                &gtk::Label::builder()
                    .label(&unit.description)
                    .xalign(0.0)
                    .css_classes(["dim-label", "caption"])
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let state = gtk::Label::builder()
                .label(&unit.sub)
                .css_classes(["dim-label", "caption"])
                .build();
            hbox.append(&dot);
            hbox.append(&names);
            hbox.append(&state);
            row.set_child(Some(&hbox));
            self.list.append(&row);
        }
        if units.len() > shown {
            let more = gtk::Label::builder()
                .label(format!(
                    "… {} more (showing the first {shown})",
                    units.len() - shown
                ))
                .css_classes(["dim-label", "caption"])
                .margin_top(6)
                .margin_bottom(6)
                .build();
            self.list.append(
                &gtk::ListBoxRow::builder()
                    .child(&more)
                    .activatable(false)
                    .build(),
            );
        }
        *self.units.borrow_mut() = units;

        // Restore the previous selection without re-triggering a journal
        // reload; fall back to the journal row.
        let index = previous
            .as_deref()
            .and_then(|name| self.units.borrow().iter().position(|u| u.name == name))
            .map(|i| i as i32 + 1)
            .unwrap_or(0);
        if let Some(row) = self.list.row_at_index(index) {
            self.list.select_row(Some(&row));
        }
        *self.selected.borrow_mut() =
            previous.filter(|name| self.units.borrow().iter().any(|u| &u.name == name));
        self.syncing.set(false);
        self.sync_unit_header();
    }

    /// (Re)load the journal panel for the current selection: a live tail
    /// when Follow is on, a bounded snapshot otherwise.
    fn load_view(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        if let Some(tail) = self.tail.borrow_mut().take() {
            tail.stop();
        }
        self.log_view.buffer().set_text("");
        if !self.connected.get() {
            return;
        }
        let unit = self.selected.borrow().clone();
        let exec = self.workspace.exec.clone();
        if self.follow.is_active() {
            let tail = services::tail_journal(
                crate::runtime::runtime().handle(),
                &exec,
                unit,
                SNAPSHOT_LINES,
            );
            let lines = tail.lines.clone();
            *self.tail.borrow_mut() = Some(tail);
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                while let Ok(first) = lines.recv().await {
                    let Some(pane) = weak.upgrade() else { break };
                    if pane.generation.get() != generation {
                        break;
                    }
                    // Coalesce bursts into one buffer edit per wakeup.
                    let mut chunk = vec![first];
                    while chunk.len() < 256 {
                        match lines.try_recv() {
                            Ok(line) => chunk.push(line),
                            Err(_) => break,
                        }
                    }
                    pane.append_log(&chunk);
                }
            });
        } else {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let handle = crate::runtime::runtime().spawn_blocking(move || {
                    services::journal_snapshot(&exec, unit.as_deref(), SNAPSHOT_LINES)
                });
                let Ok(result) = handle.await else { return };
                let Some(pane) = weak.upgrade() else { return };
                if pane.generation.get() != generation {
                    return;
                }
                match result {
                    Ok(text) => {
                        pane.log_view.buffer().set_text(&text);
                        pane.scroll_to_end();
                    }
                    Err(e) => pane.log_view.buffer().set_text(&format!("journal: {e:#}")),
                }
            });
        }
    }

    fn append_log(&self, lines: &[String]) {
        let buffer = self.log_view.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, &format!("{}\n", lines.join("\n")));
        let extra = buffer.line_count() - MAX_LOG_LINES;
        if extra > 0 {
            let mut start = buffer.start_iter();
            if let Some(mut cut) = buffer.iter_at_line(extra) {
                buffer.delete(&mut start, &mut cut);
            }
        }
        self.scroll_to_end();
    }

    fn scroll_to_end(&self) {
        self.log_view
            .scroll_to_mark(&self.end_mark, 0.0, true, 0.0, 1.0);
    }

    fn run_action(self: &Rc<Self>, action: ServiceAction) {
        let Some(unit) = self.selected.borrow().clone() else {
            return;
        };
        let exec = self.workspace.exec.clone();
        let events = self.workspace.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let unit_for_job = unit.clone();
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || services::service_action(&exec, action, &unit_for_job));
            let Ok(result) = handle.await else { return };
            match result {
                Ok(()) => events.publish(taste_core::Event::Toast(format!(
                    "{} — {unit}",
                    action.label()
                ))),
                Err(e) => events.publish(taste_core::Event::Toast(format!(
                    "{} {unit} failed: {e:#}",
                    action.label()
                ))),
            }
            if let Some(pane) = weak.upgrade() {
                pane.refresh();
                if !pane.follow.is_active() {
                    pane.load_view();
                }
            }
        });
    }

    /// Fetch and show the unit files behind the selected service — the
    /// service fragment, drop-ins, and its `.socket` unit — with a jump to
    /// the editor for any file that lives in the workspace.
    fn show_unit_files(self: &Rc<Self>) {
        let Some(unit) = self.selected.borrow().clone() else {
            return;
        };
        let exec = self.workspace.exec.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let unit_for_job = unit.clone();
            let handle = crate::runtime::runtime()
                .spawn_blocking(move || services::unit_files(&exec, &unit_for_job));
            let Ok(result) = handle.await else { return };
            let Some(pane) = weak.upgrade() else { return };
            match result {
                Ok(files) if !files.is_empty() => pane.present_unit_files(&unit, &files),
                Ok(_) => pane
                    .workspace
                    .events
                    .publish(taste_core::Event::Toast(format!(
                        "No unit files found for {unit}"
                    ))),
                Err(e) => pane
                    .workspace
                    .events
                    .publish(taste_core::Event::Toast(format!("Unit files: {e:#}"))),
            }
        });
    }

    /// Container path → workspace path, when the file lives in the mounted
    /// workspace (`/workspaces/<name>` convention, or identical paths in
    /// the self-hosting case).
    fn workspace_local(&self, container_path: &str) -> Option<std::path::PathBuf> {
        let root = self.workspace.root().to_path_buf();
        let path = Path::new(container_path);
        if path.starts_with(&root) && path.exists() {
            return Some(path.to_path_buf());
        }
        let name = root.file_name()?.to_str()?;
        let rel = path.strip_prefix(format!("/workspaces/{name}")).ok()?;
        let local = root.join(rel);
        local.exists().then_some(local)
    }

    fn present_unit_files(self: &Rc<Self>, unit: &str, files: &[UnitFile]) {
        let dialog = adw::Dialog::builder()
            .title(format!("Unit files — {unit}"))
            .content_width(700)
            .content_height(540)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        for file in files {
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let path_label = gtk::Label::builder()
                .label(format!("{} — {}", file.unit, file.path))
                .css_classes(["heading"])
                .xalign(0.0)
                .hexpand(true)
                .selectable(true)
                .ellipsize(gtk::pango::EllipsizeMode::Start)
                .build();
            head.append(&path_label);
            let open = gtk::Button::with_label("Open in Editor");
            let local = self.workspace_local(&file.path);
            open.set_sensitive(local.is_some());
            open.set_tooltip_text(Some(if local.is_some() {
                "This unit file lives in the workspace"
            } else {
                "Only files inside the workspace open in the editor"
            }));
            if let Some(local) = local {
                let events = self.workspace.events.clone();
                let dialog_weak = dialog.downgrade();
                open.connect_clicked(move |_| {
                    events.publish(taste_core::Event::OpenFileRequested {
                        path: local.clone(),
                        line: None,
                    });
                    if let Some(dialog) = dialog_weak.upgrade() {
                        dialog.close();
                    }
                });
            }
            head.append(&open);
            let view = gtk::TextView::builder()
                .editable(false)
                .cursor_visible(false)
                .monospace(true)
                .left_margin(6)
                .right_margin(6)
                .top_margin(6)
                .bottom_margin(6)
                .build();
            view.buffer().set_text(file.content.trim_end());
            let frame = gtk::Frame::builder().child(&view).build();
            let section = gtk::Box::new(gtk::Orientation::Vertical, 4);
            section.append(&head);
            section.append(&frame);
            content.append(&section);
        }
        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .vexpand(true)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(&self.widget));
    }
}
