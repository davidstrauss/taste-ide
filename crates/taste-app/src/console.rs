//! Bottom pane: tabbed console.
//!
//! Terminal tabs spawn in the current execution context (host, or inside the
//! devcontainer once it is running) — resolved at spawn time through
//! `ExecContext`, which is exactly what makes container reloads invisible to
//! existing tabs and automatic for new ones. The devcontainer supervisor's
//! build/startup log is a permanent read-only first tab.

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
use std::rc::Rc;
use std::sync::Arc;
use taste_core::Workspace;
use taste_devcontainer::{ResourceInfo, ResourceKind, Supervisor};
use vte4::prelude::*;

pub struct Console {
    pub widget: gtk::Box,
    tabs: adw::TabView,
    supervisor_log: gtk::TextView,
    devcontainer_page: adw::TabPage,
    services_page: adw::TabPage,
    follow_log: gtk::Switch,
    /// Shell tabs running on the machine/IDE-container — retired when the
    /// devcontainer attaches (work belongs inside it).
    host_shells: std::cell::RefCell<Vec<adw::TabPage>>,
    /// The environment view inside the Devcontainer tab: the podman
    /// resources (container/image/volumes) backing this workspace.
    resources_list: gtk::ListBox,
    /// Everything the Containers badge is computed from. Three callers own
    /// different parts of it (resource polls, attach/detach, the config
    /// watcher), so each records its piece and one function decides what
    /// the tab ends up saying — otherwise whichever ran last would win.
    containers: std::cell::Cell<usize>,
    containers_down: std::cell::Cell<usize>,
    container_running: std::cell::Cell<bool>,
    /// The devcontainer config on disk no longer matches what is running.
    pending_rebuild: std::cell::Cell<bool>,
    /// Created lazily on the first Flatpak log line, so projects without a
    /// manifest never see the tab.
    flatpak_log: std::cell::RefCell<Option<gtk::TextView>>,
    /// The pinned Services tab: systemd units + journal in the container.
    services: Rc<crate::services::ServicesPane>,
    workspace: Workspace,
    supervisor: Arc<Supervisor>,
}

