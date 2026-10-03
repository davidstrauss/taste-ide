//! The project's Google Cloud connection: its own sign-in to GCP, its
//! one-time setup, and a test of what the IDE may do there — all from
//! inside the IDE (David, 2026-10-03: "I do want to be able to run setup
//! and, ideally, set up the restricted service account, and finally
//! impersonate it, directly in the IDE").
//!
//! A section of the title bar's cloud popover, under the folder's sync
//! (`syncstatus.rs`; David, same day: "this GCP connection doesn't belong
//! in the chat settings"), laid out the way a GNOME popover menu is: a dim
//! section header, the project's ID, three actions as flat menu items, and
//! the verdict under them. What it says also lights the cloud's badge
//! ([`CloudLight`]), so a connection that failed is red in the title bar
//! without the popover open.
//!
//! Three steps, in the order a project needs them:
//!
//! - **Sign In with Google…** fetches the IDE's own pinned gcloud if it is
//!   not there yet (`taste_gcp::gcloud`, never the base system) and runs
//!   its browser sign-in in a console tab, into this project's own gcloud
//!   configuration.
//! - **Set Up Project…** runs `build-aux/gcp-setup.sh` in a console tab as
//!   the user: the APIs, the custom role, the keyless `taste-ide` service
//!   account holding it, and the user's leave to act as it.
//! - **Test Connection** asks Google, *as that service account*, which of
//!   the role's permissions it holds — the impersonation every later call
//!   makes — and then whether the project's quotas let a cloud environment
//!   host be created at all (David, 2026-10-03: "I want to know, as a user,
//!   if a quota is too low"), naming each one short with its limit and the
//!   need, and offering **Request More Quota…**, Google's own page for
//!   asking. It also runs on its own at launch for a project that is
//!   signed in, so the badge says how the connection stands rather than
//!   that nobody has asked.
//!
//! Both console steps run wrapped (`RunInTerminal { wrapped: true }`), in
//! the IDE's own context and never an environment's container: the
//! sign-in is IDE state, and it must not land where an agent runs. The
//! tabs run a command line that strips every inherited sign-in first
//! (`Gcloud::terminal_argv`), since a tab can only add to its environment.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;

use crate::chat::Verdict;
use taste_gcp::{gcloud, hosts, model, project, quota, rest, setup};

/// The console tab titles, which the window routes back here when the tab
/// exits (`Event::CommandTabExited`).
pub const SIGN_IN_TITLE: &str = "Google Cloud Sign In";
pub const SETUP_TITLE: &str = "Google Cloud Setup";

/// What the connection contributes to the cloud's badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CloudLight {
    /// Ready, or never set up: nothing for the user to look at.
    Quiet,
    /// Under way, or waiting on the user.
    Waiting,
    /// The connection failed.
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    SignIn,
    SetUp,
}

type LightHook = Rc<dyn Fn(CloudLight, &str)>;

pub struct CloudForm {
    pub widget: gtk::Box,
    scope: gtk::Label,
    actions: gtk::Box,
    project: gtk::Entry,
    sign_in: gtk::Button,
    set_up: gtk::Button,
    test: gtk::Button,
    /// Google's quota page, offered only when a quota is short.
    request_quota: gtk::Button,
    status: gtk::Box,
    status_dot: gtk::Box,
    status_text: gtk::Label,
    state_dir: PathBuf,
    events: taste_core::EventBus,
    /// Who is signed in to the project's gcloud, as of the last look.
    account: RefCell<Option<String>>,
    /// A step is running; the actions wait for it.
    busy: Cell<bool>,
    /// A probe has posed the section; nothing real overwrites it.
    posed: Cell<bool>,
    /// Tests run again on their own while Google applies a new grant
    /// ([`still_applying`]): how many so far.
    retries: Cell<u32>,
    on_light: RefCell<Option<LightHook>>,
}

/// One action, as a popover menu item is: flat, full width, an icon and a
/// label at its start.
fn menu_item(icon: &str, label: &str, tooltip: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::builder().label(label).xalign(0.0).build());
    gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "cloud-action"])
        .tooltip_text(tooltip)
        .build()
}

