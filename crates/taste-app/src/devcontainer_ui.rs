//! The persistent devcontainer banner.
//!
//! Revealed whenever the on-disk config has drifted from the running
//! container (or a config exists but no container runs). Stays until acted
//! on. The same state is served to agents over MCP; the button here and the
//! `devcontainer_reload` tool call converge on `Supervisor::reload`.

use std::rc::Rc;
use std::sync::Arc;

use taste_core::event::DevcontainerStateEvent;
use taste_devcontainer::Supervisor;

pub struct DevcontainerBanner {
    pub widget: adw::Banner,
    supervisor: Arc<Supervisor>,
}

impl DevcontainerBanner {
    pub fn new(supervisor: Arc<Supervisor>) -> Rc<Self> {
        let widget = adw::Banner::builder().button_label("Rebuild").build();
        let banner = Rc::new(Self { widget, supervisor });

        let weak = Rc::downgrade(&banner);
        banner.widget.connect_button_clicked(move |_| {
            let Some(banner) = weak.upgrade() else { return };
            let supervisor = banner.supervisor.clone();
            // Stay revealed: reload's own state events (Building/…/Failed)
            // retitle the banner. Hiding here would orphan the user if the
            // reload errors before any state is published.
            banner.widget.set_title("Devcontainer: starting…");
            banner.widget.set_button_label(None);
            crate::runtime::runtime().spawn(async move {
                if let Err(e) = supervisor.reload().await {
                    tracing::warn!("devcontainer reload failed: {e:#}");
                }
            });
        });

        banner
    }

    pub fn on_pending_changes(&self, pending: bool) {
        if pending {
            self.widget.set_title("Devcontainer configuration changed");
            self.widget.set_button_label(Some("Rebuild"));
            self.widget.set_revealed(true);
        } else if !self.state_wants_banner() {
            self.widget.set_revealed(false);
        }
    }

    pub fn on_state(&self, state: &DevcontainerStateEvent) {
        match state {
            DevcontainerStateEvent::ConfigDetected => {
                self.widget
                    .set_title("Safe mode — devcontainer not running; only its setup is editable");
                self.widget.set_button_label(Some("Start"));
                self.widget.set_revealed(true);
            }
            DevcontainerStateEvent::Building => {
                self.widget.set_title("Devcontainer: building…");
                self.widget.set_button_label(None);
                self.widget.set_revealed(true);
            }
            DevcontainerStateEvent::Starting => {
                self.widget.set_title("Devcontainer: starting…");
                self.widget.set_button_label(None);
                self.widget.set_revealed(true);
            }
            DevcontainerStateEvent::Running { .. } => {
                if !self.supervisor.pending_changes() {
                    self.widget.set_revealed(false);
                }
            }
            DevcontainerStateEvent::Failed { message } => {
                self.widget
                    .set_title(&format!("Safe mode — devcontainer failed: {message}"));
                self.widget.set_button_label(Some("Retry"));
                self.widget.set_revealed(true);
            }
            DevcontainerStateEvent::NoConfig => {
                self.widget
                    .set_title("Safe mode — no devcontainer; create .devcontainer/ to begin");
                self.widget.set_button_label(None);
                self.widget.set_revealed(true);
            }
            DevcontainerStateEvent::Stopped => {
                self.widget.set_title("Safe mode — devcontainer stopped");
                self.widget.set_button_label(Some("Start"));
                self.widget.set_revealed(true);
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
