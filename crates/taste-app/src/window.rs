//! The one window arrangement: files left, editor center, console bottom,
//! AI chat right. Resizable and collapsible; never rearrangeable.

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;
use taste_core::event::FlatpakStateEvent;
use taste_core::{Event, Workspace};
use taste_devcontainer::Supervisor;
use taste_flatpak::Packager;
use taste_mcp::McpServer;

use crate::chat::ChatPane;
use crate::console::Console;
use crate::devcontainer_ui::DevcontainerBanner;
use crate::editor::Editor;
use crate::filetree::FileTree;
use crate::runtime::runtime;

pub fn build_window(app: &adw::Application, root: PathBuf) -> adw::ApplicationWindow {
    // A TASTE_PROBE_CHECK instance is scaffolding, not a session: it must
    // observe (render, measure, quit) without leaving a footprint. One of
    // these once saved its empty state over a real window's on the same
    // workspace — and worse, session/load'ed the user's LIVE conversation
    // from a throwaway process.
    let probe_mode = std::env::var("TASTE_PROBE_CHECK").is_ok();
    let workspace = Workspace::open(root.clone());

    // --- background services -------------------------------------------
    let supervisor = Supervisor::new(
        root.clone(),
        workspace.events.clone(),
        workspace.exec.clone(),
    );

    // Flatpak packaging: build/install/launch is a first-class, USER-
    // triggered task (agents get read-only status/logs over MCP).
    let packager = Packager::new(root.clone(), workspace.events.clone());

    let socket = taste_mcp::socket_path(&supervisor.container_name());
    let server = McpServer::new(supervisor.clone(), packager.clone(), workspace.clone());
    let server_socket = socket.clone();
    runtime().spawn(async move {
        if let Err(e) = server.serve(server_socket).await {
            tracing::warn!("MCP server exited: {e:#}");
        }
    });

    // Agents reach the MCP server through our own binary's bridge mode.
    let bridge_command = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "taste-ide".into());
    let mcp_bridge = (
        bridge_command,
        vec!["--mcp-bridge".into(), socket.display().to_string()],
    );

    // --- panes -----------------------------------------------------------
    let editor = Editor::new(workspace.clone());
    let filetree = FileTree::new(workspace.clone());
    {
        let editor = editor.clone();
        filetree.set_on_open(move |path, line| editor.open_at(&path, line));
    }
    {
        // Changed-list rows open as diffs: the tab lands on its Changes face.
        let editor = editor.clone();
        filetree.set_on_open_diff(move |path| editor.open_changes(&path));
    }
    let console = Console::new(workspace.clone(), supervisor.clone());
    let chat = ChatPane::new(workspace.clone(), mcp_bridge, socket.clone());
    {
        // The ✨ button by the commit entry: staged diff → chat agent →
        // suggested message (the exchange stays visible in the transcript).
        let chat = chat.clone();
        filetree.set_commit_suggester(move |prompt, on_done| {
            chat.request_text(prompt, on_done);
        });
    }
    let banner = DevcontainerBanner::new(supervisor.clone(), workspace.events.clone());

    // shrink_*_child(false) everywhere: panes stop at their children's
    // minimum sizes instead of clipping their content.
    let center = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&editor.widget)
        .end_child(&console.widget)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .position(560)
        .build();

    let center_and_chat = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&center)
        .end_child(&chat.widget)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .position(980)
        .build();

    let outer = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&filetree.widget)
        .end_child(&center_and_chat)
        .resize_start_child(false)
        .resize_end_child(true)
        // shrink stays false here too: shrinkable panes get allocated
        // below their minimum and CLIP (measured: the tree lost its left
        // edge, the chat its Send button). Tiling is enabled by keeping
        // the real minimums small instead — TASTE_MEASURE_MIN=1 audits
        // them.
        .shrink_start_child(false)
        .shrink_end_child(false)
        .wide_handle(true)
        .position(260)
        .build();

    let title = adw::WindowTitle::new(
        &root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "taste".into()),
        "Taste IDE",
    );
    // Opinionated chrome: no minimize button. An IDE session is something
    // you're in or you close; maximize and close remain.
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        .decoration_layout(":maximize,close")
        .build();
    let app_icon = gtk::Image::from_icon_name(crate::APP_ID);
    app_icon.set_pixel_size(20);
    app_icon.set_margin_start(6);
    header.pack_start(&app_icon);
    // File navigation lives with the window chrome, right of the carrot.
    header.pack_start(&editor.back_button);
    header.pack_start(&editor.forward_button);
    // Primary menu — the HIG staple every GNOME window carries.
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Taste"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    header.pack_end(&menu_button);

    // The deploy button: build the workspace's Flatpak, install it into the
    // user installation, launch it. Visible only when a manifest exists.
    let flatpak_button = gtk::Button::builder()
        .icon_name("package-x-generic-symbolic")
        .tooltip_text("Build, install, and run as Flatpak")
        .visible(packager.manifest().is_some())
        .build();
    {
        let packager = packager.clone();
        flatpak_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            let spinner = gtk::Spinner::new();
            spinner.start();
            button.set_child(Some(&spinner));
            // Manifest may have been created since startup (e.g. a ghost).
            packager.rediscover();
            let packager = packager.clone();
            runtime().spawn(async move {
                // Failures surface as a toast via the FlatpakState event.
                let _ = packager.build_install_launch(true).await;
            });
        });
    }
    header.pack_end(&flatpak_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.add_top_bar(&banner.widget);
    toolbar_view.set_content(Some(&outer));

    // Toasts: transient action outcomes (commit/push/sync failures and the
    // like) surface here via Event::Toast, never only in logs.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1440)
        .default_height(900)
        .content(&toast_overlay)
        .build();

    // Debug harness: TASTE_MEASURE_MIN=1 prints every pane's minimum width
    // after first map, then quits. Minimums decide whether GNOME will tile
    // the window to half a screen, so keep them measurable.
    if std::env::var("TASTE_MEASURE_MIN").is_ok() {
        let report: Vec<(&str, gtk::Widget)> = vec![
            ("window", window.clone().upcast()),
            ("filetree", filetree.widget.clone().upcast()),
            ("center(editor+console)", center.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
            ("chat", chat.widget.clone().upcast()),
        ];
        let app = app.clone();
        window.connect_map(move |_| {
            let report = report.clone();
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                for (name, widget) in &report {
                    let (min, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    println!("min-width {name}: min={min} nat={natural}");
                }
                // Walk the console tree to attribute its minimum.
                fn walk(widget: &gtk::Widget, depth: usize) {
                    let (min, _, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                    if min > 300 {
                        println!("{}{} min={min}", "  ".repeat(depth), widget.type_().name());
                    }
                    if depth < 8 {
                        let mut child = widget.first_child();
                        while let Some(current) = child {
                            walk(&current, depth + 1);
                            child = current.next_sibling();
                        }
                    }
                }
                if let Some((_, console)) = report.iter().find(|(n, _)| *n == "console") {
                    walk(console, 0);
                }
                app.quit();
            });
        });
    }

    // Debug harness: TASTE_PROBE_CHECK=1 exercises the agents' UI probe
    // (ide_screenshot / ide_widget_geometry) through the REAL channel and
    // responder after first map, writes the PNGs under /tmp, prints the
    // geometry, then quits. Runs headless under gtk4-broadwayd — see
    // build-aux/headless/broadway-client.py for the full recipe.
    if probe_mode {
        // Colors only show against text: without this the composer
        // screenshot is an empty wash whatever the theme does.
        chat.seed_composer_for_probe("Sample prompt text — theme check");
        let ui = workspace.ui.clone();
        let app = app.clone();
        window.connect_map(move |_| {
            let ui = ui.clone();
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                glib::spawn_future_local(async move {
                    use taste_core::ui_probe::{UiReply, UiRequest};
                    for target in ["window", "chat", "chat.composer", "no-such-pane"] {
                        // "Not drawn yet" is timing, not failure: retry the
                        // way an agent would, briefly.
                        for attempt in 0..10 {
                            let request = UiRequest::Screenshot {
                                target: target.into(),
                            };
                            match ui.request(request).await {
                                Ok(UiReply::Screenshot { png, width, height }) => {
                                    let path =
                                        format!("/tmp/probe-{}.png", target.replace('.', "-"));
                                    let _ = std::fs::write(&path, &png);
                                    println!(
                                        "screenshot {target}: {width}x{height}, {} bytes -> {path}",
                                        png.len()
                                    );
                                    break;
                                }
                                Ok(UiReply::Error(e)) if e.contains("not been drawn") => {
                                    if attempt == 9 {
                                        println!("screenshot {target}: ERROR {e}");
                                    } else {
                                        glib::timeout_future(std::time::Duration::from_millis(500))
                                            .await;
                                        continue;
                                    }
                                }
                                Ok(UiReply::Error(e)) => {
                                    println!("screenshot {target}: ERROR {e}");
                                    break;
                                }
                                Ok(_) => {
                                    println!("screenshot {target}: unexpected reply");
                                    break;
                                }
                                Err(e) => {
                                    println!("screenshot {target}: channel error {e}");
                                    break;
                                }
                            }
                        }
                    }
                    for target in ["chat.composer", "console"] {
                        let request = UiRequest::Geometry {
                            target: target.into(),
                        };
                        match ui.request(request).await {
                            Ok(UiReply::Geometry(value)) => {
                                println!(
                                    "geometry {target}:\n{}",
                                    serde_json::to_string_pretty(&value).unwrap_or_default()
                                );
                            }
                            Ok(UiReply::Error(e)) => println!("geometry {target}: ERROR {e}"),
                            Ok(_) => println!("geometry {target}: unexpected reply"),
                            Err(e) => println!("geometry {target}: channel error {e}"),
                        }
                    }
                    app.quit();
                });
            });
        });
    }

    // Returning to the window clears informational notifications (turn
    // finished, disconnect); ones still awaiting a response (permission,
    // sign-in) stay until actually resolved.
    window.connect_is_active_notify(|window| {
        if window.is_active() {
            if let Some(app) = window.application() {
                app.withdraw_notification("taste-turn");
                app.withdraw_notification("taste-disconnect");
            }
        }
    });

    // Stock editor shortcuts: Ctrl+W closes the current tab, Ctrl+F
    // focuses find-in-project.
    {
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Global);
        let editor_for_close = editor.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>w"),
            Some(gtk::CallbackAction::new(move |_, _| {
                editor_for_close.close_current();
                glib::Propagation::Stop
            })),
        ));
        let filetree_for_search = filetree.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| {
                filetree_for_search.focus_search();
                glib::Propagation::Stop
            })),
        ));
        // Ctrl+P: quick-open over the background file index.
        let filetree_for_open = filetree.clone();
        let editor_for_open = editor.clone();
        let window_for_open = window.clone();
        let root_for_open = root.clone();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>p"),
            Some(gtk::CallbackAction::new(move |_, _| {
                present_quick_open(
                    &window_for_open,
                    &root_for_open,
                    filetree_for_open.index_files(),
                    editor_for_open.clone(),
                );
                glib::Propagation::Stop
            })),
        ));
        // Ctrl+Q: quit through the graceful close path.
        let window_for_quit = window.downgrade();
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>q"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if let Some(window) = window_for_quit.upgrade() {
                    window.close();
                }
                glib::Propagation::Stop
            })),
        ));
        window.add_controller(shortcuts);
    }

    // Primary-menu actions.
    {
        let about = gtk::gio::SimpleAction::new("about", None);
        let window_ref = window.clone();
        about.connect_activate(move |_, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Taste")
                .application_icon(crate::APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .developer_name("David Strauss")
                .comments("In an era of AI software authoring, all that's left is taste.")
                .build();
            dialog.present(Some(&window_ref));
        });
        window.add_action(&about);

        let shortcuts_action = gtk::gio::SimpleAction::new("shortcuts", None);
        let window_ref = window.clone();
        shortcuts_action.connect_activate(move |_, _| {
            present_shortcuts_dialog(&window_ref);
        });
        window.add_action(&shortcuts_action);
    }

    // Agents' eyes on the UI: the probe responder behind ide_screenshot
    // and ide_widget_geometry. Pane names here are the tools' contract.
    crate::ui_probe::attach(
        &workspace,
        vec![
            ("window", window.clone().upcast()),
            ("filetree", filetree.widget.clone().upcast()),
            ("editor", editor.widget.clone().upcast()),
            ("console", console.widget.clone().upcast()),
            ("chat", chat.widget.clone().upcast()),
        ],
    );

    // Agent URL bridge: sandboxed sign-in flows (e.g. Claude Code's OAuth)
    // can't open a browser themselves; their $BROWSER helper drops URLs
    // here, and we open them host-side after the user confirms.
    start_url_bridge(&window);

    // --- workspace watcher: external edits become visible ----------------
    // Kept alive for the window's lifetime (leak is deliberate: one window,
    // one process).
    match taste_core::watcher::start(root.clone(), workspace.events.clone()) {
        Ok(watcher) => {
            Box::leak(Box::new(watcher));
        }
        Err(e) => tracing::warn!("workspace watcher failed to start: {e:#}"),
    }

    // Ctrl+C on the launching console (SIGINT) and container stop
    // (SIGTERM) close the window gracefully — the same path as the close
    // button, so state persists.
    {
        let events = workspace.events.clone();
        crate::runtime::runtime().spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut int), Ok(mut term)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
            ) else {
                return;
            };
            loop {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {}
                }
                events.publish(Event::QuitRequested);
            }
        });
    }

    // --- event pump: tokio-side services → GTK --------------------------
    let events = workspace.events.subscribe();
    {
        let filetree = filetree.clone();
        let console = console.clone();
        let banner = banner.clone();
        let editor = editor.clone();
        let chat = chat.clone();
        let packager = packager.clone();
        let root = root.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    Event::GitStatusChanged => {
                        filetree.on_git_status_changed();
                        editor.sync_git_state();
                    }
                    Event::FileChanged(path) => {
                        editor.on_file_changed(&path);
                        filetree.on_git_status_changed();
                        editor.sync_git_state();
                    }
                    Event::FileTreeChanged => {
                        filetree.refresh_tree();
                        filetree.rebuild_index();
                        editor.sync_git_state();
                        // A manifest may have appeared (ghost, agent, git
                        // pull): the deploy button follows reality.
                        flatpak_button.set_visible(packager.rediscover().is_some());
                    }
                    Event::OpenFileRequested { path, line } => {
                        editor.open_at(&path, line);
                    }
                    Event::DevcontainerPendingChanges { pending } => {
                        banner.on_pending_changes(pending);
                        console.set_pending_rebuild(pending);
                    }
                    Event::DevcontainerState(state) => {
                        console.set_container_state(matches!(
                            state,
                            taste_core::event::DevcontainerStateEvent::Running { .. }
                        ));
                        banner.on_state(&state);
                        // Mode may have flipped (safe ↔ container): restyle
                        // the tree's read-only locks and re-query resources.
                        filetree.on_git_status_changed();
                        console.refresh_resources();
                    }
                    Event::DevcontainerLog(line) => console.append_supervisor_log(&line),
                    Event::FlatpakLog(line) => console.append_flatpak_log(&line),
                    Event::FlatpakState(state) => {
                        // Re-arm the deploy button when the pipeline settles.
                        let done = matches!(
                            state,
                            FlatpakStateEvent::Succeeded | FlatpakStateEvent::Failed { .. }
                        );
                        if done {
                            flatpak_button.set_sensitive(true);
                            flatpak_button.set_icon_name("package-x-generic-symbolic");
                        }
                        match state {
                            FlatpakStateEvent::Failed { message } => {
                                console.append_flatpak_log(&format!("FAILED: {message}"));
                                toast_overlay
                                    .add_toast(adw::Toast::new(&format!("Flatpak: {message}")));
                            }
                            FlatpakStateEvent::Succeeded => {
                                toast_overlay
                                    .add_toast(adw::Toast::new("Flatpak installed and launched"));
                            }
                            _ => {}
                        }
                    }
                    Event::RunInTerminal {
                        title,
                        program,
                        args,
                        env,
                        wrapped,
                    } => {
                        console.add_command_tab(&title, &program, &args, &env, wrapped);
                    }
                    Event::ShowDevcontainerLog => console.show_devcontainer_log(),
                    Event::CreateDevcontainerConfig => {
                        filetree.create_ghost(&root.join(".devcontainer/devcontainer.json"));
                    }
                    Event::CreateFileRequested { path, content } => {
                        editor.open_unsaved(&path, content);
                    }
                    Event::ServiceSummary { total, failed } => {
                        console.update_service_summary(total, failed);
                    }
                    Event::ServicesUnavailable { systemd_missing } => {
                        console.set_services_unavailable(systemd_missing);
                    }
                    Event::CommandTabExited { title, status } => {
                        if title == "Sign In" {
                            chat.on_sign_in_finished(status == 0);
                        }
                    }
                    Event::QuitRequested => {
                        if let Some(window) = editor.widget.root().and_downcast::<gtk::Window>() {
                            window.close();
                        }
                    }
                    Event::OpenUrlRequested(url) => {
                        if !(url.starts_with("https://") || url.starts_with("http://")) {
                            continue; // terminals print all sorts of things
                        }
                        open_url(&url, &toast_overlay);
                    }
                    Event::Toast(message) => {
                        toast_overlay.add_toast(adw::Toast::new(&message));
                    }
                    Event::ToastAction {
                        message,
                        label,
                        action,
                    } => {
                        let toast = adw::Toast::new(&message);
                        toast.set_button_label(Some(&label));
                        if action == "chat-destroy-session" {
                            let chat = chat.clone();
                            toast.connect_button_clicked(move |_| chat.destroy_stale_session());
                        }
                        toast_overlay.add_toast(toast);
                    }
                    Event::AgentSessionUpdate { .. } => {}
                }
            }
        });
    }

    // Initial state check runs only now that the UI is subscribed, so the
    // safe-mode banner reflects reality from the first frame.
    if let Err(e) = supervisor.recheck() {
        tracing::warn!("devcontainer recheck failed: {e:#}");
    }
    if let Err(e) = supervisor.start_watching() {
        tracing::warn!("devcontainer watcher failed: {e:#}");
    }

    // --- restore what was last open (XDG state + ACP session/load) -------
    // Never from a probe instance: session/load ATTACHES to the user's
    // real conversation, and two clients on one session is a fork bomb
    // for its history.
    let persisted = if probe_mode {
        taste_core::state::WorkspaceState::default()
    } else {
        taste_core::state::load(&root)
    };
    for path in &persisted.open_files {
        if path.is_file() {
            editor.open_at(path, None);
        }
    }
    if let Some(active) = &persisted.active_file {
        if active.is_file() {
            editor.open_at(active, None);
        }
    }
    editor.sync_git_state();
    if probe_mode {
        // No agent, no persistence: render, get probed, quit.
    } else if let (Some(agent_id), Some(session_id)) = (&persisted.agent_id, &persisted.session_id)
    {
        chat.restore_session(agent_id, session_id);
    } else {
        // First open: greet with the sign-in flow, not an empty box.
        chat.connect_default();
    }

    // Persist on close: open files come from the shared IDE state, the chat
    // session id from the pane. The conversation itself lives with the
    // agent (session/load); we keep only the handle.
    if !probe_mode {
        let workspace = workspace.clone();
        let chat = chat.clone();
        let root = root.clone();
        window.connect_close_request(move |_| {
            let open = workspace.ide.open_files();
            // Update in place: fields owned elsewhere (the persisted model
            // choice) survive untouched.
            let mut state = taste_core::state::load(&root);
            state.root = root.clone();
            state.open_files = open.iter().map(|f| f.path.clone()).collect();
            state.active_file = open.iter().find(|f| f.active).map(|f| f.path.clone());
            // Only a session with content is restorable; without one the
            // previously stored (still loadable) handle stays.
            if let Some((agent, session)) = chat.restorable_session() {
                state.agent_id = Some(agent);
                state.session_id = Some(session);
            }
            if let Err(e) = taste_core::state::save(&root, &state) {
                tracing::warn!("saving workspace state failed: {e:#}");
            }
            glib::Propagation::Proceed
        });
    }

    window
}

