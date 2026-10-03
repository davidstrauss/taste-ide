//! The settings shade's Google Cloud group: the project's own sign-in to
//! GCP, its one-time setup, and a test of what the IDE may do there — all
//! from inside the IDE (David, 2026-10-03: "I do want to be able to run
//! setup and, ideally, set up the restricted service account, and finally
//! impersonate it, directly in the IDE").
//!
//! Three steps, in the order a project needs them:
//!
//! - **Sign in with Google** fetches the IDE's own pinned gcloud if it is
//!   not there yet (`taste_gcp::gcloud`, never the base system) and runs
//!   its browser sign-in in a console tab, into this project's own gcloud
//!   configuration.
//! - **Set up the project** runs `build-aux/gcp-setup.sh` in a console tab
//!   as the user: the APIs, the custom role, the keyless `taste-ide`
//!   service account holding it, and the user's leave to act as it.
//! - **Test** asks Google, *as that service account*, which of the role's
//!   permissions it holds — the impersonation every later call makes.
//!
//! Both console steps run wrapped (`RunInTerminal { wrapped: true }`), in
//! the IDE's own context and never an environment's container: the
//! sign-in is IDE state, and it must not land where an agent runs. The
//! tabs run a command line that strips every inherited sign-in first
//! (`Gcloud::terminal_argv`), since a tab can only add to its environment.
//!
//! The group is the project's rather than any one agent's, so it is in
//! every chat's shade: what it sets up serves the GLM-5.3 route now and
//! the substrate's cloud environments later.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use crate::chat::{Verdict, HEADING_GAP};
use taste_gcp::{gcloud, project, rest, setup};

/// The console tab titles, which the window routes back here when the tab
/// exits (`Event::CommandTabExited`).
pub const SIGN_IN_TITLE: &str = "Google Cloud Sign In";
pub const SETUP_TITLE: &str = "Google Cloud Setup";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    SignIn,
    SetUp,
}

pub struct CloudForm {
    pub group: gtk::Box,
    project: adw::EntryRow,
    sign_in: adw::ButtonRow,
    set_up: adw::ButtonRow,
    test: adw::ButtonRow,
    status: gtk::Box,
    status_dot: gtk::Box,
    status_text: gtk::Label,
    state_dir: PathBuf,
    events: taste_core::EventBus,
    /// Who is signed in to the project's gcloud, as of the last look.
    account: RefCell<Option<String>>,
    /// A step is running; the buttons wait for it.
    busy: Cell<bool>,
    /// A probe has posed the group; nothing real overwrites it.
    posed: Cell<bool>,
}

impl CloudForm {
    pub fn new(state_dir: PathBuf, events: taste_core::EventBus) -> Rc<Self> {
        let heading = gtk::Label::builder()
            .label("Google Cloud")
            .css_classes(["dim-label", "caption-heading"])
            .xalign(0.0)
            .margin_start(8)
            .margin_top(12)
            .margin_bottom(HEADING_GAP)
            .build();
        let scope = gtk::Label::builder()
            .label(
                "This project's own sign-in, for its cloud machines. The IDE runs its own copy \
                 of gcloud and keeps the sign-in in its state for this project, never in the \
                 checkout.",
            )
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(40)
            .xalign(0.0)
            .margin_start(8)
            .margin_bottom(6)
            .build();
        let project = adw::EntryRow::builder().title("Project ID").build();
        let sign_in = adw::ButtonRow::builder()
            .title("Sign in with Google")
            .start_icon_name("avatar-default-symbolic")
            .tooltip_text(
                "Runs gcloud's browser sign-in in a console tab, into this project's own \
                 configuration; fetches the IDE's own gcloud first if it is not there yet",
            )
            .build();
        let set_up = adw::ButtonRow::builder()
            .title("Set up the project")
            .start_icon_name("emblem-system-symbolic")
            .tooltip_text(
                "Runs the setup (build-aux/gcp-setup.sh) in a console tab as you: the APIs, a \
                 role of exactly what the IDE calls, and a keyless service account holding it \
                 that you may act as",
            )
            .build();
        let test = adw::ButtonRow::builder()
            .title("Test")
            .start_icon_name("checkbox-checked-symbolic")
            .tooltip_text(
                "Asks Google, as the IDE's service account, which of the permissions it needs \
                 it holds; creates nothing",
            )
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for row in [
            project.clone().upcast::<gtk::Widget>(),
            sign_in.clone().upcast(),
            set_up.clone().upcast(),
            test.clone().upcast(),
        ] {
            list.append(&row);
        }
        // The verdict line the shade's other groups use: a light in a
        // square slot, the sentence in the wide column beside it.
        let status_dot = gtk::Box::builder().css_classes(["env-dot", "off"]).build();
        let status_slot = crate::filetree::leading_slot(&status_dot);
        status_slot.set_valign(gtk::Align::Center);
        let status_text = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(40)
            .xalign(0.0)
            .hexpand(true)
            .selectable(true)
            .build();
        let status = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(8)
            .margin_top(6)
            .visible(false)
            .build();
        status.append(&status_slot);
        status.append(&status_text);
        let group = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();
        group.append(&heading);
        group.append(&scope);
        group.append(&list);
        group.append(&status);

        let form = Rc::new(Self {
            group,
            project,
            sign_in,
            set_up,
            test,
            status,
            status_dot,
            status_text,
            state_dir,
            events,
            account: RefCell::new(None),
            busy: Cell::new(false),
            posed: Cell::new(false),
        });
        let weak = Rc::downgrade(&form);
        form.project.connect_changed(move |_| {
            if let Some(form) = weak.upgrade() {
                form.sync_sensitivity();
            }
        });
        let weak = Rc::downgrade(&form);
        form.sign_in.connect_activated(move |_| {
            if let Some(form) = weak.upgrade() {
                form.start(Step::SignIn);
            }
        });
        let weak = Rc::downgrade(&form);
        form.set_up.connect_activated(move |_| {
            if let Some(form) = weak.upgrade() {
                form.start(Step::SetUp);
            }
        });
        let weak = Rc::downgrade(&form);
        form.test.connect_activated(move |_| {
            if let Some(form) = weak.upgrade() {
                form.run_test();
            }
        });
        form.sync_sensitivity();
        form
    }

