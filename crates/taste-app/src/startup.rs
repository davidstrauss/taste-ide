//! The environment's startup, as a page: the ordered checklist of what
//! has to happen before the environment can be used, the log those steps
//! are writing, and the reason when the safe-mode environment is what is
//! coming up instead of the project's own.
//!
//! It takes the editor's strip over while it exists — a pinned tab, the
//! current one, ahead of every file — because almost nothing can be done
//! before at least a safe-mode container is up, and a banner's one line
//! over a strip of files the user cannot act on told them less than the
//! whole story would (David, 2026-09-22: "Take over the main code
//! view/editing panel/region ... an ordered checklist of what's done and
//! needs to be done before the env is available for use"). Under it, the
//! construction stripes the banner's bar wore, slowed right down: a
//! pattern this large moving at a walk would be the only thing in the
//! window anyone could look at.
//!
//! Two kinds of fallback are told apart, because they mean different
//! things for the person reading (David, same day). A project whose
//! devcontainer is missing or broken gets the safe-mode environment and
//! a button that hands the agent the repair: that is the case this page
//! exists to make ordinary. The safe-mode environment itself failing is
//! the machine's setup, the VM provider, or a bug in the IDE — said so,
//! with no agent to prompt, since there is none.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::event::DevcontainerStateEvent;

use crate::logview::{LogKind, LogPage};

/// How fast the stripes slide, in pixels per second: a page-sized pattern
/// wants to be seen moving only by someone who watches for it.
const STRIPE_SPEED: f64 = 2.0;

/// How long "Environment ready" stays on the page before it goes.
pub const READY_LINGER: std::time::Duration = std::time::Duration::from_millis(2000);

/// The steps, in the order they happen. Not every start does work at
/// every step — the guest image is fetched once per machine, the files
/// service's image built once per VM — so each is worded as a state to
/// be in rather than a thing to do, and a step a start did not need is
/// simply checked: it was already true (David, 2026-09-22: "Word steps so
/// that, if unnecessary, it's idempotently correct to show them as
/// checked").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Other projects' VMs that no window owns, stopped for the room.
    Sweep,
    /// The guest image fetched, unpacked, and verified — once per machine.
    GuestImage,
    /// The workspace's VM booted and answering.
    Vm,
    /// The files service's image built in the VM — once per VM.
    ServiceImage,
    /// The files service connected.
    Files,
    /// The checkout placed in the VM and synced with the folder.
    Place,
    /// The environment's image built.
    Build,
    /// The container started and its setup commands run.
    Start,
    /// The environment is up.
    Ready,
}

impl Step {
    pub const ALL: [Step; 9] = [
        Step::Sweep,
        Step::GuestImage,
        Step::Vm,
        Step::ServiceImage,
        Step::Files,
        Step::Place,
        Step::Build,
        Step::Start,
        Step::Ready,
    ];

