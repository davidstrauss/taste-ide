//! The persistent devcontainer banner.
//!
//! Revealed while the primary is in safe mode or on its way somewhere:
//! building, starting, failed, stopped, no config — and the baseline
//! running, which is what safe mode looks like now that safe mode is a
//! container too. It does NOT speak for configuration drift under the
//! project's own config (David, 2026-09-06: "I don't want the banner
//! telling me to rebuild the container. The yellow indicator in the env
//! listing is sufficient"): a drifted project container is a running one,
//! and the backlog row's amber light with its "needs rebuild" text says
//! so, beside the toolbar's Rebuild. Leaving safe mode is a different
//! event: while the baseline runs, the banner says what the project's
//! config comes to — ready, so Rebuild leaves safe mode; passed over, and
//! why; or absent, so Create — because a devcontainer.json that became
//! usable with nothing on screen asking to use it read as the IDE ignoring
//! it (David, 2026-09-16: "Relaunched. Still in safe mode. No request to
//! reload."). The same state is served to agents over MCP; the buttons
//! here and the `devcontainer_reload` tool call converge on
//! `Supervisor::reload`.
//! Build/start stages add a pulsing progress strip and a "View Log" button
//! that jumps to the (tailing) console log.
//!
//! Hand-rolled rather than AdwBanner: the action belongs right next to the
//! status text (AdwBanner pins its button at the far end and can't restyle
//! it). Banner look comes from `.taste-banner` (see main.rs CSS); the
//! button is a suggested action — the one blue thing on the strip.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use taste_core::event::DevcontainerStateEvent;
use taste_core::EventBus;
use taste_devcontainer::Supervisor;

#[derive(Clone, Copy, PartialEq)]
enum ButtonAction {
    Reload,
    ViewLog,
    CreateConfig,
}

pub struct DevcontainerBanner {
    pub widget: gtk::Box,
    revealer: gtk::Revealer,
    title: gtk::Label,
    button: gtk::Button,
    progress: gtk::ProgressBar,
    supervisor: Arc<Supervisor>,
    events: EventBus,
    action: Cell<ButtonAction>,
    /// `TASTE_PROBE_BANNER` posed this banner: real state stops moving it.
    posed: Cell<bool>,
}