impl CloudForm {
    pub fn new(state_dir: PathBuf, events: taste_core::EventBus) -> Rc<Self> {
        let heading = gtk::Label::builder()
            .label("Google Cloud")
            .css_classes(["heading", "dim-label"])
            .xalign(0.0)
            .build();
        let scope = gtk::Label::builder()
            .label(
                "This project's own sign-in, for its cloud machines. Kept in the IDE's state \
                 for this project, never in the checkout.",
            )
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .max_width_chars(34)
            .xalign(0.0)
            .build();
        let project = gtk::Entry::builder()
            .placeholder_text("Project ID, such as my-project-123")
            .build();
        let sign_in = menu_item(
            "avatar-default-symbolic",
            "Sign In with Google…",
            "Runs gcloud's browser sign-in in a console tab, into this project's own \
             configuration; fetches the IDE's own gcloud first if it is not there yet",
        );
        let set_up = menu_item(
            "emblem-system-symbolic",
            "Set Up Project…",
            "Runs the setup (build-aux/gcp-setup.sh) in a console tab as you: the APIs, a role \
             of exactly what the IDE calls, and a keyless service account holding it that you \
             may act as",
        );
        let test = menu_item(
            "network-transmit-receive-symbolic",
            "Test Connection",
            "Asks Google, as the IDE's service account, which of the permissions it needs it \
             holds; creates nothing",
        );
        let request_quota = menu_item(
            "web-browser-symbolic",
            "Request More Quota…",
            "Opens Google's quota page for this project in your browser, where an increase \
             is asked for",
        );
        request_quota.set_visible(false);
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 0);
        actions.append(&sign_in);
        actions.append(&set_up);
        actions.append(&test);
        actions.append(&request_quota);

        // The verdict line the IDE's other forms use: a light in a square
        // slot, the sentence in the wide column beside it.
        let status_dot = gtk::Box::builder().css_classes(["env-dot", "off"]).build();
        let status_slot = crate::filetree::leading_slot(&status_dot);
        status_slot.set_valign(gtk::Align::Center);
        let status_text = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(34)
            .xalign(0.0)
            .hexpand(true)
            .selectable(true)
            .build();
        let status = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .visible(false)
            .build();
        status.append(&status_slot);
        status.append(&status_text);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        widget.append(&heading);
        widget.append(&scope);
        widget.append(&project);
        widget.append(&actions);
        let (scope, actions) = (scope.clone(), actions.clone());
        widget.append(&status);