    fn title(self) -> &'static str {
        match self {
            Step::Sweep => "Stop unused VMs and verify capacity",
            Step::GuestImage => "Have the guest image on this machine",
            Step::Vm => "Have the workspace's VM up",
            Step::ServiceImage => "Have the files service image in the VM",
            Step::Files => "Have the files service connected",
            Step::Place => "Have the checkout in the VM, in step with the folder",
            Step::Build => "Have the environment's image built",
            Step::Start => "Have the container running with its setup done",
            Step::Ready => "Environment ready",
        }
    }

    /// Which log tells this step's story.
    fn log(self) -> LogKind {
        match self {
            Step::Build | Step::Start | Step::Ready => LogKind::Environment,
            _ => LogKind::Vm,
        }
    }

    /// The step a `Preparing` state's words name.
    fn for_preparing(what: &str) -> Step {
        let what = what.to_lowercase();
        if what.contains("no window owns") {
            Step::Sweep
        } else if what.contains("guest image") {
            Step::GuestImage
        } else if what.contains("service image") {
            Step::ServiceImage
        } else if what.contains("files service") {
            Step::Files
        } else if what.contains("checkout") {
            Step::Place
        } else {
            Step::Vm
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Active,
    Done,
    Failed,
}

struct StepRow {
    row: gtk::Box,
    icon: gtk::Image,
    spinner: gtk::Spinner,
    title: gtk::Label,
    detail: gtk::Label,
    status: Cell<Status>,
    /// What the step came to, once it is done — shown under its check.
    conclusion: RefCell<Option<String>>,
}

impl StepRow {
    fn new(step: Step) -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let mark = gtk::Stack::builder()
            .hhomogeneous(true)
            .vhomogeneous(true)
            .valign(gtk::Align::Start)
            .build();
        let icon = gtk::Image::builder()
            .icon_name("radio-symbolic")
            .pixel_size(16)
            .build();
        let spinner = gtk::Spinner::new();
        mark.add_named(&icon, Some("icon"));
        mark.add_named(&spinner, Some("spinner"));
        mark.set_visible_child_name("icon");
        let words = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let title = gtk::Label::builder()
            .label(step.title())
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            // Tabular figures: a detail carrying a percentage or a count
            // redraws each second, and proportional digits would shuffle
            // the words after them at every one.
            .css_classes(["caption", "dim-label", "numeric"])
            .visible(false)
            .build();
        words.append(&title);
        words.append(&detail);
        row.append(&mark);
        row.append(&words);
        let this = Self {
            row,
            icon,
            spinner,
            title,
            detail,
            status: Cell::new(Status::Pending),
            conclusion: RefCell::new(None),
        };
        this.set(Status::Pending, None);
        this
    }

    fn set(&self, status: Status, detail: Option<&str>) {
        self.status.set(status);
        let mark = self.icon.parent().and_downcast::<gtk::Stack>();
        for class in [
            "startup-step-pending",
            "startup-step-active",
            "startup-step-done",
            "startup-step-failed",
        ] {
            self.row.remove_css_class(class);
        }
        let (icon, class) = match status {
            Status::Pending => ("radio-symbolic", "startup-step-pending"),
            Status::Active => ("radio-symbolic", "startup-step-active"),
            Status::Done => ("object-select-symbolic", "startup-step-done"),
            Status::Failed => ("dialog-error-symbolic", "startup-step-failed"),
        };
        self.row.add_css_class(class);
        self.icon.set_icon_name(Some(icon));
        if let Some(mark) = mark {
            if status == Status::Active {
                self.spinner.start();
                mark.set_visible_child_name("spinner");
            } else {
                self.spinner.stop();
                mark.set_visible_child_name("icon");
            }
        }
        // The active step says what it is doing — its current substep,
        // "Step 2 of 9: RUN dnf install" — and a checked one what it came
        // to — "Stopped 2 unused VMs. 12.0 GiB of memory available for IDE
        // VMs." A pending step is its title alone, and so is a checked one
        // with no conclusion to give (David, 2026-09-22: "I would like
        // descriptions under the completed env rebuild steps, but I want
        // them to state the summary/conclusion"). A failed one says why.
        let show = |text: &str| {
            self.detail.set_label(text);
            self.detail.set_visible(true);
        };
        match (status, detail) {
            (Status::Active | Status::Failed, Some(text)) if !text.is_empty() => show(text),
            (Status::Active, None) => {
                // Keep the last substep until the next one arrives.
            }
            (Status::Done, _) => match self.conclusion.borrow().as_deref() {
                Some(text) => show(text),
                None => self.detail.set_visible(false),
            },
            _ => self.detail.set_visible(false),
        }
        if status == Status::Active {
            self.title.add_css_class("heading");
        } else {
            self.title.remove_css_class("heading");
        }
    }
}

pub struct StartupPage {
    pub widget: gtk::Widget,
    heading: gtk::Label,
    note: gtk::Label,
    prompt: gtk::Button,
    rows: Vec<StepRow>,
    logs: gtk::Stack,
    vm_log: Rc<LogPage>,
    build_log: Rc<LogPage>,
    current: Cell<Option<Step>>,
    /// When the current step became current, for the conclusions the
    /// page works out itself — the build's and the container's.
    since: Cell<Option<std::time::Instant>>,
    /// The image build's step count and how many of them the cache
    /// answered, read off its log.
    build_steps: Cell<(u32, u32)>,
    /// Whether a start is under way: set by the first transitional state,
    /// cleared when the page is reset for the next start.
    underway: Cell<bool>,
    /// The start reached Running and is lingering on "ready"; a new
    /// transitional state clears it (`begin`).
    settled: Cell<bool>,
    area: gtk::DrawingArea,
    offset: Cell<f64>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    on_prompt: RefCell<Option<Rc<dyn Fn()>>>,
}

