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

/// How much of the environment build log the repair prompt carries.
const REPAIR_LOG_LINES: usize = 60;

/// How long a security key waits for its touch, as the strip counts it
/// down. FIDO authenticators give about half a minute; the agent's own
/// wait can be shorter, and neither says. "About", and a countdown rather
/// than a promise (David, 2026-09-16: "It should include a countdown for
/// how long I have").
const TOUCH_WINDOW_SECS: u64 = 30;

#[derive(Clone, Copy, PartialEq)]
enum ButtonAction {
    Reload,
    ViewLog,
    CreateConfig,
    /// Send what is in the entry (or "yes") to the question being asked.
    Answer,
    /// Hand the primary's agent the repair: what failed, the log's tail,
    /// and how to work (`repair_prompt`).
    PromptAgent,
    /// Ask the primary's agent to write the definition a project has none
    /// of (`author_prompt`) — set-up, not repair.
    AuthorAgent,
}

/// The faces the strip has while the baseline runs, by what the project's
/// config comes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BaselineFace {
    /// A usable config nobody has built into yet.
    Ready,
    /// The config's image would not build or pull.
    BuildFailed,
    /// The config was refused or could not be read.
    ConfigRefused,
    /// No config at all.
    NoConfig,
}

pub struct DevcontainerBanner {
    pub widget: gtk::Box,
    revealer: gtk::Revealer,
    /// The face's glyph, leading the strip: a fingerprint for a touch, a
    /// key for a secret, a warning or an error for the environment's
    /// trouble (David, 2026-09-16: "lead with a thumbprint icon. Each type
    /// of banner should have a nice icon").
    icon: gtk::Image,
    /// The strip itself, whose colour a face may change.
    /// The banner's coloured surface — the grid the bar and the row share
    /// — which wears `taste-banner` and `attention`, since a row painting
    /// its own colour would paint over the bar beneath it.
    surface: gtk::Grid,
    title: gtk::Label,
    button: gtk::Button,
    /// The second button a question has: Cancel, or No.
    cancel: gtk::Button,
    /// The second button a failed environment has: Prompt Agent, or
    /// Retry. Plain, beside the suggested one.
    secondary: gtk::Button,
    secondary_action: Cell<ButtonAction>,
    /// Where Prompt Agent sends its text and the log it attaches: Dispatch,
    /// aimed at the primary's chat, wired by the window once both exist.
    on_prompt_agent: RefCell<Option<Rc<dyn Fn(String, Option<String>)>>>,
    /// Where a passphrase or PIN is typed, hidden; and where a username
    /// is, in the clear. One shown at a time, and neither outside a question.
    secret: gtk::PasswordEntry,
    text: gtk::Entry,
    supervisor: Arc<Supervisor>,
    events: EventBus,
    action: Cell<ButtonAction>,
    /// The question or notice on the strip, if one is: its id and kind.
    /// While it stands, the environment's own faces wait behind it.
    question: RefCell<Option<(u64, AskKind)>>,
    /// The last environment state drawn, to come back to when a question
    /// is answered or a notice withdrawn.
    last_state: RefCell<Option<DevcontainerStateEvent>>,
    /// When the touch notice went up, for its countdown.
    notice_since: Cell<Option<std::time::Instant>>,
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
        let secondary = gtk::Button::builder()
            .label("Prompt Agent")
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
        let icon = gtk::Image::builder()
            .icon_name("system-run-symbolic")
            .pixel_size(16)
            .valign(gtk::Align::Center)
            .build();
        // The row itself is transparent, its padding carried as margins:
        // the banner's colour is the surface's below, so the bar can draw
        // between the colour and the words.
        // As tall as a button from the start, so a face that gains one
        // (View Log, Rebuild) does not grow the banner (David, 2026-09-22:
        // "The bar should be big enough for the buttons without getting
        // taller when the buttons appear").
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .height_request(34)
            .build();
        row.append(&icon);
        row.append(&title);
        row.append(&secret);
        row.append(&text);
        row.append(&button);
        row.append(&secondary);
        row.append(&cancel);
        // The bar under the row: both in one cell of a grid, which sizes
        // the cell by the row and paints its children in order — the
        // drawing first, the row over it — so the stripes are exactly the
        // row's height and the text sits on them. (An overlay was tried
        // and allocated the row its minimum, wrapping the title to a
        // column.)
        let underlay = gtk::Grid::builder().css_classes(["taste-banner"]).build();
        underlay.attach(&row, 0, 0, 1, 1);
        let revealer = gtk::Revealer::builder()
            .child(&underlay)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&revealer);
        let this = Rc::new(Self {
            widget,
            revealer,
            icon,
            surface: underlay.clone(),
            title,
            button: button.clone(),
            cancel: cancel.clone(),
            secondary: secondary.clone(),
            secondary_action: Cell::new(ButtonAction::PromptAgent),
            on_prompt_agent: RefCell::new(None),
            secret: secret.clone(),
            text: text.clone(),
            supervisor,
            events,
            action: Cell::new(ButtonAction::Reload),
            question: RefCell::new(None),
            last_state: RefCell::new(None),
            notice_since: Cell::new(None),
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

        {
            let weak = Rc::downgrade(&this);
            secondary.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                this.act(this.secondary_action.get());
            });
        }
        let weak = Rc::downgrade(&this);
        button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            this.act(this.action.get());
        });

        this
    }

    /// One button's work, whichever button it is on.
    fn act(self: &Rc<Self>, action: ButtonAction) {
        {
            let this = self;
            match action {
                ButtonAction::Answer => this.answer_question(),
                ButtonAction::PromptAgent => {
                    let (prompt, log) = this.repair_prompt();
                    match this.on_prompt_agent.borrow().as_ref() {
                        Some(send) => send(prompt, log),
                        None => this
                            .events
                            .publish(taste_core::Event::Toast("No chat to prompt yet".into())),
                    }
                }
                ButtonAction::AuthorAgent => match this.on_prompt_agent.borrow().as_ref() {
                    Some(send) => send(author_prompt(), None),
                    None => this
                        .events
                        .publish(taste_core::Event::Toast("No chat to prompt yet".into())),
                },
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
        }
    }

    /// Where Prompt Agent's text and log go: the same path a typed message
    /// takes from Dispatch to the chat (David, 2026-09-16: "You should be
    /// using the same path that Dispatch goes when it goes to chat").
    pub fn set_on_prompt_agent(&self, send: impl Fn(String, Option<String>) + 'static) {
        *self.on_prompt_agent.borrow_mut() = Some(Rc::new(send));
    }

    fn set_secondary(&self, label: Option<&str>, action: ButtonAction) {
        self.secondary_action.set(action);
        match label {
            Some(label) => {
                self.secondary.set_label(label);
                self.secondary.set_visible(true);
            }
            None => self.secondary.set_visible(false),
        }
        // One blue button per face, and on the no-devcontainer face it is
        // the agent writing the definition rather than the blank template:
        // that is the way to a working environment, and Create is the way
        // to write one by hand (David, 2026-09-23: "Make \"Prompt Agent\"
        // the blue highlighted one").
        let agent_leads = label.is_some() && matches!(action, ButtonAction::AuthorAgent);
        if agent_leads {
            self.button.remove_css_class("suggested-action");
            self.secondary.add_css_class("suggested-action");
        } else {
            self.secondary.remove_css_class("suggested-action");
            self.button.add_css_class("suggested-action");
        }
    }

    /// The prompt Prompt Agent sends, and the log it attaches
    /// ([`repair_prompt`], for this banner's environment).
    fn repair_prompt(&self) -> (String, Option<String>) {
        repair_prompt(&self.supervisor)
    }

    fn set_title(&self, text: &str) {
        self.title.set_label(text);
    }

    /// The face's glyph, and whether the strip wears its attention colour
    /// — amber, for the one face that wants the person's hand rather than
    /// their reading: a key waiting to be touched.
    fn set_face(&self, icon: &str, attention: bool) {
        self.icon.set_icon_name(Some(icon));
        if attention {
            self.surface.add_css_class("attention");
        } else {
            self.surface.remove_css_class("attention");
        }
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
                self.set_face(
                    if kind == AskKind::Secret {
                        "dialog-password-symbolic"
                    } else {
                        "dialog-question-symbolic"
                    },
                    false,
                );
                self.set_title(first);
                self.action.set(ButtonAction::Answer);
                self.set_button(Some("Answer"));
                self.cancel.set_label("Cancel");
                self.cancel.set_visible(true);
            }
            AskKind::Confirm => {
                self.set_face("dialog-question-symbolic", false);
                self.set_title(first);
                self.action.set(ButtonAction::Answer);
                self.set_button(Some("Yes"));
                self.cancel.set_label("No");
                self.cancel.set_visible(true);
            }
            AskKind::Notice => {
                self.set_face("auth-fingerprint-symbolic", true);
                self.set_button(None);
                self.cancel.set_visible(false);
                // The countdown: the notice's own words, then how long the
                // key is likely to keep waiting, once a second until the
                // touch lands or the window closes.
                self.notice_since.set(Some(std::time::Instant::now()));
                let base = first.to_string();
                self.set_title(&touch_countdown(&base, 0));
                let weak = Rc::downgrade(self);
                glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
                    let Some(this) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let live = this
                        .question
                        .borrow()
                        .is_some_and(|(current, k)| current == id && k == AskKind::Notice);
                    if !live {
                        return glib::ControlFlow::Break;
                    }
                    let elapsed = this
                        .notice_since
                        .get()
                        .map(|since| since.elapsed().as_secs())
                        .unwrap_or(0);
                    this.set_title(&touch_countdown(&base, elapsed));
                    glib::ControlFlow::Continue
                });
            }
        }
        self.set_secondary(None, ButtonAction::PromptAgent);
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
        self.notice_since.set(None);
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
            let face = this.baseline_face(resolved, reason.as_deref());
            this.show_baseline_face(face);
        });
    }

    /// Which face the resolution comes to.
    fn baseline_face(
        &self,
        resolved: taste_core::ConfigAuthority,
        reason: Option<&str>,
    ) -> BaselineFace {
        match (resolved, reason) {
            (taste_core::ConfigAuthority::Project, _) => BaselineFace::Ready,
            (taste_core::ConfigAuthority::Baseline, Some(_)) if self.supervisor.build_failed() => {
                BaselineFace::BuildFailed
            }
            (taste_core::ConfigAuthority::Baseline, Some(_)) => BaselineFace::ConfigRefused,
            (taste_core::ConfigAuthority::Baseline, None) => BaselineFace::NoConfig,
        }
    }

    /// Safe mode's sentences, fixed and short: the detail is in the log
    /// and in the prompt, not on the strip (David, 2026-09-16: "Replace
    /// this long error with fixed text: Safe mode — failed environment
    /// build [View Log] [Prompt Agent]").
    fn show_baseline_face(&self, face: BaselineFace) {
        match face {
            BaselineFace::Ready => {
                self.set_face("system-run-symbolic", false);
                self.set_title(
                    "Safe mode — devcontainer.json is ready; rebuild to run the project's own \
                     environment",
                );
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Rebuild"));
                self.set_secondary(None, ButtonAction::PromptAgent);
            }
            BaselineFace::BuildFailed => {
                self.set_face("dialog-error-symbolic", false);
                self.set_title("Safe mode — failed environment build");
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
                self.set_secondary(Some("Prompt Agent"), ButtonAction::PromptAgent);
            }
            BaselineFace::ConfigRefused => {
                self.set_face("dialog-warning-symbolic", false);
                self.set_title("Safe mode — invalid devcontainer setup");
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
                self.set_secondary(Some("Prompt Agent"), ButtonAction::PromptAgent);
            }
            BaselineFace::NoConfig => {
                self.set_face("document-new-symbolic", false);
                self.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.set_button(Some("Create"));
                self.set_secondary(Some("Prompt Agent"), ButtonAction::AuthorAgent);
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
            "ready" => self.show_baseline_face(BaselineFace::Ready),
            "failed" => self.show_baseline_face(BaselineFace::BuildFailed),
            "passed" => self.show_baseline_face(BaselineFace::ConfigRefused),
            _ => self.show_baseline_face(BaselineFace::NoConfig),
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
        // Every face below that wants a second button says so; the rest
        // start without one.
        if !matches!(state, DevcontainerStateEvent::Failed { .. }) {
            self.set_secondary(None, ButtonAction::PromptAgent);
        }
        match state {
            DevcontainerStateEvent::ConfigDetected => {
                self.set_face("system-run-symbolic", false);
                self.set_title("Safe mode — devcontainer not running; only its setup is editable");
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Start"));
                self.set_revealed(true);
            }
            // A start's stages are the startup page's (startup.rs), which
            // takes the editor's strip over while they run; the banner
            // says nothing during them.
            DevcontainerStateEvent::Building
            | DevcontainerStateEvent::Starting
            | DevcontainerStateEvent::Preparing { .. } => {
                self.set_revealed(false);
            }
            DevcontainerStateEvent::Running { .. } => {
                // Under the project's config, running is running, drifted
                // or not — the row says which. Under the baseline, this is
                // safe mode, and the banner says what the config comes to.
                self.sync_running();
            }
            DevcontainerStateEvent::Failed { message } => {
                // The baseline itself did not come up (a project image that
                // fails hands over to the baseline instead): the message is
                // the log's, and the strip stays short.
                // An environment that will not start is an error, not a
                // warning (David, 2026-09-21: "That's not just a warning").
                tracing::error!("environment failed: {message}");
                taste_core::app_log::push(
                    "error",
                    "environments",
                    &format!("environment failed: {message}"),
                );
                // The one failed-build face, whichever rung failed: the
                // log, and the agent handed the repair (David, 2026-09-16:
                // "use the same banner as the other failure to minimize
                // divergence").
                self.show_baseline_face(BaselineFace::BuildFailed);
            }
            DevcontainerStateEvent::NoConfig => {
                // State + one action: Create opens the blank config, the
                // same flow as the tree's ghost row.
                self.set_face("document-new-symbolic", false);
                self.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.set_button(Some("Create"));
                // The other way to a definition: the agent writes it
                // (David, 2026-09-23: "This should have the prompt agent
                // option on it").
                self.set_secondary(Some("Prompt Agent"), ButtonAction::AuthorAgent);
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Stopped => {
                self.set_face("media-playback-stop-symbolic", false);
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

/// The touch notice with its countdown: "about N s to touch" while the
/// window is thought open, and "still waiting" once it has passed — the
/// key may yet take the touch, and the step ends the notice when it gives
/// up.
fn touch_countdown(base: &str, elapsed_secs: u64) -> String {
    match TOUCH_WINDOW_SECS.checked_sub(elapsed_secs) {
        Some(left) if left > 0 => format!("{base} · about {left} s to touch"),
        _ => format!("{base} · still waiting"),
    }
}

/// The prompt that asks the agent to write a devcontainer definition for
/// a project that has none — set-up, not repair: nothing failed, so there
/// is no log to attach and no cause to find.
pub(crate) fn author_prompt() -> String {
    "Write this project's devcontainer definition.\n\n\
     WHAT IS TRUE\n\
     The project has no .devcontainer/ yet. The IDE is running its own baseline environment \
     (safe mode) so that one can be written: .devcontainer/ is writable, and the rest of the \
     checkout is read-only until the project's own environment builds.\n\n\
     HOW TO WORK\n\
     1. Learn what the project needs with your file tools: its README, build files, \
     lockfiles, and CI configuration name the languages, tools, and versions.\n\
     2. Write .devcontainer/Containerfile that installs them, and \
     .devcontainer/devcontainer.json that builds it with \
     \"build\": {\"dockerfile\": \"Containerfile\"}. Keep it usable by VS Code and \
     Codespaces. This IDE does not apply devcontainer features: install tools in the \
     Containerfile instead. If the checkout has a Taskfile.yml (taskfile.dev), install Task \
     in the Containerfile too (Fedora's package is go-task; either binary name works): the \
     IDE lists and runs the project's tasks through it.\n\
     If the project runs podman or docker itself (its build, tests, or tasks call it): \
     install podman and fuse-overlayfs; set \"privileged\": true in devcontainer.json (the \
     IDE grants exactly what nested podman needs for it, and VS Code reads it too); after the \
     package install, run `setcap cap_setuid+ep /usr/bin/newuidmap && setcap cap_setgid+ep \
     /usr/bin/newgidmap`, because the image build drops their file capabilities; and give \
     the image's user subordinate IDs inside the container's range, for a user with uid \
     1000: `echo USER:1:999 > /etc/subuid; echo USER:1001:64535 >> /etc/subuid`, and the \
     same for /etc/subgid. useradd's default range does not exist in the container.\n\
     3. Do not run podman, docker, or the build yourself. When the files are ready, call the \
     devcontainer_reload tool once.\n\
     4. Then call the environment tool with include [\"log\"]. If it reports a failure, read \
     the log, fix the files, and reload again. After a start, the IDE checks the container for \
     Task and for nested podman; a failure names what is missing and the change that fixes it. Stop after three attempts and report what you \
     tried.\n\
     5. Finish with one short paragraph: what the environment provides, and why."
        .to_string()
}

/// The prompt Prompt Agent sends for `supervisor`'s environment, and the
/// log it attaches: what happened, the evidence, the exact steps, the
/// walls it will meet, and when to stop. Written to the house rule on
/// agent-facing text — a surface any model can act on without a round
/// trip is cheaper for every model — so the IDE's own reading of the
/// failure rides in the prompt and the log's tail beside it as an
/// attachment, rather than being something the agent has to go and fetch
/// first.
///
/// Two failures, two prompts. A build that did not happen leaves the
/// baseline running and only `.devcontainer/` writable, and the causes
/// are the config's. A lifecycle command that failed leaves the project's
/// own container up and the whole checkout writable, and the cause is
/// what the command needed and did not find; telling that agent its
/// checkout is read-only would be wrong in the way that costs turns.
pub(crate) fn repair_prompt(
    supervisor: &taste_devcontainer::Supervisor,
) -> (String, Option<String>) {
    // The failed build's own lines when a build failed: by the time anyone
    // asks, the live log's tail is the baseline's build, written after it.
    let failed_log = supervisor.failed_build_log();
    let from_failure = failed_log.is_some();
    let log = failed_log.unwrap_or_else(|| supervisor.logs_tail(REPAIR_LOG_LINES));
    let log = (!log.is_empty()).then(|| log.join("\n"));
    let evidence = match &log {
        Some(_) if from_failure => {
            "The failed build's log, as it stood when it failed, is attached as \
             environment-build.log; podman's error is at its end. Read it before changing \
             anything."
        }
        Some(_) => {
            "The last lines of the environment build log are attached as \
             environment-build.log; read them before changing anything."
        }
        None => {
            "The build log is empty; call the environment tool with include [\"log\"] \
             for the latest lines before changing anything."
        }
    };
    let lifecycle = supervisor
        .hook_failure()
        .filter(|_| supervisor.exec().is_container());
    if let Some(reason) = lifecycle {
        let prompt = format!(
            "Fix this project's devcontainer setup so its lifecycle commands succeed.\n\n\
             WHAT HAPPENED\n\
             The IDE built and started the environment from .devcontainer/devcontainer.json. \
             Then one of its lifecycle commands failed, and the commands after it were \
             skipped. The container is running and the whole checkout is writable. The \
             failure, as the IDE read it:\n  {reason}\n{evidence}\n\n\
             HOW TO WORK\n\
             1. Read .devcontainer/devcontainer.json with your file tools, and any file it \
             names (a Containerfile or Dockerfile). Do not guess at their contents.\n\
             2. Find the smallest change that makes the command succeed. Common causes, in \
             order of likelihood:\n\
                - the command needs a tool the image does not have (composer, npm, a \
             language extension): install it in the Containerfile;\n\
                - a \"features\" block was expected to provide it: this IDE does not apply \
             devcontainer features, so install those tools in the Containerfile instead;\n\
                - the command runs in the wrong directory, or before a file it needs \
             exists: give it the path, or move it to a later lifecycle step;\n\
                - the command needs the network or credentials the container does not \
             have.\n\
             You may run the failed command yourself with the ide_exec tool to see its \
             full output; it runs inside the container.\n\
             3. Edit the files under .devcontainer/ that the fix needs.\n\
             4. Do not run podman, docker, or the build yourself. When the files are ready, \
             call the devcontainer_reload tool once. The IDE rebuilds the environment and \
             asks the user before running any lifecycle command.\n\
             5. Then call the environment tool. If it reports lifecycle_failed, call it \
             again with include [\"log\"], read the new failure, and go back to step 2. \
             Stop after three attempts and report what you tried.\n\
             6. Finish with one short paragraph: what was wrong, what you changed, and \
             whether the commands ran."
        );
        return (prompt, log);
    }
    let reason = supervisor
        .config_passed_over()
        .or_else(|| match supervisor.state() {
            taste_devcontainer::SupervisorState::Failed { message } => Some(message),
            _ => None,
        })
        .unwrap_or_else(|| "the environment did not build".to_string());
    let prompt = format!(
        "Fix this project's devcontainer setup so the environment builds.\n\n\
         WHAT HAPPENED\n\
         The IDE tried to build the environment from .devcontainer/devcontainer.json and \
         could not. It is running its own baseline environment instead (safe mode). The \
         failure, as the IDE read it:\n  {reason}\n{evidence}\n\n\
         HOW TO WORK\n\
         1. Read .devcontainer/devcontainer.json with your file tools, and any file it \
         names (a Containerfile or Dockerfile). Do not guess at their contents.\n\
         2. Find the smallest change that makes the build succeed. Common causes, in \
         order of likelihood:\n\
            - an image tag that does not exist on the registry: use a plain, published \
         tag (for example a version like \"8.3\" rather than a variant you are unsure \
         of), or build from a Containerfile instead;\n\
            - a \"features\" block: this IDE does not apply devcontainer features; \
         install those tools in a Containerfile and reference it with \
         \"build\": {{\"dockerfile\": \"Containerfile\"}} in place of \"image\";\n\
            - a mount whose source is outside the workspace, or an object-form mount: \
         bind sources must be under ${{localWorkspaceFolder}}, written in the string \
         form;\n\
            - a postCreateCommand that needs a tool the image does not have.\n\
         3. Edit files under .devcontainer/ only. That directory is writable; the rest \
         of the checkout is read-only until the environment builds.\n\
         4. Do not run podman, docker, or the build yourself. When the files are ready, \
         call the devcontainer_reload tool once. The IDE builds the environment and asks \
         the user before running any lifecycle command.\n\
         5. Then call the environment tool with include [\"log\"]. If it reports a \
         failure, read the new log lines and go back to step 2. Stop after three \
         attempts and report what you tried.\n\
         6. Finish with one short paragraph: what was wrong, what you changed, and \
         whether the environment built."
    );
    (prompt, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_touch_countdown_counts_down_and_then_waits() {
        assert_eq!(
            touch_countdown("Touch your security key", 0),
            "Touch your security key · about 30 s to touch"
        );
        assert_eq!(
            touch_countdown("Touch your security key", 29),
            "Touch your security key · about 1 s to touch"
        );
        assert_eq!(
            touch_countdown("Touch your security key", 30),
            "Touch your security key · still waiting"
        );
        assert_eq!(
            touch_countdown("Touch your security key", 90),
            "Touch your security key · still waiting"
        );
    }
}
