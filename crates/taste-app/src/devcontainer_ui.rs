//! The persistent devcontainer banner.
//!
//! Revealed whenever the on-disk config has drifted from the running
//! container (or a config exists but no container runs). Stays until acted
//! on. The same state is served to agents over MCP; the button here and the
//! `devcontainer_reload` tool call converge on `Supervisor::reload`.
//! Build/start stages add a pulsing progress strip and a "View Log" button
//! that jumps to the (tailing) console log.

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
    banner: adw::Banner,
    progress: gtk::ProgressBar,
    supervisor: Arc<Supervisor>,
    events: EventBus,
    action: Cell<ButtonAction>,
}

impl DevcontainerBanner {
    pub fn new(supervisor: Arc<Supervisor>, events: EventBus) -> Rc<Self> {
        let banner = adw::Banner::builder().button_label("Rebuild").build();
        let progress = gtk::ProgressBar::builder()
            .visible(false)
            .css_classes(["osd"])
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&banner);
        widget.append(&progress);
        let banner_widget = banner.clone();
        let this = Rc::new(Self {
            widget,
            banner: banner_widget,
            progress,
            supervisor,
            events,
            action: Cell::new(ButtonAction::Reload),
        });

        let weak = Rc::downgrade(&this);
        banner.connect_button_clicked(move |_| {
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
                    this.banner.set_title("Devcontainer: starting…");
                    this.banner.set_button_label(None);
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

    pub fn on_pending_changes(self: &Rc<Self>, pending: bool) {
        if pending {
            self.banner.set_title("Devcontainer configuration changed");
            self.action.set(ButtonAction::Reload);
            self.banner.set_button_label(Some("Rebuild"));
            self.banner.set_revealed(true);
        } else if !self.state_wants_banner() {
            self.banner.set_revealed(false);
        }
    }

    pub fn on_state(self: &Rc<Self>, state: &DevcontainerStateEvent) {
        self.set_working(matches!(
            state,
            DevcontainerStateEvent::Building | DevcontainerStateEvent::Starting
        ));
        match state {
            DevcontainerStateEvent::ConfigDetected => {
                self.banner
                    .set_title("Safe mode — devcontainer not running; only its setup is editable");
                self.action.set(ButtonAction::Reload);
                self.banner.set_button_label(Some("Start"));
                self.banner.set_revealed(true);
            }
            DevcontainerStateEvent::Building => {
                self.banner.set_title("Devcontainer: building…");
                self.action.set(ButtonAction::ViewLog);
                self.banner.set_button_label(Some("View Log"));
                self.banner.set_revealed(true);
            }
            DevcontainerStateEvent::Starting => {
                self.banner.set_title("Devcontainer: starting…");
                self.action.set(ButtonAction::ViewLog);
                self.banner.set_button_label(Some("View Log"));
                self.banner.set_revealed(true);
            }
            DevcontainerStateEvent::Running { .. } => {
                if !self.supervisor.pending_changes() {
                    self.banner.set_revealed(false);
                }
            }
            DevcontainerStateEvent::Failed { message } => {
                self.banner
                    .set_title(&format!("Safe mode — devcontainer failed: {message}"));
                self.action.set(ButtonAction::Reload);
                self.banner.set_button_label(Some("Retry"));
                self.banner.set_revealed(true);
            }
            DevcontainerStateEvent::NoConfig => {
                // State + one action: Create opens the blank config, the
                // same flow as the tree's ghost row.
                self.banner.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.banner.set_button_label(Some("Create"));
                self.banner.set_revealed(true);
            }
            DevcontainerStateEvent::Stopped => {
                self.banner.set_title("Safe mode — devcontainer stopped");
                self.action.set(ButtonAction::Reload);
                self.banner.set_button_label(Some("Start"));
                self.banner.set_revealed(true);
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