impl StartupPage {
    pub fn new() -> Rc<Self> {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .can_target(false)
            .build();

        let heading = gtk::Label::builder()
            .label("Environment starting")
            .css_classes(["title-2"])
            .xalign(0.0)
            .build();
        let note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .build();
        let prompt = gtk::Button::builder()
            .label("Prompt Agent")
            .tooltip_text(
                "Hand the agent what failed and the log's tail, and ask it to diagnose and \
                 repair the devcontainer",
            )
            .css_classes(["suggested-action"])
            .halign(gtk::Align::Start)
            .visible(false)
            .build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let rows: Vec<StepRow> = Step::ALL
            .iter()
            .map(|step| {
                let row = StepRow::new(*step);
                list.append(&row.row);
                row
            })
            .collect();
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .css_classes(["startup-card"])
            .build();
        card.append(&heading);
        card.append(&note);
        card.append(&list);
        card.append(&prompt);
        let clamp = adw::Clamp::builder()
            .maximum_size(640)
            .tightening_threshold(480)
            .child(&card)
            .margin_top(24)
            .margin_bottom(12)
            .margin_start(24)
            .margin_end(24)
            .build();

        // The log the current step is writing, below the checklist: the
        // VM's story until the container's build begins, the environment's
        // build log from there.
        let vm_log = LogPage::new(LogKind::Vm, "primary", &[]);
        let build_log = LogPage::new(LogKind::Environment, "primary", &[]);
        let logs = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .css_classes(["startup-log"])
            .margin_start(24)
            .margin_end(24)
            .margin_bottom(24)
            .build();
        logs.add_named(&vm_log.widget, Some("vm"));
        logs.add_named(&build_log.widget, Some("build"));
        logs.set_visible_child_name("vm");

        // The log gives way first: it fills whatever the checklist leaves
        // and shrinks with the pane down to five lines, and below that the
        // page scrolls as one — checklist, note, and log — rather than
        // running off the bottom of the pane, clipped (David, 2026-09-22:
        // "If you can't even show 5 lines, then the env rebuild panel
        // should be scrollable").
        vm_log.set_min_lines(5);
        build_log.set_min_lines(5);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&clamp);
        content.append(&logs);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            // The page's natural height is still its content's, so the
            // editor's split is set as it was; only its minimum is the
            // scroller's.
            .propagate_natural_height(true)
            .child(&content)
            .build();
        let overlay = gtk::Overlay::builder().child(&area).build();
        overlay.add_overlay(&scroller);