        let form = Rc::new(Self {
            widget,
            scope,
            actions,
            project,
            sign_in,
            set_up,
            test,
            request_quota,
            status,
            status_dot,
            status_text,
            state_dir,
            events,
            account: RefCell::new(None),
            busy: Cell::new(false),
            posed: Cell::new(false),
            retries: Cell::new(0),
            on_light: RefCell::new(None),
        });
        // A popover focuses its first focusable child, and an entry that
        // takes focus selects all of itself, which reads as a field about
        // to be overwritten. The caret goes to the end instead — whenever
        // the field gains focus, since when GTK selects relative to an
        // opening is not something to race.
        form.project.connect_has_focus_notify(|entry| {
            if entry.has_focus() {
                let entry = entry.clone();
                glib::idle_add_local_once(move || {
                    let end = entry.text_length() as i32;
                    entry.select_region(end, end);
                });
            }
        });
        let weak = Rc::downgrade(&form);
        form.project.connect_changed(move |_| {
            if let Some(form) = weak.upgrade() {
                form.sync_sensitivity();
            }
        });
        let weak = Rc::downgrade(&form);
        form.sign_in.connect_clicked(move |_| {
            if let Some(form) = weak.upgrade() {
                form.start(Step::SignIn);
            }
        });
        let weak = Rc::downgrade(&form);
        form.set_up.connect_clicked(move |_| {
            if let Some(form) = weak.upgrade() {
                form.start(Step::SetUp);
            }
        });
        let weak = Rc::downgrade(&form);
        form.test.connect_clicked(move |_| {
            if let Some(form) = weak.upgrade() {
                form.retries.set(0);
                form.run_test();
            }
        });
        let weak = Rc::downgrade(&form);
        form.request_quota.connect_clicked(move |_| {
            let Some(form) = weak.upgrade() else { return };
            let Some(project_id) = form.project_id() else {
                return;
            };
            let window = form.widget.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(&quota::console_url(&project_id)).launch(
                window.as_ref(),
                gtk::gio::Cancellable::NONE,
                |_| {},
            );
        });
        form.sync_sensitivity();
        form
    }

    /// The popover just opened. What keeps its project field from opening
    /// selected is the focus handler in [`Self::new`]; this is the hook for
    /// anything else an opening should do.
    pub fn popped_up(&self) {}

    /// The window is closing: the section shows where the connection
    /// stands — its header and its verdict — and nothing that would start
    /// anything new, which at a close is only noise.
    pub fn freeze(&self) {
        self.busy.set(true);
        self.sync_sensitivity();
        for part in [
            self.scope.upcast_ref::<gtk::Widget>(),
            self.project.upcast_ref(),
            self.actions.upcast_ref(),
        ] {
            part.set_visible(false);
        }
        self.status_text.set_selectable(false);
    }

    /// Who hears what the connection is saying: the cloud's badge.
    pub fn set_on_light(&self, hook: impl Fn(CloudLight, &str) + 'static) {
        *self.on_light.borrow_mut() = Some(Rc::new(hook));
    }

    /// Say one thing under the actions, and tell the badge what it means.
    /// Asking for a project nobody has named is quiet: a project with no
    /// cloud is not one with a problem.
    fn say(&self, verdict: Verdict, text: &str) {
        self.say_more(verdict, text, None);
    }

    /// [`Self::say`], with the whole of a long story in the line's tooltip:
    /// gcloud's errors run to a paragraph, and a popover's status line is
    /// a sentence.
    fn say_more(&self, verdict: Verdict, text: &str, whole: Option<&str>) {
        self.request_quota.set_visible(false);
        self.status_text.set_tooltip_text(whole);
        for class in ["off", "green", "red", "amber"] {
            self.status_dot.remove_css_class(class);
        }
        self.status_dot.add_css_class(verdict.class());
        self.status_text.set_label(text);
        self.status.set_visible(true);
        let light = match verdict {
            Verdict::Pass => CloudLight::Quiet,
            Verdict::Fail => CloudLight::Failed,
            Verdict::Pending => CloudLight::Waiting,
            Verdict::Attention if self.project_id().is_some() => CloudLight::Waiting,
            Verdict::Attention => CloudLight::Quiet,
        };
        let hook = self.on_light.borrow().clone();
        if let Some(hook) = hook {
            hook(light, text);
        }
    }

    /// The project field, if it names a project.
    fn project_id(&self) -> Option<String> {
        let id = self.project.text().trim().to_string();
        setup::valid_project_id(&id).then_some(id)
    }

    /// Sign in needs a project; setting up and testing also need a
    /// sign-in; nothing starts while a step runs.
    fn sync_sensitivity(&self) {
        let idle = !self.busy.get();
        let named = self.project_id().is_some();
        let signed_in = self.account.borrow().is_some();
        self.sign_in.set_sensitive(idle && named);
        self.set_up.set_sensitive(idle && named && signed_in);
        self.test.set_sensitive(idle && named && signed_in);
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.sync_sensitivity();
    }

    /// Read what is on file and who is signed in, off this thread, and say
    /// where the project stands — testing the connection when there is a
    /// sign-in to test, so the badge is an answer rather than a guess.
    pub fn sync(self: &Rc<Self>) {
        if self.posed.get() || self.busy.get() {
            return;
        }
        let state_dir = self.state_dir.clone();
        let look = crate::runtime::runtime().spawn(async move {
            let choices = project::load(&state_dir).ok().flatten();
            let binary = gcloud::binary(&gcloud::sdk_root(), &gcloud::SDK);
            let account = match (&choices, binary.exists()) {
                (Some(choices), true) => project::gcloud(&state_dir, binary, &choices.project)
                    .account()
                    .await
                    .ok()
                    .flatten(),
                _ => None,
            };
            (choices, account)
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((choices, account)) = look.await else {
                return;
            };
            let Some(form) = weak.upgrade() else { return };
            if form.posed.get() || form.busy.get() {
                return;
            }
            if let Some(choices) = &choices {
                if form.project.text().is_empty() {
                    form.project.set_text(&choices.project);
                }
            }
            *form.account.borrow_mut() = account.clone();
            form.sync_sensitivity();
            match (choices, account) {
                (None, _) => form.say(
                    Verdict::Attention,
                    "Not set up — enter the project, sign in with Google, then set it up",
                ),
                (Some(_), None) => form.say(
                    Verdict::Attention,
                    "Not signed in — Sign In with Google opens a console tab",
                ),
                (Some(_), Some(_)) => form.run_test(),
            }
        });
    }

    /// Store the project, fetch gcloud if it is not there, and open the
    /// step's console tab.
    fn start(self: &Rc<Self>, step: Step) {
        let Some(project_id) = self.project_id() else {
            self.say(
                Verdict::Fail,
                "Enter the project's ID, such as my-project-123 — lowercase letters, digits, \
                 and hyphens",
            );
            return;
        };
        self.set_busy(true);
        let (progress_tx, progress_rx) = async_channel::unbounded::<(u64, u64)>();
        let state_dir = self.state_dir.clone();
        let project_for_task = project_id.clone();
        let work = crate::runtime::runtime().spawn(async move {
            project::store(
                &state_dir,
                &project::CloudProject {
                    project: project_for_task.clone(),
                },
            )?;
            let shown = std::sync::atomic::AtomicU64::new(u64::MAX);
            let binary = gcloud::ensure_installed(&gcloud::sdk_root(), move |done, total| {
                let mib = done >> 20;
                if shown.swap(mib, std::sync::atomic::Ordering::Relaxed) != mib {
                    let _ = progress_tx.try_send((done, total));
                }
            })
            .await?;
            Ok::<_, anyhow::Error>(project::gcloud(&state_dir, binary, &project_for_task))
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((done, total)) = progress_rx.recv().await {
                let Some(form) = weak.upgrade() else { return };
                form.say(
                    Verdict::Pending,
                    &format!(
                        "Fetching the IDE's own gcloud {} · {} of {} MiB",
                        gcloud::SDK.version,
                        done >> 20,
                        total.max(1) >> 20
                    ),
                );
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = match work.await {
                Ok(result) => result,
                Err(join) => Err(anyhow::anyhow!("the step did not finish: {join}")),
            };
            let Some(form) = weak.upgrade() else { return };
            let gcloud = match result {
                Ok(gcloud) => gcloud,
                Err(error) => {
                    form.set_busy(false);
                    let whole = format!("{error:#}");
                    form.say_more(
                        Verdict::Fail,
                        &format!("Couldn't start: {}", gcloud::first_sentence(&whole)),
                        Some(&whole),
                    );
                    return;
                }
            };
            let (title, (program, args), sentence) = match step {
                Step::SignIn => (
                    SIGN_IN_TITLE,
                    gcloud.terminal_argv(&["auth", "login", "--brief"], true),
                    "A console tab is running the sign-in — finish it in the browser it opens"
                        .to_string(),
                ),
                Step::SetUp => (
                    SETUP_TITLE,
                    gcloud.setup_terminal_argv(false),
                    format!(
                        "A console tab is setting up {project_id} as {}",
                        form.account.borrow().as_deref().unwrap_or("you")
                    ),
                ),
            };
            form.events.publish(taste_core::Event::RunInTerminal {
                title: title.into(),
                program,
                args,
                env: Vec::new(),
                wrapped: true,
            });
            form.say(Verdict::Pending, &sentence);
        });
    }

    /// A console step's tab exited (`Event::CommandTabExited`). A sign-in
    /// is followed by a look at who signed in, and a setup by the test,
    /// since whether the setup took is what the test asks.
    pub fn step_finished(self: &Rc<Self>, title: &str, status: i32) {
        let step = match title {
            SIGN_IN_TITLE => Step::SignIn,
            SETUP_TITLE => Step::SetUp,
            _ => return,
        };
        self.set_busy(false);
        if status != 0 {
            self.say(
                Verdict::Fail,
                &format!(
                    "The {} stopped with status {status} — its console tab says why",
                    match step {
                        Step::SignIn => "sign-in",
                        Step::SetUp => "setup",
                    }
                ),
            );
            return;
        }
        match step {
            Step::SignIn => self.sync(),
            Step::SetUp => self.run_test(),
        }
    }

    /// Ask Google, as the service account, which of the role's
    /// permissions it holds.
    fn run_test(self: &Rc<Self>) {
        let Some(project_id) = self.project_id() else {
            return;
        };
        let account = setup::service_account(&project_id);
        self.set_busy(true);
        self.say(
            Verdict::Pending,
            &format!("Asking Google, as {account}, which permissions it holds…"),
        );
        let state_dir = self.state_dir.clone();
        let project_for_task = project_id.clone();
        let ask = crate::runtime::runtime().spawn(async move {
            let binary = gcloud::binary(&gcloud::sdk_root(), &gcloud::SDK);
            if !binary.exists() {
                anyhow::bail!("the IDE's gcloud is not fetched yet — sign in first");
            }
            let gcloud = project::gcloud(&state_dir, binary, &project_for_task);
            // Asked as the user first: impersonating an account the setup
            // has not made yet fails in a paragraph about roles, when the
            // answer is one step away.
            if gcloud.service_account_exists().await? == Some(false) {
                return Ok(None);
            }
            let tokens = rest::TokenSource::gcloud(gcloud);
            let gcp = rest::Gcp::new(Arc::new(tokens), rest::Endpoints::default());
            let held = gcp
                .test_permissions(&project_for_task, setup::PERMISSIONS)
                .await?;
            let all_held = setup::PERMISSIONS
                .iter()
                .all(|p| held.iter().any(|h| h == p));
            // The quotas are asked only of a role that may ask them.
            let quotas = if all_held {
                Some(
                    host_quotas(&gcp, &project_for_task)
                        .await
                        .map_err(|e| format!("{e:#}")),
                )
            } else {
                None
            };
            Ok(Some((held, quotas)))
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = match ask.await {
                Ok(result) => result,
                Err(join) => Err(anyhow::anyhow!("the test did not finish: {join}")),
            };
            let Some(form) = weak.upgrade() else { return };
            form.set_busy(false);
            match result {
                Ok(None) => form.say(
                    Verdict::Attention,
                    &format!(
                        "Signed in · Set Up Project creates {}, the account the IDE acts as, \
                         which {project_id} does not have yet",
                        setup::ACCOUNT
                    ),
                ),
                Ok(Some((held, quotas))) => {
                    let missing: Vec<&str> = setup::PERMISSIONS
                        .iter()
                        .copied()
                        .filter(|p| !held.iter().any(|h| h == p))
                        .collect();
                    form.retries.set(0);
                    if !missing.is_empty() {
                        form.say(Verdict::Fail, &missing_sentence(&missing));
                        return;
                    }
                    match quotas {
                        Some(Ok(machines)) => form.say_quotas(&project_id, &machines),
                        Some(Err(why)) => form.say_more(
                            Verdict::Attention,
                            "Ready, but the quotas could not be read — Set Up Project again, \
                             which turns on the API they are read through",
                            Some(&why),
                        ),
                        None => form.say(Verdict::Pass, &ready_sentence(&account, &project_id)),
                    }
                }
                Err(error) => {
                    let whole = format!("{error:#}");
                    if still_applying(&whole) && form.retries.get() < APPLY_RETRIES {
                        // The setup just granted the user leave to act as
                        // the account, and Google takes minutes to apply a
                        // grant: what reads as "denied" is "not yet".
                        form.retries.set(form.retries.get() + 1);
                        form.say_more(
                            Verdict::Attention,
                            &format!(
                                "Google is still applying your permission to act as {} — \
                                 grants take up to seven minutes. Testing again in {}s.",
                                setup::ACCOUNT,
                                APPLY_RETRY_EVERY.as_secs()
                            ),
                            Some(&whole),
                        );
                        let weak = Rc::downgrade(&form);
                        glib::timeout_add_local_once(APPLY_RETRY_EVERY, move || {
                            if let Some(form) = weak.upgrade() {
                                if !form.busy.get() {
                                    form.run_test();
                                }
                            }
                        });
                        return;
                    }
                    let advice = if still_applying(&whole) {
                        format!(
                            "You still may not act as {} after seven minutes. Set Up Project \
                             grants it — run it again, signed in as the account that should \
                             use the IDE.",
                            setup::ACCOUNT
                        )
                    } else {
                        format!(
                            "{} Set Up Project, then test again",
                            gcloud::first_sentence(&whole)
                        )
                    };
                    form.say_more(Verdict::Fail, &advice, Some(&whole));
                }
            }
        });
    }

    /// The test's last word when the permissions are all held: ready while
    /// any host machine fits the quotas — the provisioner tries them in
    /// turn — or ready but with quotas too low for every one of them, the
    /// first's shortfalls named in the line, every machine's in its
    /// tooltip, and Google's page for asking one click away.
    fn say_quotas(&self, project_id: &str, machines: &[(String, Vec<quota::Shortfall>)]) {
        let Some((first, short)) = machines.first() else {
            self.say(Verdict::Pass, &ready_sentence("", project_id));
            return;
        };
        if machines.iter().any(|(_, short)| short.is_empty()) {
            self.say(Verdict::Pass, &ready_sentence("", project_id));
            return;
        }
        let whole: Vec<String> = machines
            .iter()
            .map(|(machine, short)| {
                if short.is_empty() {
                    format!("{machine} fits the quotas.")
                } else {
                    format!("{}.", quota::sentence(machine, short))
                }
            })
            .chain(std::iter::once(format!(
                "Ask for more at {}",
                quota::console_url(project_id)
            )))
            .collect();
        self.say_more(
            Verdict::Attention,
            &quota_sentence(first, short),
            Some(&whole.join("\n")),
        );
        self.request_quota.set_visible(true);
    }

    /// `TASTE_PROBE_CLOUD=<variant>`: the section posed in a state a shot
    /// otherwise needs a Google account for. Nothing is read or written.
    pub fn pose_for_probe(&self, variant: &str) {
        self.posed.set(true);
        self.project.set_text("my-project-123");
        match variant {
            "unset" => {
                self.project.set_text("");
                self.say(
                    Verdict::Attention,
                    "Not set up — enter the project, sign in with Google, then set it up",
                );
            }
            "fetching" => {
                self.busy.set(true);
                self.say(
                    Verdict::Pending,
                    &format!(
                        "Fetching the IDE's own gcloud {} · 41 of 83 MiB",
                        gcloud::SDK.version
                    ),
                );
            }
            "no-account" => {
                *self.account.borrow_mut() = Some("david@example.com".into());
                self.say(
                    Verdict::Attention,
                    &format!(
                        "Signed in · Set Up Project creates {}, the account the IDE acts as, \
                         which my-project-123 does not have yet",
                        setup::ACCOUNT
                    ),
                );
            }
            "quota" => {
                *self.account.borrow_mut() = Some("david@example.com".into());
                let short = |label: &str, limit, amount| quota::Shortfall {
                    need: quota::Need {
                        quota_id: String::new(),
                        dimensions: Vec::new(),
                        amount,
                        label: label.into(),
                    },
                    limit,
                };
                self.say_quotas(
                    "my-project-123",
                    &[
                        (
                            "n4-standard-8".into(),
                            vec![
                                short("vCPUs in all regions", 4, 8),
                                short("N4 vCPUs in us-central1", 0, 8),
                            ],
                        ),
                        (
                            "c4-standard-8".into(),
                            vec![
                                short("vCPUs in all regions", 4, 8),
                                short("C4 vCPUs in us-central1", 0, 8),
                            ],
                        ),
                    ],
                );
            }
            "missing" => {
                *self.account.borrow_mut() = Some("david@example.com".into());
                self.say(
                    Verdict::Fail,
                    &missing_sentence(&[
                        "compute.instances.getGuestAttributes",
                        "dns.policies.create",
                        "iap.tunnelInstances.accessViaIAP",
                    ]),
                );
            }
            _ => {
                *self.account.borrow_mut() = Some("david@example.com".into());
                self.say(
                    Verdict::Pass,
                    &ready_sentence(&setup::service_account("my-project-123"), "my-project-123"),
                );
            }
        }
        self.sync_sensitivity();
    }
}