    fn say(&self, verdict: Verdict, text: &str) {
        for class in ["off", "green", "red", "amber"] {
            self.status_dot.remove_css_class(class);
        }
        self.status_dot.add_css_class(verdict.class());
        self.status_text.set_label(text);
        self.status.set_visible(true);
    }

    /// The project row, if it names a project.
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
    /// where the project stands.
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
                    "Not signed in — Sign in with Google opens a console tab",
                ),
                (Some(_), Some(account)) => form.say(
                    Verdict::Attention,
                    &format!(
                        "Signed in as {account} · Test checks what the IDE's service account \
                         may do"
                    ),
                ),
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
                    form.say(Verdict::Fail, &format!("Couldn't start: {error:#}"));
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
    /// is followed by a look at who signed in; a setup by the test, since
    /// whether the setup took is what the test asks.
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
            let tokens =
                rest::TokenSource::gcloud(project::gcloud(&state_dir, binary, &project_for_task));
            let gcp = rest::Gcp::new(Arc::new(tokens), rest::Endpoints::default());
            gcp.test_permissions(&project_for_task, setup::PERMISSIONS)
                .await
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
                Ok(held) => {
                    let missing: Vec<&str> = setup::PERMISSIONS
                        .iter()
                        .copied()
                        .filter(|p| !held.iter().any(|h| h == p))
                        .collect();
                    if missing.is_empty() {
                        form.say(
                            Verdict::Pass,
                            &format!(
                                "Ready · {account} holds all {} permissions the IDE uses in \
                                 {project_id}",
                                setup::PERMISSIONS.len()
                            ),
                        );
                    } else {
                        form.say(Verdict::Fail, &missing_sentence(&missing));
                    }
                }
                Err(error) => form.say(
                    Verdict::Fail,
                    &format!("{error:#} — Set up the project, then test again"),
                ),
            }
        });
    }

    /// `TASTE_PROBE_CLOUD=<variant>`: the group posed in a state a shot
    /// otherwise needs a Google account for. Nothing is read or written.
    pub fn pose_for_probe(&self, variant: &str) {
        self.posed.set(true);
        self.project.set_text("my-project-123");
        let account = "taste-ide@my-project-123.iam.gserviceaccount.com";
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
            "signed-in" => {
                *self.account.borrow_mut() = Some("david@example.com".into());
                self.say(
                    Verdict::Attention,
                    "Signed in as david@example.com · Test checks what the IDE's service \
                     account may do",
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
                    &format!(
                        "Ready · {account} holds all {} permissions the IDE uses in \
                         my-project-123",
                        setup::PERMISSIONS.len()
                    ),
                );
            }
        }
        self.sync_sensitivity();
    }
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
        "Missing {} of {}: {listed} — Set up the project again",
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
                "Missing 2 of {}: a.b.c, d.e.f — Set up the project again",
                setup::PERMISSIONS.len()
            )
        );
        assert!(missing_sentence(&["a", "b", "c", "d", "e"]).contains("a, b, c, and 2 more"));
    }
}