        let this = Rc::new(Self {
            widget: overlay.upcast(),
            heading,
            note,
            prompt,
            rows,
            logs,
            vm_log,
            build_log,
            current: Cell::new(None),
            since: Cell::new(None),
            build_steps: Cell::new((0, 0)),
            underway: Cell::new(false),
            settled: Cell::new(false),
            area,
            offset: Cell::new(0.0),
            tick: RefCell::new(None),
            on_prompt: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&this);
            this.area.set_draw_func(move |_, cr, width, height| {
                let Some(this) = weak.upgrade() else { return };
                crate::stripes::draw(
                    cr,
                    width,
                    height,
                    1.0,
                    this.offset.get(),
                    adw::StyleManager::default().is_dark(),
                );
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.prompt.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                let hook = this.on_prompt.borrow().clone();
                if let Some(hook) = hook {
                    hook();
                }
            });
        }
        this
    }

    /// Where Prompt Agent goes: the primary's chat, with the repair.
    pub fn set_on_prompt_agent(&self, hook: impl Fn() + 'static) {
        *self.on_prompt.borrow_mut() = Some(Rc::new(hook));
    }

    /// The two logs, for the editor to wire their follow state to its
    /// toggle.
    pub fn logs(&self) -> (Rc<LogPage>, Rc<LogPage>) {
        (self.vm_log.clone(), self.build_log.clone())
    }

    /// The log on screen now — what the strip's follow toggle acts on.
    pub fn current_log(&self) -> Rc<LogPage> {
        if self.logs.visible_child_name().as_deref() == Some("build") {
            self.build_log.clone()
        } else {
            self.vm_log.clone()
        }
    }

    /// Whether a start is being drawn.
    pub fn underway(&self) -> bool {
        self.underway.get()
    }

    /// A fresh start: every step pending, the note gone, the stripes
    /// moving.
    fn begin(self: &Rc<Self>) {
        self.underway.set(true);
        self.settled.set(false);
        self.current.set(None);
        self.heading.set_label("Environment starting");
        self.note.set_visible(false);
        self.prompt.set_visible(false);
        self.since.set(None);
        self.build_steps.set((0, 0));
        for row in &self.rows {
            row.conclusion.replace(None);
            row.set(Status::Pending, None);
            row.detail.set_visible(false);
        }
        self.logs.set_visible_child_name("vm");
        self.start_animation();
    }

    /// The environment is ready and the page is lingering on the fact.
    pub fn mark_settled(&self) {
        self.settled.set(true);
    }

    pub fn settled(&self) -> bool {
        self.settled.get()
    }

    /// The start is over, one way or another: the stripes rest.
    pub fn end(&self) {
        self.underway.set(false);
        self.settled.set(false);
        if let Some(tick) = self.tick.borrow_mut().take() {
            tick.remove();
        }
    }

    fn start_animation(self: &Rc<Self>) {
        if self.tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let last: Cell<Option<i64>> = Cell::new(None);
        let id = self.area.add_tick_callback(move |area, clock| {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = clock.frame_time();
            if let Some(before) = last.get() {
                let dt = (now - before) as f64 / 1_000_000.0;
                this.offset
                    .set((this.offset.get() - STRIPE_SPEED * dt) % crate::stripes::PERIOD);
            }
            last.set(Some(now));
            area.queue_draw();
            glib::ControlFlow::Continue
        });
        *self.tick.borrow_mut() = Some(id);
    }

    fn row(&self, step: Step) -> &StepRow {
        &self.rows[Step::ALL.iter().position(|s| *s == step).unwrap_or(0)]
    }

    /// `step` is happening now: every step before it is checked — the
    /// one that was active is done, and the ones that never announced
    /// themselves were already true — and the log below switches to the
    /// one this step writes.
    fn activate(self: &Rc<Self>, step: Step, detail: Option<&str>) {
        if !self.underway.get() || self.settled.get() {
            self.begin();
        }
        for earlier in Step::ALL.iter().filter(|s| **s < step) {
            let row = self.row(*earlier);
            if row.status.get() == Status::Active {
                self.conclude_own(*earlier);
            }
            if matches!(row.status.get(), Status::Pending | Status::Active) {
                row.set(Status::Done, None);
            }
        }
        let row = self.row(step);
        if row.status.get() != Status::Failed {
            row.set(Status::Active, detail);
        }
        if self.current.get() != Some(step) {
            self.since.set(Some(std::time::Instant::now()));
        }
        self.current.set(Some(step));
        self.logs.set_visible_child_name(match step.log() {
            LogKind::Environment => "build",
            _ => "vm",
        });
    }

    /// The conclusions the page works out from what it watched, for the
    /// steps that happen in the environment's own build rather than in
    /// the registry: how long, and — for the image — how much of it the
    /// cache already had.
    fn conclude_own(&self, step: Step) {
        let row = self.row(step);
        if row.conclusion.borrow().is_some() {
            return;
        }
        let Some(took) = self.since.get().map(|since| since.elapsed()) else {
            return;
        };
        let took = duration_words(took);
        let words = match step {
            Step::Build => match self.build_steps.get() {
                (0, _) => format!("Image ready in {took}."),
                // A FROM step never says "Using cache": the rest all did.
                (of, cached) if cached + 1 >= of => {
                    format!("Up to date: all {of} steps from the cache, in {took}.")
                }
                (of, 0) => format!("Built in {took}: {of} steps."),
                (of, cached) => format!("Built in {took}: {of} steps, {cached} from the cache."),
            },
            Step::Start => format!("Container up in {took}."),
            _ => return,
        };
        row.conclusion.replace(Some(words));
    }

    /// A step's conclusion as the registry says it, shown under its check
    /// once the step is done — now, when it already is.
    pub fn on_concluded(&self, stage: taste_core::StartupStage, summary: &str) {
        use taste_core::StartupStage as S;
        if !self.underway.get() {
            return;
        }
        let step = match stage {
            S::Sweep => Step::Sweep,
            S::GuestImage => Step::GuestImage,
            S::Vm => Step::Vm,
            S::ServiceImage => Step::ServiceImage,
            S::Files => Step::Files,
            S::Place => Step::Place,
        };
        let row = self.row(step);
        // Whole, and wrapped if it must: a conclusion is read once and
        // does not move, so there is nothing to keep to one line.
        row.conclusion.replace(Some(summary.trim().to_string()));
        if row.status.get() == Status::Done {
            row.set(Status::Done, None);
        }
    }

    /// Every step done; the heading says so.
    fn finish(self: &Rc<Self>) {
        if let Some(step) = self.current.get() {
            if self.row(step).status.get() == Status::Active {
                self.conclude_own(step);
            }
        }
        for step in Step::ALL {
            let row = self.row(step);
            if row.status.get() != Status::Failed {
                row.set(Status::Done, None);
            }
        }
        self.row(Step::Ready).set(Status::Done, None);
        self.current.set(Some(Step::Ready));
        self.heading.set_label("Environment ready");
    }

    /// The state, as the supervisor tells it. `baseline` says whose
    /// container the state is about — the project's own, or the safe-mode
    /// environment's — which is what decides how a failure is read.
    pub fn on_state(self: &Rc<Self>, state: &DevcontainerStateEvent, baseline: bool) {
        match state {
            DevcontainerStateEvent::Preparing { what } => {
                self.activate(Step::for_preparing(what), None);
            }
            DevcontainerStateEvent::Building => self.activate(Step::Build, None),
            DevcontainerStateEvent::Starting => self.activate(Step::Start, None),
            DevcontainerStateEvent::Running { .. } => {
                if self.underway.get() {
                    self.finish();
                }
            }
            DevcontainerStateEvent::Failed { message } => {
                if !self.underway.get() {
                    self.begin();
                }
                let first = message.lines().next().unwrap_or(message).trim().to_string();
                let current = self.current.get().unwrap_or(Step::Build);
                self.row(current).set(Status::Failed, Some(&first));
                if baseline {
                    // Not the project's doing: the safe-mode environment is
                    // the IDE's own, so this is the machine, the VM
                    // provider, or a bug in the IDE. No agent to hand it to.
                    self.heading.set_label("Environment failed");
                    self.set_note(
                        &format!(
                            "The safe-mode environment itself could not start: {first}. This is \
                             not the project's configuration — it is this machine's setup, the \
                             VM provider, or a bug in the IDE. The Taste IDE log has the detail."
                        ),
                        false,
                    );
                } else {
                    self.heading.set_label("Falling back to safe mode");
                    self.set_note(
                        &format!(
                            "The project's devcontainer failed: {first}. The safe-mode environment \
                             is coming up instead, with the tools to fix the definition and \
                             rebuild; the agent can be handed the repair."
                        ),
                        true,
                    );
                }
            }
            DevcontainerStateEvent::NoConfig => {
                if self.underway.get() || self.note.is_visible() {
                    self.heading.set_label("Starting in safe mode");
                }
                self.set_note(
                    "This project has no devcontainer definition. The safe-mode environment is \
                     coming up: a generic container with the tools to write one and rebuild into \
                     it; the agent can be asked to author it.",
                    true,
                );
            }
            DevcontainerStateEvent::ConfigDetected | DevcontainerStateEvent::Stopped => {}
        }
    }

    fn set_note(&self, text: &str, prompt: bool) {
        self.note.set_label(text);
        self.note.set_visible(true);
        self.prompt.set_visible(prompt);
    }

    /// The guest image's fetch, unpack, or check: the first step, once per
    /// machine.
    pub fn on_guest_image(self: &Rc<Self>, fetch: &taste_core::GuestImageFetch) {
        use taste_core::GuestImagePhase as P;
        if !fetch.phase.active() {
            if self.row(Step::GuestImage).status.get() == Status::Active {
                self.row(Step::GuestImage)
                    .set(Status::Done, Some("fetched and verified"));
            }
            return;
        }
        let mib = |bytes: u64| bytes / (1024 * 1024);
        let detail = match fetch.phase {
            P::Fetching if fetch.total > 0 => {
                format!("{} of {} MiB", mib(fetch.done), mib(fetch.total))
            }
            P::Fetching => "downloading".to_string(),
            P::Decompressing => "unpacking".to_string(),
            _ => "verifying".to_string(),
        };
        self.activate(Step::GuestImage, Some(&detail));
    }

    /// A line of the environment's build log: appended, and made the
    /// active step's substep — the image build's `STEP n/m: …` as "Step n
    /// of m: …", the container's start as the line itself.
    pub fn on_build_line(&self, line: &str) {
        self.build_log
            .append(std::slice::from_ref(&line.to_string()));
        let clean = crate::ansi::strip_escapes(line);
        let clean = clean.trim();
        if clean.is_empty() {
            return;
        }
        match self.current.get() {
            Some(Step::Build) => {
                let (of, cached) = self.build_steps.get();
                if clean.starts_with("--> Using cache") {
                    self.build_steps.set((of, cached + 1));
                }
                if let Some((n, of, rest)) = build_step_words(clean) {
                    self.build_steps.set((of, self.build_steps.get().1));
                    self.row(Step::Build).set(
                        Status::Active,
                        Some(&format!("Step {n} of {of}: {}", substep_words(rest))),
                    );
                }
            }
            Some(Step::Start) => {
                self.row(Step::Start)
                    .set(Status::Active, Some(&substep_words(clean)));
            }
            _ => {}
        }
    }

    /// A line of the VM's story: appended, and — when it is the IDE's own
    /// step line rather than the guest's console — made the active step's
    /// substep.
    pub fn on_vm_line(&self, line: &str) {
        self.vm_log.append(std::slice::from_ref(&line.to_string()));
        let Some(step) = self.current.get() else {
            return;
        };
        if step.log() != LogKind::Vm {
            return;
        }
        if let Some(said) = line.strip_prefix("[taste-ide] ") {
            let row = self.row(step);
            if row.status.get() == Status::Active {
                row.set(Status::Active, Some(&substep_words(said)));
            }
        }
    }

    /// How far the active step has got — git's progress through the
    /// checkout's seed — as its detail, in place of the last; the log
    /// below is not written, since each of these replaces the one before.
    pub fn on_vm_progress(&self, line: &str) {
        let Some(step) = self.current.get() else {
            return;
        };
        let row = self.row(step);
        if step.log() == LogKind::Vm && row.status.get() == Status::Active {
            row.set(Status::Active, Some(&substep_words(line)));
        }
    }

    /// TASTE_PROBE_CHECK only: pose the page at a stage —
    /// `TASTE_PROBE_STARTUP=vm|place|build|failed|noconfig|ready` — with a few
    /// lines in its log, since a start is a minute of a machine's life and
    /// a shot has none of it.
    #[doc(hidden)]
    pub fn pose_for_probe(self: &Rc<Self>, kind: &str) {
        self.begin();
        for line in [
            "[taste-ide] the domain is already running",
            "[taste-ide] waiting for the guest's sshd on 127.0.0.1:35551 (up to 240s)",
            "[  OK  ] Started sshd.service - OpenSSH server daemon.",
            "[taste-ide] sshd answers; registering the podman connection and waiting for podman in the guest",
            "[taste-ide] podman in the guest answers; the VM is ready",
        ] {
            self.on_vm_line(line);
        }
        use taste_core::StartupStage as S;
        for (stage, words) in [
            (
                S::Sweep,
                "Stopped 2 unused VMs. 12.4 GiB of memory available for IDE VMs; one takes \
                 10.7 GiB.",
            ),
            (
                S::GuestImage,
                "976 MiB, released 2026-08-29 (44.20260829.3.1), verified 2026-09-20",
            ),
            (S::Vm, "Booted in 41 s: 8 vCPUs, 10.7 GiB."),
            (S::ServiceImage, "Already built in this VM."),
            (S::Files, "Connected in 38 ms, over ssh to the VM."),
            (
                S::Place,
                "Already in the VM, on main, in step with the folder.",
            ),
        ] {
            self.on_concluded(stage, words);
        }
        match kind {
            "build" => {
                self.activate(Step::Vm, None);
                self.activate(Step::Files, None);
                self.activate(Step::Place, None);
                self.activate(Step::Build, None);
                for line in [
                    "STEP 1/9: FROM registry.fedoraproject.org/fedora:44",
                    "STEP 2/9: RUN dnf install -y gcc git",
                    "STEP 3/9: RUN useradd -m dev",
                    "STEP 4/9: COPY . /workspaces/taste-ide",
                ] {
                    self.on_build_line(line);
                }
            }
            "failed" => {
                self.activate(Step::Vm, None);
                self.activate(Step::Files, None);
                self.activate(Step::Place, None);
                self.activate(Step::Build, None);
                self.on_build_line("STEP 2/9: RUN dnf install -y gcc gti");
                self.on_build_line("Error: Unable to find a match: gti");
                self.on_state(
                    &DevcontainerStateEvent::Failed {
                        message: "podman build: Error: Unable to find a match: gti".into(),
                    },
                    false,
                );
            }
            "noconfig" => {
                self.activate(Step::Vm, None);
                self.activate(Step::Files, None);
                self.activate(Step::Place, None);
                self.on_state(&DevcontainerStateEvent::NoConfig, false);
                self.activate(Step::Build, Some("the safe-mode environment"));
            }
            "place" => {
                self.activate(Step::Vm, None);
                self.activate(Step::Files, None);
                self.activate(Step::Place, None);
                self.on_vm_line("[taste-ide] seeding the checkout: counting objects, done");
                self.on_vm_progress(
                    "seeding the checkout: sending objects, 45% (1364 of 3031), 40.20 MiB",
                );
            }
            "ready" => {
                self.activate(Step::Vm, None);
                self.activate(Step::Files, None);
                self.activate(Step::Place, None);
                self.activate(Step::Build, None);
                self.activate(Step::Start, None);
                self.finish();
            }
            _ => self.activate(Step::Vm, Some("bringing up the workspace's VM")),
        }
    }
}

