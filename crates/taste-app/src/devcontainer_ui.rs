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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use taste_core::event::{AskKind, DevcontainerStateEvent};
use taste_core::EventBus;
use taste_devcontainer::Supervisor;

#[derive(Clone, Copy, PartialEq)]
enum ButtonAction {
    Reload,
    ViewLog,
    CreateConfig,
    /// Send what is in the entry (or "yes") to the question being asked.
    Answer,
}

pub struct DevcontainerBanner {
    pub widget: gtk::Box,
    revealer: gtk::Revealer,
    title: gtk::Label,
    button: gtk::Button,
    /// The second button a question has: Cancel, or No.
    cancel: gtk::Button,
    /// Where a passphrase or PIN is typed, hidden; and where a username
    /// is, in the clear. One shown at a time, and neither outside a question.
    secret: gtk::PasswordEntry,
    text: gtk::Entry,
    progress: gtk::ProgressBar,
    supervisor: Arc<Supervisor>,
    events: EventBus,
    action: Cell<ButtonAction>,
    /// The question or notice on the strip, if one is: its id and kind.
    /// While it stands, the environment's own faces wait behind it.
    question: RefCell<Option<(u64, AskKind)>>,
    /// The last environment state drawn, to come back to when a question
    /// is answered or a notice withdrawn.
    last_state: RefCell<Option<DevcontainerStateEvent>>,
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
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let secret = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .activates_default(false)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let text = gtk::Entry::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["taste-banner"])
            .build();
        row.append(&title);
        row.append(&secret);
        row.append(&text);
        row.append(&button);
        row.append(&cancel);
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
            cancel: cancel.clone(),
            secret: secret.clone(),
            text: text.clone(),
            progress,
            supervisor,
            events,
            action: Cell::new(ButtonAction::Reload),
            question: RefCell::new(None),
            last_state: RefCell::new(None),
            posed: Cell::new(false),
        });

        // A question's answer: the button, Enter in either entry, or the
        // second button for a cancel.
        {
            let weak = Rc::downgrade(&this);
            secret.connect_activate(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.answer_question();
                }
            });
            let weak = Rc::downgrade(&this);
            text.connect_activate(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.answer_question();
                }
            });
            let weak = Rc::downgrade(&this);
            cancel.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                let taken = this.question.borrow_mut().take();
                if let Some((id, kind)) = taken {
                    // "No" is an answer to a yes/no question; anything else
                    // cancelled is no answer at all.
                    crate::askpass::answer(
                        id,
                        (kind == AskKind::Confirm).then(|| "no".to_string()),
                    );
                    this.finish_question();
                }
            });
        }

        let weak = Rc::downgrade(&this);
        button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            match this.action.get() {
                ButtonAction::Answer => this.answer_question(),
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

    /// Git or ssh has a question, or a notice, for the person: the strip
    /// asks it, and the environment's own face waits behind it.
    pub fn ask(self: &Rc<Self>, id: u64, prompt: &str, kind: AskKind) {
        // One at a time: a second asker while the first stands is answered
        // "no answer" rather than replacing a question mid-type.
        if self.question.borrow().is_some() {
            crate::askpass::answer(id, None);
            return;
        }
        *self.question.borrow_mut() = Some((id, kind));
        let first = prompt.lines().next().unwrap_or(prompt).trim();
        let first = first.trim_end_matches(':').trim();
        self.secret.set_text("");
        self.text.set_text("");
        self.secret.set_visible(kind == AskKind::Secret);
        self.text.set_visible(kind == AskKind::Text);
        match kind {
            AskKind::Secret | AskKind::Text => {
                self.set_title(first);
                self.action.set(ButtonAction::Answer);
                self.set_button(Some("Answer"));
                self.cancel.set_label("Cancel");
                self.cancel.set_visible(true);
            }
            AskKind::Confirm => {
                self.set_title(first);
                self.action.set(ButtonAction::Answer);
                self.set_button(Some("Yes"));
                self.cancel.set_label("No");
                self.cancel.set_visible(true);
            }
            AskKind::Notice => {
                self.set_title(first);
                self.set_button(None);
                self.cancel.set_visible(false);
            }
        }
        self.set_revealed(true);
        if kind == AskKind::Secret {
            self.secret.grab_focus();
        } else if kind == AskKind::Text {
            self.text.grab_focus();
        }
    }

    /// The asker is done — answered, timed out, or (a notice) satisfied —
    /// and the strip goes back to the environment.
    pub fn ask_done(self: &Rc<Self>, id: u64) {
        let ours = self
            .question
            .borrow()
            .is_some_and(|(current, _)| current == id);
        if !ours {
            return;
        }
        self.question.borrow_mut().take();
        self.finish_question();
    }

    fn answer_question(self: &Rc<Self>) {
        let taken = self.question.borrow_mut().take();
        let Some((id, kind)) = taken else { return };
        let answer = match kind {
            AskKind::Secret => self.secret.text().to_string(),
            AskKind::Text => self.text.text().to_string(),
            AskKind::Confirm => "yes".to_string(),
            AskKind::Notice => return,
        };
        crate::askpass::answer(id, Some(answer));
        self.finish_question();
    }

    /// Clear the question's widgets and redraw the environment's face.
    fn finish_question(self: &Rc<Self>) {
        self.secret.set_text("");
        self.secret.set_visible(false);
        self.text.set_visible(false);
        self.cancel.set_visible(false);
        let last = self.last_state.borrow().clone();
        match last {
            Some(state) => self.draw_state(&state),
            None => self.set_revealed(false),
        }
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
        if self.posed.get() || self.question.borrow().is_some() {
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
                || this.question.borrow().is_some()
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
                    "Safe mode — devcontainer.json passed over: {first} (full log under \
                     Logs → Environment Build)"
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

    /// `TASTE_PROBE_BANNER=ready|passed|none|ask|touch`: pose the
    /// running-baseline faces, or a question or a notice from git, without
    /// a checkout in that state, and hold it against the state events that
    /// follow.
    pub fn pose_for_probe(self: &Rc<Self>, kind: &str) {
        self.posed.set(true);
        match kind {
            "ask" => self.ask(u64::MAX, "Enter PIN for authenticator:", AskKind::Secret),
            "touch" => self.ask(
                u64::MAX,
                "Touch your security key — Pull is waiting on it",
                AskKind::Notice,
            ),
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
        *self.last_state.borrow_mut() = Some(state.clone());
        if self.posed.get() || self.question.borrow().is_some() {
            // Remembered above; drawn once the question is over.
            return;
        }
        self.draw_state(state);
    }

    fn draw_state(self: &Rc<Self>, state: &DevcontainerStateEvent) {
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
                     (full log under Logs → Environment Build)"
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