/// The account by its id, `taste-ide`: its full address wraps a narrow
/// popover mid-word, and what it is called says which account it is.
/// How often, and how many times, a test denied impersonation runs again
/// on its own: Google applies an IAM grant within seven minutes.
const APPLY_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
const APPLY_RETRIES: u32 = 21;

/// Whether a failure is the user being refused leave to act as the service
/// account — what a grant Google has not applied yet looks like.
fn still_applying(said: &str) -> bool {
    said.contains("iam.serviceAccounts.getAccessToken") && said.contains("PERMISSION_DENIED")
}

/// The machines a cloud environment host is made on, first choice first
/// (`taste_gcp::hosts`), each with the quotas too low for it in the
/// region hosts go to.
async fn host_quotas(
    gcp: &rest::Gcp,
    project_id: &str,
) -> anyhow::Result<Vec<(String, Vec<quota::Shortfall>)>> {
    let mut machines = Vec::new();
    let candidates = std::iter::once(hosts::HOST_MACHINE).chain(
        hosts::HOST_FALLBACKS
            .iter()
            .copied()
            .filter(|m| *m != hosts::HOST_MACHINE),
    );
    for machine in candidates {
        let short = quota::shortfalls(gcp, project_id, model::DEFAULT_REGION, machine).await?;
        machines.push((machine.to_string(), short));
    }
    Ok(machines)
}