/// `STEP 3/9: RUN …`, as podman prints an image build's steps, read as
/// (3, 9); anything else is not a step.
#[cfg(test)]
fn build_step(line: &str) -> Option<(u32, u32)> {
    build_step_words(line).map(|(n, of, _)| (n, of))
}

/// [`build_step`] with the step's own words — what follows the colon.
fn build_step_words(line: &str) -> Option<(u32, u32, &str)> {
    let rest = line.strip_prefix("STEP ")?;
    let (n, after) = rest.split_once('/')?;
    let digits = after
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let words = after[digits..].trim_start_matches(':').trim();
    Some((n.trim().parse().ok()?, after[..digits].parse().ok()?, words))
}

/// A duration as a conclusion says it: `41 s`, `3 min 2 s`.
fn duration_words(took: std::time::Duration) -> String {
    let secs = took.as_secs().max(1);
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// A substep as one short line: the first line, its first letter up,
/// cut with an ellipsis where a log line runs on.
fn substep_words(text: &str) -> String {
    let first = text.lines().next().unwrap_or(text).trim();
    let mut chars = first.chars();
    let mut out: String = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    if out.chars().count() > 96 {
        out = out.chars().take(95).collect::<String>() + "…";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparing_words_name_their_step_and_build_lines_their_number() {
        assert_eq!(
            Step::for_preparing("stopping other projects' VMs that no window owns"),
            Step::Sweep
        );
        assert_eq!(
            Step::for_preparing("building the files service image (once per VM)"),
            Step::ServiceImage
        );
        assert_eq!(
            Step::for_preparing("connecting the files service"),
            Step::Files
        );
        assert_eq!(
            Step::for_preparing("placing the checkout in the VM"),
            Step::Place
        );
        assert_eq!(
            Step::for_preparing("bringing up the workspace's VM"),
            Step::Vm
        );
        assert_eq!(build_step("STEP 3/9: RUN dnf install -y gcc"), Some((3, 9)));
        assert_eq!(build_step("Successfully tagged"), None);
        assert_eq!(
            build_step_words("STEP 3/9: RUN dnf install -y gcc"),
            Some((3, 9, "RUN dnf install -y gcc"))
        );
        assert_eq!(
            substep_words("waiting for the guest's sshd on 127.0.0.1:35551"),
            "Waiting for the guest's sshd on 127.0.0.1:35551"
        );
        assert!(Step::Sweep < Step::Vm && Step::Vm < Step::Build && Step::Build < Step::Ready);
    }
}
