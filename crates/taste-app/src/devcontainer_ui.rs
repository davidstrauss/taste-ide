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
    /// The Virtual Machine log: the stages before a container exists —
    /// the image fetched, the VM booted, the files service, the placement
    /// — are its story, not the build log's.
    ViewVmLog,
    CreateConfig,
    /// Send what is in the entry (or "yes") to the question being asked.
    Answer,
    /// Hand the primary's agent the repair: what failed, the log's tail,
    /// and how to work (`repair_prompt`).
    PromptAgent,
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
    /// The operation's bar, drawn UNDER the row as its background: hazard
    /// stripes filling the row from the left to how far the operation has
    /// come, sliding left while it runs (David, 2026-09-21: "make it the
    /// background of the text ... Animate the bar moving to the left").
    bar: gtk::DrawingArea,
    /// How far the operation in progress has come, 0–1, never moving
    /// backwards within one operation; and the build step the log last
    /// named, which is the bar's fine grain while the image builds.
    progress_value: Cell<f64>,
    build_step: Cell<Option<(u32, u32)>>,
    /// The stripes' phase, in pixels, and the frame-clock tick that
    /// advances it while an operation runs.
    bar_offset: Cell<f64>,
    bar_tick: RefCell<Option<gtk::TickCallbackId>>,
    /// An operation has been drawn and has not ended: the next Running is
    /// its end, and gets the Done face for a moment before the banner
    /// goes.
    operation_underway: Cell<bool>,
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
        let bar = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .can_target(false)
            .build();
        let underlay = gtk::Grid::builder().css_classes(["taste-banner"]).build();
        underlay.attach(&bar, 0, 0, 1, 1);
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
            bar,
            progress_value: Cell::new(0.0),
            build_step: Cell::new(None),
            bar_offset: Cell::new(0.0),
            bar_tick: RefCell::new(None),
            operation_underway: Cell::new(false),
            supervisor,
            events,
            action: Cell::new(ButtonAction::Reload),
            question: RefCell::new(None),
            last_state: RefCell::new(None),
            notice_since: Cell::new(None),
            posed: Cell::new(false),
        });
        this.install_bar();

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
                ButtonAction::ViewLog => {
                    this.events.publish(taste_core::Event::ShowDevcontainerLog);
                }
                ButtonAction::ViewVmLog => {
                    this.events.publish(taste_core::Event::ShowVmLog);
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
        self.secondary.set_visible(false);
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

    /// The bar under the banner: one operation — the VM coming up, the
    /// checkout placed, the image built, the container started — as one
    /// bar that reaches full exactly when the environment is ready
    /// (David, 2026-09-21: "show a progress bar encompassing the entire
    /// operation ... once it reaches full, my env should be entirely
    /// ready"). `None` ends the operation and hides the bar; a fraction
    /// only ever moves it forward, since the steps come in order and a
    /// bar that drops back reads as a second operation.
    fn set_progress(self: &Rc<Self>, fraction: Option<f64>) {
        match fraction {
            None => {
                self.progress_value.set(0.0);
                self.build_step.set(None);
                if let Some(tick) = self.bar_tick.borrow_mut().take() {
                    tick.remove();
                }
                self.bar.queue_draw();
            }
            Some(fraction) => {
                let fraction = fraction.clamp(0.0, 1.0).max(self.progress_value.get());
                self.progress_value.set(fraction);
                self.bar.queue_draw();
                if self.bar_tick.borrow().is_some() {
                    return;
                }
                // The stripes slide left on the frame clock, a steady
                // pace in pixels per second whatever the frame rate.
                let weak = Rc::downgrade(self);
                let last: Cell<Option<i64>> = Cell::new(None);
                let id = self.bar.add_tick_callback(move |bar, clock| {
                    let Some(this) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let now = clock.frame_time();
                    if let Some(before) = last.get() {
                        let dt = (now - before) as f64 / 1_000_000.0;
                        let offset = (this.bar_offset.get() - STRIPE_SPEED * dt) % STRIPE_PERIOD;
                        this.bar_offset.set(offset);
                    }
                    last.set(Some(now));
                    bar.queue_draw();
                    glib::ControlFlow::Continue
                });
                *self.bar_tick.borrow_mut() = Some(id);
            }
        }
    }

    /// Wire the bar's drawing to this banner's state, once it exists.
    fn install_bar(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.bar.set_draw_func(move |_, cr, width, height| {
            let Some(this) = weak.upgrade() else { return };
            draw_stripes(
                cr,
                width,
                height,
                this.progress_value.get(),
                this.bar_offset.get(),
                adw::StyleManager::default().is_dark(),
            );
        });
    }

    /// The guest image's fetch, decompression, or verification: the
    /// operation's first stage, once per machine, ahead of the VM's boot
    /// (David, 2026-09-22: "include downloading the VM image -- if
    /// necessary -- from the internet as the first step of the
    /// 'construction'"). Drawn only while the environment is being
    /// readied, never over a running one.
    pub fn on_guest_image(self: &Rc<Self>, fetch: &taste_core::GuestImageFetch) {
        use taste_core::GuestImagePhase as P;
        if !fetch.phase.active() {
            return;
        }
        let settled = matches!(
            self.last_state.borrow().as_ref(),
            Some(
                DevcontainerStateEvent::Running { .. }
                    | DevcontainerStateEvent::Building
                    | DevcontainerStateEvent::Starting
            )
        );
        if settled || self.posed.get() || self.question.borrow().is_some() {
            return;
        }
        let mib = |bytes: u64| bytes / (1024 * 1024);
        let (title, fraction) = match fetch.phase {
            P::Fetching if fetch.total > 0 => (
                format!(
                    "Getting ready — fetching the guest image ({} of {} MiB)",
                    mib(fetch.done),
                    mib(fetch.total)
                ),
                0.01 + 0.06 * (fetch.done as f64 / fetch.total as f64),
            ),
            P::Fetching => ("Getting ready — fetching the guest image".to_string(), 0.01),
            P::Decompressing => (
                "Getting ready — unpacking the guest image".to_string(),
                0.075,
            ),
            _ => (
                "Getting ready — verifying the guest image".to_string(),
                0.09,
            ),
        };
        self.set_face("emblem-synchronizing-symbolic", true);
        self.set_title(&title);
        self.action.set(ButtonAction::ViewVmLog);
        self.set_button(Some("View Log"));
        self.set_revealed(true);
        self.set_progress(Some(fraction));
    }

    /// A line of the environment's build log: the image build's `STEP
    /// n/m` moves the bar through the build's share of the operation.
    pub fn on_log_line(self: &Rc<Self>, line: &str) {
        let Some(step) = build_step(line) else {
            return;
        };
        self.build_step.set(Some(step));
        if matches!(
            self.last_state.borrow().as_ref(),
            Some(DevcontainerStateEvent::Building)
        ) {
            self.set_progress(Some(operation_fraction(
                &DevcontainerStateEvent::Building,
                Some(step),
            )));
            self.set_title(&format!(
                "Getting ready — building the image (step {} of {})",
                step.0, step.1
            ));
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
                self.set_secondary(None, ButtonAction::PromptAgent);
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
            // Mid-operation: the image build at its fourth step of nine,
            // the bar in its hazard stripes that far along.
            "building" => {
                self.posed.set(false);
                self.build_step.set(Some((4, 9)));
                self.on_state(&DevcontainerStateEvent::Building);
                self.posed.set(true);
            }
            // The operation's last face: the bar full behind Done.
            "done" => {
                self.posed.set(false);
                self.operation_underway.set(true);
                self.on_state(&DevcontainerStateEvent::Running {
                    container_id: "posed".into(),
                });
                self.posed.set(true);
            }
            // The operation's first face: the VM coming up, the bar just
            // begun.
            "vm" => {
                self.posed.set(false);
                self.on_state(&DevcontainerStateEvent::Preparing {
                    what: "bringing up the workspace's VM".into(),
                });
                self.posed.set(true);
            }
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
        // The operation's bar: a transitional state moves it, a settled
        // one ends it. Every transitional face wears the same glyph and
        // the same "Getting ready —" so the sequence reads as one thing
        // happening (David, 2026-09-21: "a consistent presentation in the
        // header banner").
        match state {
            DevcontainerStateEvent::Preparing { .. }
            | DevcontainerStateEvent::Building
            | DevcontainerStateEvent::Starting => {
                self.operation_underway.set(true);
                self.set_progress(Some(operation_fraction(state, self.build_step.get())));
            }
            // The end of an operation: the bar full behind "Done" for a
            // moment, then the running face (David, 2026-09-22: "end with
            // a 'Done -- environment ready' state for a second or two at
            // the end of the process. The whole bar should be filled").
            DevcontainerStateEvent::Running { .. } if self.operation_underway.get() => {
                self.operation_underway.set(false);
                self.set_progress(Some(1.0));
                self.set_face("emblem-ok-symbolic", true);
                self.set_title("Done — environment ready");
                self.set_button(None);
                self.set_revealed(true);
                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(DONE_LINGER, move || {
                    let Some(this) = weak.upgrade() else { return };
                    // A posed banner is a still: it keeps the face.
                    if this.posed.get() {
                        return;
                    }
                    // Still running, and nothing else has taken the banner
                    // since: end the bar and draw the running face.
                    if matches!(
                        this.last_state.borrow().as_ref(),
                        Some(DevcontainerStateEvent::Running { .. })
                    ) && !this.operation_underway.get()
                    {
                        this.set_progress(None);
                        this.sync_running();
                    }
                });
                return;
            }
            _ => {
                self.operation_underway.set(false);
                self.set_progress(None);
            }
        }

        match state {
            DevcontainerStateEvent::ConfigDetected => {
                self.set_face("system-run-symbolic", false);
                self.set_title("Safe mode — devcontainer not running; only its setup is editable");
                self.action.set(ButtonAction::Reload);
                self.set_button(Some("Start"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Building => {
                self.set_face("emblem-synchronizing-symbolic", true);
                self.set_title(&match self.build_step.get() {
                    Some((n, of)) => {
                        format!("Getting ready — building the image (step {n} of {of})")
                    }
                    None => "Getting ready — building the image".to_string(),
                });
                self.action.set(ButtonAction::ViewLog);
                self.set_button(Some("View Log"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::Starting => {
                self.set_face("emblem-synchronizing-symbolic", true);
                self.set_title("Getting ready — starting the container and its setup commands");
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
            DevcontainerStateEvent::Preparing { what } => {
                // What the IDE is doing to get the environment somewhere it
                // can run, with the log that tells it one press away
                // (David, 2026-09-22: "Each of the stages should have a
                // button to 'View Logs', whenever possible").
                self.set_face("emblem-synchronizing-symbolic", true);
                // One line at the window's narrowest: a title that wraps
                // grows the window past its minimum height.
                self.set_title(&format!("Getting ready — {what}"));
                self.action.set(ButtonAction::ViewVmLog);
                self.set_button(Some("View Log"));
                self.set_revealed(true);
            }
            DevcontainerStateEvent::NoConfig => {
                // State + one action: Create opens the blank config, the
                // same flow as the tree's ghost row.
                self.set_face("document-new-symbolic", false);
                self.set_title("Safe mode — no devcontainer");
                self.action.set(ButtonAction::CreateConfig);
                self.set_button(Some("Create"));
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

/// The stripes' pitch — one yellow band and one dark, in pixels — and
/// how fast they slide left, in pixels per second: a walking pace, so
/// the bar reads as working without drawing the eye from the words on
/// it.
const STRIPE_PERIOD: f64 = 28.0;
const STRIPE_SPEED: f64 = 12.0;
/// How long the full bar and "Done" stay before the running face.
const DONE_LINGER: std::time::Duration = std::time::Duration::from_millis(2000);

/// The operation's bar as the row's whole background: the part done in
/// hazard bands at 45°, phase-shifted by `offset`, the part to come in a
/// flat grey. Both opaque, and both kept well away from the text's
/// colour — dark bands and a dark grey under the dark scheme's light
/// text, pale bands and a light grey under the light scheme's dark text
/// (David, 2026-09-21: "much higher contrast versus the text ... Make the
/// incomplete portion of the bar dark gray (or light gray in light
/// mode)"). Nothing is drawn at zero; the banner's own colour shows.
fn draw_stripes(
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    fraction: f64,
    offset: f64,
    dark: bool,
) {
    let filled = f64::from(width) * fraction.clamp(0.0, 1.0);
    if filled <= 0.5 || height <= 0 {
        return;
    }
    let h = f64::from(height);
    let (yellow, other, remainder) = if dark {
        (
            (0.22, 0.18, 0.03, 1.0),
            (0.08, 0.08, 0.08, 1.0),
            (0.14, 0.14, 0.14, 1.0),
        )
    } else {
        (
            (1.0, 0.96, 0.80, 1.0),
            (0.99, 0.99, 0.99, 1.0),
            (0.92, 0.92, 0.91, 1.0),
        )
    };
    cr.set_source_rgba(remainder.0, remainder.1, remainder.2, remainder.3);
    cr.rectangle(0.0, 0.0, f64::from(width), h);
    let _ = cr.fill();
    cr.save().ok();
    cr.rectangle(0.0, 0.0, filled, h);
    cr.clip();
    let half = STRIPE_PERIOD / 2.0;
    // Each band is a parallelogram leaning left: its top edge `half` wide
    // at `x`, its bottom edge shifted by the height, so the bands run at
    // 45° and a leftward slide of the phase reads as leftward motion.
    let mut x = (offset % STRIPE_PERIOD) - STRIPE_PERIOD - h;
    let mut yellow_band = true;
    while x < filled + h {
        let (r, g, b, a) = if yellow_band { yellow } else { other };
        cr.set_source_rgba(r, g, b, a);
        cr.move_to(x, 0.0);
        cr.line_to(x + half, 0.0);
        cr.line_to(x + half - h, h);
        cr.line_to(x - h, h);
        cr.close_path();
        let _ = cr.fill();
        x += half;
        yellow_band = !yellow_band;
    }
    cr.restore().ok();
}

/// Where one operation stands, 0–1, from the state it is in and the
/// build step the log last named. The shares are the time each phase
/// takes on this machine, roughly: the VM's boot and the files service
/// are the first quarter, placing the checkout a little more, the image
/// build the middle half — it is the only phase with a grain of its own,
/// podman's `STEP n/m` — and the container's start and setup commands
/// the last stretch. Running is 1.0 and is not asked of this: the bar is
/// gone by then.
fn operation_fraction(state: &DevcontainerStateEvent, build_step: Option<(u32, u32)>) -> f64 {
    match state {
        DevcontainerStateEvent::Preparing { what } => {
            let what = what.to_lowercase();
            if what.contains("files service") {
                0.25
            } else if what.contains("checkout") {
                0.32
            } else if what.contains("vm") {
                0.10
            } else {
                0.15
            }
        }
        DevcontainerStateEvent::Building => match build_step {
            Some((n, of)) if of > 0 => 0.40 + 0.40 * (f64::from(n.min(of)) / f64::from(of)),
            _ => 0.40,
        },
        DevcontainerStateEvent::Starting => 0.85,
        DevcontainerStateEvent::Running { .. } => 1.0,
        _ => 0.0,
    }
}

/// `STEP 3/9: RUN …`, as podman prints an image build's steps, read as
/// (3, 9); anything else is not a step.
fn build_step(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("STEP ")?;
    let (n, of) = rest.split_once('/')?;
    let of = of.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((n.trim().parse().ok()?, of.parse().ok()?))
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
    let log = supervisor.logs_tail(REPAIR_LOG_LINES);
    let log = (!log.is_empty()).then(|| log.join("\n"));
    let evidence = match &log {
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
    fn the_operation_bar_only_moves_forward_and_reads_the_build_steps() {
        use taste_core::event::DevcontainerStateEvent as S;
        assert_eq!(build_step("STEP 3/9: RUN dnf install -y gcc"), Some((3, 9)));
        assert_eq!(build_step("STEP 12/12: COMMIT localhost/x"), Some((12, 12)));
        assert_eq!(build_step("Successfully tagged"), None);
        let vm = operation_fraction(
            &S::Preparing {
                what: "bringing up the workspace's VM".into(),
            },
            None,
        );
        let files = operation_fraction(
            &S::Preparing {
                what: "connecting the files service".into(),
            },
            None,
        );
        let build_start = operation_fraction(&S::Building, None);
        let build_mid = operation_fraction(&S::Building, Some((5, 10)));
        let build_end = operation_fraction(&S::Building, Some((10, 10)));
        let start = operation_fraction(&S::Starting, None);
        assert!(
            vm < files && files < build_start,
            "{vm} {files} {build_start}"
        );
        assert!(build_start < build_mid && build_mid < build_end && build_end <= start);
        assert!(start < 1.0);
    }

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