/// The line for quotas too low: the machine, and each quota as its limit
/// against the need, short enough for a popover. The machine's hyphens
/// are non-breaking, since the line wraps by character and would
/// otherwise split `c4-highmem-192` at one.
fn quota_sentence(machine: &str, short: &[quota::Shortfall]) -> String {
    let listed: Vec<String> = short
        .iter()
        .map(|s| format!("{}: {}, needs {}", s.need.label, s.limit, s.need.amount))
        .collect();
    format!(
        "Ready, but quotas are too low for a cloud environment host on {}. {}",
        machine.replace('-', "\u{2011}"),
        listed.join("; ")
    )
}

#[cfg(test)]
mod quota_tests {
    use super::*;

    #[test]
    fn a_short_quota_says_its_limit_and_the_need() {
        let short = quota::Shortfall {
            need: quota::Need {
                quota_id: "CPUS-ALL-REGIONS-per-project".into(),
                dimensions: Vec::new(),
                amount: 8,
                label: "vCPUs in all regions".into(),
            },
            limit: 4,
        };
        let said = quota_sentence("n4-standard-8", &[short]);
        assert_eq!(
            said,
            "Ready, but quotas are too low for a cloud environment host on \
             n4\u{2011}standard\u{2011}8. vCPUs in all regions: 4, needs 8"
        );
    }
}