impl Console {
    pub fn new(workspace: Workspace, supervisor: Arc<Supervisor>) -> Rc<Self> {
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
            .tooltip_text("New terminal (in the current container context)")
            .css_classes(["flat"])
            .build();
        // New-terminal lives at the END of the strip: pinned tabs and
        // their icons keep the left edge.
        tab_bar.set_end_action_widget(Some(&new_tab_button));

        // Permanent Devcontainer tab: environment view on top, log below.
        let refresh_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh resources")
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
        let stop_button = gtk::Button::with_label("Stop");
        stop_button.set_tooltip_text(Some("Stop and remove the container"));
        let rebuild_button = gtk::Button::with_label("Rebuild");
        rebuild_button.set_tooltip_text(Some("Rebuild and restart from the current config"));
        let nuke_button = gtk::Button::builder()
            .label("Nuke")
            .tooltip_text("Remove the container AND its image; next start rebuilds from scratch")
            .css_classes(["destructive-action"])
            .build();
        let action_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        action_bar.set_margin_top(8);
        action_bar.set_margin_bottom(6);
        action_bar.set_margin_start(12);
        action_bar.set_margin_end(12);
        let env_label = gtk::Label::builder()
            .label("Environment")
            .css_classes(["heading"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        action_bar.append(&env_label);
        action_bar.append(&tail_label);
        action_bar.append(&follow_log);
        action_bar.append(&refresh_button);
        action_bar.append(&stop_button);
        action_bar.append(&rebuild_button);
        action_bar.append(&nuke_button);

        let resources_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();

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

        let devcontainer_page_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        devcontainer_page_box.append(&action_bar);
        devcontainer_page_box.append(&resources_list);
        devcontainer_page_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        devcontainer_page_box.append(&log_scroller);
        let log_page = tabs.append(&devcontainer_page_box);
        log_page.set_title("Containers");
        // Pinned tabs render icon-only: without an icon they draw as the
        // missing-image placeholder.
        log_page.set_icon(Some(&gtk::gio::ThemedIcon::new("taste-container-off")));

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
            devcontainer_page: log_page.clone(),
            services_page: services_page.clone(),
            follow_log: follow_log.clone(),
            host_shells: std::cell::RefCell::new(Vec::new()),
            resources_list,
            containers: std::cell::Cell::new(0),
            containers_down: std::cell::Cell::new(0),
            container_running: std::cell::Cell::new(false),
            pending_rebuild: std::cell::Cell::new(false),
            flatpak_log: std::cell::RefCell::new(None),
            services,
            workspace,
            supervisor,
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
                console.refresh_resources();
            }
        });
        let weak = Rc::downgrade(&console);
        stop_button.connect_clicked(move |_| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            let supervisor = console.supervisor.clone();
            crate::runtime::runtime().spawn(async move {
                let _ = supervisor.stop().await;
            });
        });
        let weak = Rc::downgrade(&console);
        rebuild_button.connect_clicked(move |_| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            let supervisor = console.supervisor.clone();
            let events = console.workspace.events.clone();
            crate::runtime::runtime().spawn(async move {
                if let Err(e) = supervisor.reload().await {
                    events.publish(taste_core::Event::Toast(format!("Rebuild failed: {e}")));
                }
            });
        });
        let weak = Rc::downgrade(&console);
        nuke_button.connect_clicked(move |_| {
            let Some(console) = weak.upgrade() else {
                return;
            };
            let supervisor = console.supervisor.clone();
            console.clone().confirm_destructive(
                "Nuke devcontainer?",
                "Removes the container and its image. The next start rebuilds \
                 from scratch. Named volumes (caches) are kept.",
                "Remove",
                move || {
                    let supervisor = supervisor.clone();
                    crate::runtime::runtime().spawn(async move {
                        let _ = supervisor.nuke().await;
                    });
                },
            );
        });

        // The Devcontainer and Services tabs are permanent fixtures.
        {
            let weak = Rc::downgrade(&console);
            console.tabs.connect_close_page(move |tabs, page| {
                let Some(console) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if *page == console.devcontainer_page || *page == console.services_page {
                    tabs.close_page_finish(page, false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }

        console.add_terminal_tab();
        console.refresh_resources();
        console
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

    /// Re-query podman for this environment's resources and re-render.
    /// Also the single hook for devcontainer state changes, so the
    /// Services tab rides along.
    pub fn refresh_resources(self: &Rc<Self>) {
        self.services.refresh();
        let supervisor = self.supervisor.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle =
                crate::runtime::runtime().spawn(async move { supervisor.list_resources().await });
            let Ok(resources) = handle.await else { return };
            let Some(console) = weak.upgrade() else {
                return;
            };
            let containers = resources
                .iter()
                .filter(|r| r.kind == ResourceKind::Container)
                .count();
            let down = resources
                .iter()
                .filter(|r| {
                    r.kind == ResourceKind::Container
                        && r.status.to_lowercase().starts_with("exited")
                })
                .count();
            console.containers.set(containers);
            console.containers_down.set(down);
            console.refresh_container_badge();
            console.render_resources(&resources);
        });
    }

    fn render_resources(self: &Rc<Self>, resources: &[ResourceInfo]) {
        while let Some(child) = self.resources_list.first_child() {
            self.resources_list.remove(&child);
        }
        if resources.is_empty() {
            let empty = gtk::Label::builder()
                .label("No containers or images yet — start the devcontainer to create them.")
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
                    let supervisor = console.supervisor.clone();
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

    /// A status badge in a tab's indicator slot: `Some((icon, tooltip))` to
    /// show one, `None` to clear it.
    ///
    /// AdwTabPage offers a title, an icon, this indicator and an attention
    /// dot — there is no badge property, and AdwTabBar builds its own tab
    /// widgets, so a literal text pill would mean hand-rolling the tab strip
    /// and giving up reordering, pinning and overflow. The indicator is the
    /// slot the platform has for exactly this, and it carries its own
    /// tooltip.
    fn set_pill(page: &adw::TabPage, pill: Option<(&str, String)>) {
        match pill {
            Some((icon, tooltip)) => {
                page.set_indicator_icon(Some(&gtk::gio::ThemedIcon::new(icon)));
                page.set_indicator_tooltip(&tooltip);
            }
            None => {
                page.set_indicator_icon(gtk::gio::Icon::NONE);
                page.set_indicator_tooltip("");
            }
        }
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
        let keep = Rc::new(std::cell::Cell::new(false));
        {
            let keep = keep.clone();
            toast.connect_button_clicked(move |toast| {
                keep.set(true);
                toast.dismiss();
            });
        }
        overlay.add_toast(toast.clone());
        let what = what.to_string();
        let remaining = std::cell::Cell::new(5i32);
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

    /// Badge the Devcontainer tab icon: green dot = connected, red =
    /// safe mode.
    pub fn set_container_state(self: &Rc<Self>, running: bool) {
        if running {
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
        self.container_running.set(running);
        self.refresh_container_badge();
    }

    /// The devcontainer config on disk changed under the running container.
    /// The rebuild banner says so loudly; this is the quiet version, for
    /// once the banner has been read and the console tab is all you see.
    pub fn set_pending_rebuild(&self, pending: bool) {
        if self.pending_rebuild.replace(pending) == pending {
            return;
        }
        self.refresh_container_badge();
    }

    /// Title and icon for the Containers tab.
    ///
    /// Yellow means "running, but stale" — the same reading Services gives
    /// it. Red stays reserved for containers that actually fell over, which
    /// is what the attention dot marks.
    fn refresh_container_badge(&self) {
        let containers = self.containers.get();
        let pending = self.pending_rebuild.get();
        self.devcontainer_page
            .set_title(&format!("Containers · {containers}"));
        self.devcontainer_page
            .set_needs_attention(self.containers_down.get() > 0);
        self.devcontainer_page
            .set_icon(Some(&gtk::gio::ThemedIcon::new(if pending {
                "taste-container-warn"
            } else if self.container_running.get() {
                "taste-container-on"
            } else {
                "taste-container-off"
            })));
        // The badge rides in the indicator slot rather than the title:
        // "Containers · 2 · 2 need rebuild" said it, but a tab title is not
        // where a sentence belongs. Config changes are workspace-wide, so a
        // pending rebuild makes every running container the stale one — the
        // count that used to be in the title is in the tooltip.
        Self::set_pill(
            &self.devcontainer_page,
            pending.then(|| {
                (
                    "software-update-available-symbolic",
                    if containers > 0 {
                        format!(
                            "Needs rebuild — the configuration changed under \
                             {containers} running container(s)"
                        )
                    } else {
                        "Needs rebuild — the devcontainer configuration changed".to_string()
                    },
                )
            }),
        );
    }

    /// Live badge for the Services tab: count, failures called out.
    pub fn update_service_summary(&self, total: usize, failed: usize) {
        Self::set_pill(&self.services_page, None);
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
        self.services_page.set_title("Services");
        self.services_page
            .set_icon(Some(&gtk::gio::ThemedIcon::new(if systemd_missing {
                "taste-services-warn"
            } else {
                "taste-services-none"
            })));
        self.services_page.set_needs_attention(false);
        Self::set_pill(
            &self.services_page,
            systemd_missing.then(|| {
                (
                    "action-unavailable-symbolic",
                    "No systemd in this container — services cannot be listed".to_string(),
                )
            }),
        );
    }

    /// Bring the Devcontainer log tab to the front (the banner's
    /// "View Log" lands here).
    pub fn show_devcontainer_log(&self) {
        self.tabs.set_selected_page(&self.devcontainer_page);
    }

    /// Append a devcontainer build/startup log line — and tail it, so a
    /// running build reads like a running build.
    pub fn append_supervisor_log(&self, line: &str) {
        let buffer = self.supervisor_log.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, line);
        buffer.insert(&mut end, "\n");
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
        let (terminal, page) = self.spawn_tab(title, "system-run-symbolic", spec, env);
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

    /// Open a shell tab in the *current* execution context.
    pub fn add_terminal_tab(self: &Rc<Self>) {
        let spec = self.workspace.exec.resolve("/bin/bash", &[], true);
        // Name the shell by where it REALLY runs — "host" was ambiguous
        // when the IDE itself lives in a container.
        let in_devcontainer = self.workspace.exec.container_id().is_some();
        // Non-devcontainer shells carry a red warning badge: they run on
        // the host (or the IDE's own barely-confined container), outside
        // the environment work is supposed to happen in.
        let (title, icon) = if in_devcontainer {
            ("devcontainer", "package-x-generic-symbolic")
        } else if self.workspace.exec.is_inside_container() {
            // Self-hosting bootstrap: the IDE's own container IS the
            // project's devcontainer (container mode by construction), so
            // its shells are confined — no warning. Warn only when the
            // surrounding container is not the devcontainer (safe mode).
            if self.workspace.exec.is_container() {
                ("IDE container", "package-x-generic-symbolic")
            } else {
                ("IDE container", "taste-container-warn")
            }
        } else {
            ("this machine", "taste-host-warn")
        };
        let (terminal, page) = self.spawn_tab(title, icon, spec, &[]);
        // Retitle as user@host, asked of the shell's own execution context
        // (the placeholder above stands until the probe answers).
        {
            let probe = self.workspace.exec.resolve(
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
                let complete = title.split_once('@').is_some_and(|(u, h)| !u.is_empty() && !h.is_empty());
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
            terminal.connect_child_exited(move |_, _| {
                if let Some(console) = weak.upgrade() {
                    console.countdown_close(page.clone(), "Shell exited");
                }
            });
        }
        if !in_devcontainer && !self.workspace.exec.is_inside_container() {
            self.host_shells.borrow_mut().push(page);
        }
    }

    fn spawn_tab(
        &self,
        title: &str,
        icon: &str,
        spec: taste_core::CommandSpec,
        extra_env: &[(String, String)],
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
        let hovered_url: Rc<std::cell::RefCell<Option<String>>> =
            Rc::new(std::cell::RefCell::new(None));
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
            Some(&self.workspace.root().display().to_string()),
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