/// Watch the sandbox URL drop directory; confirm and open each URL in the
/// user's browser. Untrusted input by definition (an agent wrote it), hence
/// the scheme check and the explicit confirmation.
/// The shortcuts reference, as a plain boxed list (AdwShortcutsDialog
/// needs a newer libadwaita than we target).
fn present_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    for (accel, title) in [
        ("Ctrl+P", "Open a file by name"),
        ("Ctrl+F", "Find in project"),
        ("Ctrl+S", "Save the current file"),
        ("Ctrl+W", "Close the current tab"),
        ("Ctrl+Q", "Quit (state is saved)"),
        ("Ctrl+Shift+C / V", "Copy / paste in terminals"),
        ("Ctrl+Click", "Open a link from a terminal"),
        ("Tab / Esc", "Accept / dismiss an AI suggestion"),
        ("Enter / Shift+Enter", "Send prompt / new line"),
    ] {
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(
            &gtk::Label::builder()
                .label(accel)
                .css_classes(["dim-label", "numeric"])
                .build(),
        );
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(420)
        .build();
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

/// Quick-open: type-to-filter over the indexed file list, Enter opens the
/// top hit. Lean by design — the index already exists for search.
fn present_quick_open(
    parent: &adw::ApplicationWindow,
    root: &std::path::Path,
    index: Option<std::sync::Arc<Vec<std::path::PathBuf>>>,
    editor: std::rc::Rc<crate::editor::Editor>,
) {
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Type a file name…")
        .margin_top(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let results = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Browse)
        .css_classes(["navigation-sidebar"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&results)
        .vexpand(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.append(&entry);
    content.append(&scroller);
    let dialog = adw::Dialog::builder()
        .title("Open File")
        .content_width(520)
        .content_height(420)
        .build();
    dialog.set_child(Some(&content));

    let root = root.to_path_buf();
    let refresh = {
        let results = results.clone();
        let dialog = dialog.downgrade();
        let editor = editor.clone();
        let root = root.clone();
        move |query: &str| {
            while let Some(child) = results.first_child() {
                results.remove(&child);
            }
            let Some(index) = index.as_ref() else {
                results.append(
                    &gtk::Label::builder()
                        .label("Index still building — try again in a moment")
                        .css_classes(["dim-label"])
                        .margin_top(12)
                        .build(),
                );
                return;
            };
            let query = query.to_lowercase();
            // File-name hits first, then path hits; both bounded.
            let mut hits: Vec<&std::path::PathBuf> = Vec::new();
            for by_name in [true, false] {
                for path in index.iter() {
                    if hits.len() >= 50 {
                        break;
                    }
                    let rel = path.strip_prefix(&root).unwrap_or(path);
                    let hay = if by_name {
                        rel.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase()
                    } else {
                        rel.display().to_string().to_lowercase()
                    };
                    let already = hits.contains(&path);
                    if !already && hay.contains(&query) {
                        hits.push(path);
                    }
                }
            }
            for path in hits {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                // Row titles are Pango markup: file names must be escaped.
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&name))
                    .subtitle(glib::markup_escape_text(&rel.display().to_string()))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_gicon(&crate::editor::file_type_icon(
                    path,
                )));
                let editor = editor.clone();
                let dialog = dialog.clone();
                let path = path.clone();
                row.connect_activated(move |_| {
                    editor.open_at(&path, None);
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                });
                results.append(&row);
            }
            if let Some(first) = results.row_at_index(0) {
                results.select_row(Some(&first));
            }
        }
    };
    {
        let refresh = refresh.clone();
        entry.connect_search_changed(move |entry| refresh(&entry.text()));
    }
    {
        let results = results.clone();
        entry.connect_activate(move |_| {
            if let Some(row) = results.selected_row().or_else(|| results.row_at_index(0)) {
                row.emit_activate();
            }
        });
    }
    refresh("");
    dialog.present(Some(parent));
    entry.grab_focus();
}