impl DevcontainerBanner {
    pub fn new(supervisor: Arc<Supervisor>, events: EventBus) -> Rc<Self> {
        let title = gtk::Label::builder()
            .css_classes(["heading"])
            .xalign(0.0)
            .wrap(true)
            .build();
        let button = gtk::Button::builder()
            .label("Rebuild")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["taste-banner"])
            .build();
        row.append(&title);
        row.append(&button);
        let revealer = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        let progress = gtk::ProgressBar::builder()
            .visible(false)
            .css_classes(["osd"])
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&revealer);
        widget.append(&progress);
        let this = Rc::new(Self {
            widget,
            revealer,
            title,
            button: button.clone(),
            progress,
            supervisor,
            events,
            action: Cell::new(ButtonAction::Reload),
            posed: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            match this.action.get() {
                ButtonAction::ViewLog => {
                    this.events.publish(taste_core::Event::ShowDevcontainerLog);
                }
                ButtonAction::CreateConfig => {
                    this.events
                        .publish(taste_core::Event::CreateDevcontainerConfig);
                }
                ButtonAction::Reload => {
                    let supervisor = this.supervisor.clone();
                    // Stay revealed: reload's own state events retitle the
                    // banner; hiding here would orphan an early failure.
                    this.set_title("Devcontainer starting…");
                    this.set_button(None);
                    crate::runtime::runtime().spawn(async move {
                        if let Err(e) = supervisor.reload().await {
                            tracing::warn!("devcontainer reload failed: {e:#}");
                        }
                    });
                }
            }
        });

        this
    }

    fn set_title(&self, text: &str) {
        self.title.set_label(text);
    }

    fn set_button(&self, label: Option<&str>) {
        match label {
            Some(label) => {
                self.button.set_label(label);
                self.button.set_visible(true);
            }
            None => self.button.set_visible(false),
        }
    }

    fn set_revealed(&self, revealed: bool) {
        self.revealer.set_reveal_child(revealed);
    }

    fn set_working(self: &Rc<Self>, working: bool) {
        if working == self.progress.is_visible() {
            return;
        }
        self.progress.set_visible(working);
        if working {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
                match weak.upgrade() {
                    Some(this) if this.progress.is_visible() => {
                        this.progress.pulse();
                        glib::ControlFlow::Continue
                    }
                    _ => glib::ControlFlow::Break,
                }
            });
        }
    }

    /// Drift under the project's own config is not the banner's to
    /// announce: the fleet row carries it. Under the baseline it is the
    /// news this banner exists for — the config on disk just resolved to
    /// something other than what runs — so the running-baseline face is
    /// redrawn.
    pub fn on_pending_changes(self: &Rc<Self>, _pending: bool) {
        if self.posed.get() {
            return;
        }
        if matches!(
            self.supervisor.state(),
            taste_devcontainer::SupervisorState::Running { .. }
        ) {
            self.sync_running();
        } else if !self.state_wants_banner() {
            self.set_revealed(false);
        }
    }

    /// The banner while a container runs. Under the project's own config,
    /// nothing: running is running. Under the baseline, safe mode's face,
    /// worded by what the project's config resolves to right now — which
    /// is a read of the checkout, so it runs on the blocking pool and the
    /// face lands a moment later.
    fn sync_running(self: &Rc<Self>) {
        if self.supervisor.config_authority() != taste_core::ConfigAuthority::Baseline {
            self.set_revealed(false);
            return;
        }
        let supervisor = self.supervisor.clone();
        let resolve =
            crate::runtime::runtime().spawn_blocking(move || supervisor.resolve_authority());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((resolved, reason)) = resolve.await else {
                return;
            };
            let Some(this) = weak.upgrade() else { return };
            if this.posed.get()
                || !matches!(
                    this.supervisor.state(),
                    taste_devcontainer::SupervisorState::Running { .. }
                )
            {
                return;
            }
            this.show_baseline_face(resolved, reason.as_deref());
        });
    }

    /// Safe mode's three sentences, by what the project's config comes to.
    fn show_baseline_face(&self, resolved: taste_core::ConfigAuthority, reason: Option<&str>) {
        match (resolved, reason) {
            (taste_core::ConfigAuthority::Project, _) => {
                self.set_title(
                    "Safe mode — devcontainer.json is ready; rebuild to run the project's own \
                     environment",
                );
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Rebuild"));
            }
            (taste_core::ConfigAuthority::Baseline, Some(reason)) => {
                let first = reason.lines().next().unwrap_or(reason);
                self.set_title(&format!(
                    "Safe mode — devcontainer.json passed over: {first} (full log in the \
                     Containers tab)"
                ));
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
            }
            (taste_core::ConfigAuthority::Baseline, None) => {
                self.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.set_button(Some("Create"));
            }
        }
        self.set_revealed(true);
    }

    /// `TASTE_PROBE_BANNER=ready|passed|none`: pose the running-baseline
    /// face without a checkout in that state, and hold it against the
    /// state events that follow.
    pub fn pose_for_probe(self: &Rc<Self>, kind: &str) {
        self.posed.set(true);
        match kind {
            "ready" => self.show_baseline_face(taste_core::ConfigAuthority::Project, None),
            "passed" => self.show_baseline_face(
                taste_core::ConfigAuthority::Baseline,
                Some(
                    "the project config was refused: devcontainer.json mount \
                     \"source=${localWorkspaceFolder}/vendor,…\": bind sources must stay \
                     inside the workspace",
                ),
            ),
            _ => self.show_baseline_face(taste_core::ConfigAuthority::Baseline, None),
        }
    }

    pub fn on_state(self: &Rc<Self>, state: &DevcontainerStateEvent) {
        if self.posed.get() {
            return;
        }
        self.set_working(matches!(
            state,
            DevcontainerStateEvent::Building | DevcontainerStateEvent::Starting
        ));
        match state {
            DevcontainerStateEvent::ConfigDetected => {
                self.set_title("Safe mode — devcontainer not running; only its setup is editable");
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Start"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Building => {
                self.set_title("Devcontainer building…");
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Starting => {
                self.set_title("Devcontainer starting…");
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Running { .. } => {
                // Under the project's config, running is running, drifted
                // or not — the row says which. Under the baseline, this is
                // safe mode, and the banner says what the config comes to.
                self.sync_running();
            }
            DevcontainerStateEvent::Failed { message } => {
                self.set_title(&format!(
                    "Safe mode — devcontainer failed: {message} \
                     (full log in the Containers tab)"
                ));
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Retry"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::NoConfig => {
                // State + one action: Create opens the blank config, the
                // same flow as the tree's ghost row.
                self.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.set_button(Some("Create"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Stopped => {
                self.set_title("Safe mode — devcontainer stopped");
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Start"));
                self.set_revealed(true);
            }
        }
    }

    fn state_wants_banner(&self) -> bool {
        // The banner doubles as the safe-mode indicator: visible in every
        // state except Running-without-drift.
        use taste_devcontainer::SupervisorState as S;
        !matches!(self.supervisor.state(), S::Running { .. })
    }
}