fn ready_sentence(_account: &str, project_id: &str) -> String {
    format!(
        "Ready · {} holds all {} permissions the IDE uses in {project_id}",
        setup::ACCOUNT,
        setup::PERMISSIONS.len()
    )
}

/// What a test that found permissions missing says: how many, the first
/// few by name, and what to do.
fn missing_sentence(missing: &[&str]) -> String {
    let shown: Vec<&str> = missing.iter().copied().take(3).collect();
    let more = missing.len().saturating_sub(shown.len());
    let listed = match more {
        0 => shown.join(", "),
        _ => format!("{}, and {more} more", shown.join(", ")),
    };
    format!(
        "Missing {} of {}: {listed} — Set Up Project again",
        missing.len(),
        setup::PERMISSIONS.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_list_is_named_whole_and_a_long_one_counted() {
        assert_eq!(
            missing_sentence(&["a.b.c", "d.e.f"]),
            format!(
                "Missing 2 of {}: a.b.c, d.e.f — Set Up Project again",
                setup::PERMISSIONS.len()
            )
        );
        assert!(missing_sentence(&["a", "b", "c", "d", "e"]).contains("a, b, c, and 2 more"));
    }

    #[test]
    fn a_grant_not_yet_applied_is_told_apart() {
        assert!(still_applying(
            "ERROR: (gcloud.auth.print-access-token) PERMISSION_DENIED: Failed to impersonate \
             [taste-ide@taste-ide.iam.gserviceaccount.com]. Permission \
             'iam.serviceAccounts.getAccessToken' denied on resource (or it may not exist)."
        ));
        assert!(!still_applying("PERMISSION_DENIED: compute.instances.list"));
        assert_eq!(
            APPLY_RETRY_EVERY * APPLY_RETRIES,
            std::time::Duration::from_secs(420)
        );
    }

    #[test]
    fn cloud_guests_boot_the_pools_release() {
        assert_eq!(
            taste_gcp::guest::FCOS_RELEASE,
            taste_devcontainer::guest::RELEASE
        );
    }

    #[test]
    fn the_worst_light_wins() {
        assert!(CloudLight::Failed > CloudLight::Waiting);
        assert!(CloudLight::Waiting > CloudLight::Quiet);
    }
}