/// Open a URL the user asked for: through the bootstrap's host-side
/// opener when present (the container has no browser), else the portal
/// (packaged runs), else the clipboard.
fn open_url(url: &str, overlay: &adw::ToastOverlay) {
    if let Some((dir, token)) = crate::host_open_channel() {
        let path = dir.join(format!("{token}.{}", glib::monotonic_time()));
        let contents = url.to_string();
        let overlay = overlay.clone();
        let handle =
            crate::runtime::runtime().spawn_blocking(move || std::fs::write(&path, contents));
        glib::spawn_future_local(async move {
            let message = match handle.await {
                Ok(Ok(())) => "Opening in your browser…",
                _ => "Couldn't reach the host URL opener",
            };
            overlay.add_toast(adw::Toast::new(message));
        });
        return;
    }
    let overlay = overlay.clone();
    let fallback = url.to_string();
    gtk::UriLauncher::new(url).launch(
        None::<&gtk::Window>,
        gtk::gio::Cancellable::NONE,
        move |result| {
            if result.is_err() {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&fallback);
                }
                overlay.add_toast(adw::Toast::new(
                    "No browser here — link copied, paste it on the host",
                ));
            }
        },
    );
}

fn start_url_bridge(window: &adw::ApplicationWindow) {
    use notify::Watcher;

    let dir = taste_acp::sandbox::url_bridge_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("url bridge dir: {e}");
        return;
    }
    // Stale drops from previous runs must not pop dialogs at startup.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    let (tx, rx) = async_channel::unbounded::<std::path::PathBuf>();
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            // Create fires at open(O_CREAT), often BEFORE the helper's
            // printf writes the URL — Modify covers the completed write.
            if matches!(
                event.kind,
                notify::EventKind::Create(_) | notify::EventKind::Modify(_)
            ) {
                for path in event.paths {
                    let _ = tx.try_send(path);
                }
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("url bridge watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("url bridge watch: {e}");
        return;
    }
    Box::leak(Box::new(watcher));

    let window = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(path) = rx.recv().await {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue; // already consumed by an earlier event
            };
            let url = raw.trim().to_string();
            if url.is_empty() {
                // Created but not yet written: leave the file; the Modify
                // event after the write will bring us back.
                continue;
            }
            let _ = std::fs::remove_file(&path);
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                tracing::warn!("url bridge: refusing non-http(s) url");
                continue;
            }
            let Some(window) = window.upgrade() else {
                break;
            };
            let dialog = adw::AlertDialog::new(
                Some("Open sign-in link?"),
                Some(&format!(
                    "An agent wants to open this in your browser:\n\n{url}"
                )),
            );
            // "Copy Link" matters in the self-hosting bootstrap: inside the
            // container there is no browser, so the user pastes it into the
            // host browser (OAuth callbacks work under --network=host).
            dialog.add_responses(&[
                ("deny", "Deny"),
                ("copy", "Copy Link"),
                ("open", "Open in Browser"),
            ]);
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("open"));
            dialog.set_close_response("deny");
            let launch_url = url.clone();
            let launch_window = window.clone();
            dialog.connect_response(Some("open"), move |_, _| {
                // Same channel as user clicks: host opener → portal →
                // clipboard.
                if let Some(overlay) = launch_window.content().and_downcast::<adw::ToastOverlay>() {
                    open_url(&launch_url, &overlay);
                }
            });
            let copy_url = url.clone();
            let copy_window = window.clone();
            dialog.connect_response(Some("copy"), move |_, _| {
                copy_window.clipboard().set_text(&copy_url);
            });
            dialog.present(Some(&window));
        }
    });
}
