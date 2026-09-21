//! **The substrate: which podman runs an environment's containers.**
//!
//! The IDE's containers used to have exactly one home — rootless podman on
//! the user's own host. This module is the seam that gives them others,
//! and its whole output is a [`taste_core::PodmanTarget`]: a name podman
//! knows. Everything downstream — lifecycle, builds, the environment
//! channel, `ide_exec`, relocation — takes that name and is otherwise
//! unchanged.
//!
//! # The providers, and why they are one abstraction
//!
//! | Provider | Where containers run | How it is reached |
//! | --- | --- | --- |
//! | [`Provider::Vm`] | a VM the IDE provisioned for this workspace | the connection the provisioner registered |
//! | [`Provider::Remote`] | a host with podman on it that the IDE was pointed at | a connection over ssh |
//! | [`Provider::None`] | nowhere: no provisioner supplied a VM | nothing; every start is refused with the reason |
//!
//! A cloud VM is not a fourth kind. A provisioner authenticates to GCP/AWS/
//! Azure, creates a host, registers a connection, and hands back a VM —
//! provisioning reduces to *produce a connection*, and nothing below this
//! module learns a new word. That is the reason the substrate is a
//! connection abstraction and not a `--vm` flag.
//!
//! **There is no host rung.** Until 2026-09-21 the ladder ended on the
//! user's own podman, and a `podman machine` sat between the VM and it.
//! Both are gone (David, 2026-09-17: "I don't want to degrade below VM
//! isolation once this is all running"): a workspace whose provisioner
//! cannot supply a VM has environments that do not start, and each says
//! which provisioner failed and why (`Supervisor::reload` →
//! `SupervisorState::Failed`). [`Provider::Host`] remains for the test
//! suites, which exercise the lifecycle on the host's podman without a
//! hypervisor; the ladder never resolves to it.
//!
//! # A pool per workspace, a substrate per environment
//!
//! A workspace's VMs are a pool (`crate::pool`), and an environment is
//! placed on one of them by capacity (`Pool::place`). So the substrate is
//! a property of an **environment**, not of the workspace: two
//! environments of one workspace may be on two VMs. [`Substrate::resolve`]
//! answers for a workspace — the VM its primary lands on and the first
//! agent environments fill — and the registry keeps one substrate per VM
//! the pool has (`EnvironmentRegistry::substrate_for`).
//!
//! # How the provider is chosen
//!
//! By convention, not configuration (CLAUDE.md → convention over
//! configuration over code). There is no sizing knob, no provider setting,
//! and no per-project substrate:
//!
//! 1. a podman connection named by `TASTE_PODMAN_CONNECTION`, if set — a
//!    host you registered yourself with `podman system connection add`,
//!    adopted rather than provisioned, and the seam a remote provisioner
//!    will terminate at;
//! 2. otherwise a VM from this workspace's pool (`crate::pool`) — one
//!    libvirt has for it, or a new one when it has none and the host has
//!    room. **Provisioning is automatic**: a host with a user-session
//!    libvirt gets a VM per workspace without being asked, because
//!    configuring nothing is choosing the default provisioner;
//! 3. otherwise **nothing**, and the environments say so.
//!
//! # Loud when chosen and lost; a log line when nothing could be asked
//!
//! A VM that exists for this workspace and will not come up, or a
//! connection the user named that does not answer, is a note — toast and
//! app log — because the user did not get what they chose. A host with no
//! libvirt at all is a log line and the same refusal: nothing was chosen,
//! so there is nothing to shout about, but there is also nothing to run
//! on, and every environment's row says which provisioner is missing.
//! [`Descent`] is that distinction, made explicit and testable.

use std::path::Path;
use std::sync::Arc;

use taste_core::PodmanTarget;

use crate::pool::{Pool, PoolError};
use crate::provision::{Vm, VmFacts};

/// Which kind of podman service an environment's containers live on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// A VM the IDE provisioned for this workspace. The domain name is also
    /// the connection name.
    Vm { domain: String },
    /// Any podman service reachable through a registered connection —
    /// a host the IDE was pointed at rather than one it made.
    Remote { connection: String },
    /// No provisioner supplied a VM. Nothing runs; the substrate's note
    /// says why, and every environment refuses to start with it.
    None,
    /// The host's own rootless podman. **Not a rung**: the ladder never
    /// resolves to it. It exists for the test suites, which exercise the
    /// container lifecycle on the host without a hypervisor, and for
    /// nothing else — a production substrate is a VM, a connection, or
    /// none (docs/ENVIRONMENTS.md → "There is no rung below VM isolation").
    #[doc(hidden)]
    Host,
}

impl Provider {
    /// The connection name, or `None` where there is no connection.
    pub fn connection(&self) -> Option<&str> {
        match self {
            Provider::Vm { domain } => Some(domain),
            Provider::Remote { connection } => Some(connection),
            Provider::None | Provider::Host => None,
        }
    }

    /// One phrase, for the environment facts and the log.
    pub fn describe(&self) -> String {
        match self {
            Provider::Vm { domain } => format!("VM {domain} (local libvirt, KVM)"),
            Provider::Remote { connection } => {
                format!("remote podman over connection {connection}")
            }
            Provider::None => "no VM provisioner supplied a VM".into(),
            Provider::Host => "the host's own podman (tests only)".into(),
        }
    }
}

/// The resolved substrate: a provider, the target every podman invocation
/// composes against, and anything worth saying about how it was chosen.
#[derive(Debug, Clone)]
pub struct Substrate {
    provider: Provider,
    target: PodmanTarget,
    /// Why the substrate is what it is, when that is not obvious — a VM
    /// that would not come up, a connection that did not answer. Surfaced,
    /// never swallowed.
    note: Option<String>,
    /// What to record about the descent even when there is nothing to say
    /// out loud — see [`Descent::log_line`].
    log: Option<String>,
    /// The guest's own numbers, when the provider is a VM or a machine.
    /// Held so the environment facts can be honest about what the
    /// substrate costs without asking the hypervisor again on a UI thread.
    vm: Option<VmFacts>,
    /// The VM itself, when the provider is one: what placing a checkout
    /// there needs — its ssh port, its workspace — without asking libvirt
    /// again.
    vm_info: Option<Vm>,
}

/// The environment variable that points the IDE at an already-registered
/// podman connection.
pub const CONNECTION_OVERRIDE_ENV: &str = "TASTE_PODMAN_CONNECTION";

/// How the ladder ended where it did — the sole input to the notice
/// decision, kept apart from the plumbing so the decision can be tested as
/// a pure function.
///
/// **A note is a claim that the user did not get what they chose.** A
/// connection they named that does not answer, a VM that exists for this
/// workspace and will not come up, a VM that could not be made, a host
/// with no room for one: chosen, and lost — loud. A hypervisor that could
/// not be asked at all — libvirt not installed — is a log line: nothing
/// was chosen. Either way the ladder ends on [`Provider::None`] now, and
/// the environments carry the refusal to their rows; what the note decides
/// is whether the WINDOW is interrupted about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descent {
    /// A connection the user named did not answer. Chosen, and lost.
    ChosenConnectionFailed { name: String, error: String },
    /// A VM exists for this workspace — which is what selects it — and
    /// would not come up. Chosen, and lost.
    ProvisionedVmFailed { domain: String, error: String },
    /// The provisioner is there and a VM could not be made. Chosen, and
    /// lost.
    ProvisionFailed { error: String },
    /// The provisioner is there and the host has no room for another VM
    /// beside the ones running. Chosen, and refused — loud, because the
    /// user's other project is why this one has no VM.
    ProvisionerAtCapacity {
        running: usize,
        committed_mib: u64,
        host_mib: u64,
    },
    /// A hypervisor could not be asked about guests at all: libvirt is
    /// absent, or the binary is not there. Nothing was chosen, because
    /// nothing could be — so this is a log line and not a toast, and the
    /// environments' rows carry the reason.
    QueryFailed { what: &'static str, error: String },
    /// Nothing was chosen and nothing was asked: a probe run.
    NothingChosen,
}

impl Descent {
    /// What to say out loud, or nothing.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::ChosenConnectionFailed { name, error } => Some(format!(
                "{CONNECTION_OVERRIDE_ENV}={name} did not answer ({error}); this \
                 workspace's environments cannot start until it does"
            )),
            Self::ProvisionedVmFailed { domain, error } => Some(format!(
                "the VM {domain} exists for this workspace but could not be brought up \
                 ({error}); this workspace's environments cannot start until it is"
            )),
            Self::ProvisionFailed { error } => Some(format!(
                "could not provision a VM for this workspace ({error}); this \
                 workspace's environments cannot start without one"
            )),
            Self::ProvisionerAtCapacity {
                running,
                committed_mib,
                host_mib,
            } => Some(format!(
                "local libvirt is at capacity: {running} VM{} already committing \
                 {:.1} of {:.1} GiB; this workspace's environments cannot start until \
                 one is stopped",
                if *running == 1 { "" } else { "s" },
                *committed_mib as f64 / 1024.0,
                *host_mib as f64 / 1024.0
            )),
            Self::QueryFailed { .. } | Self::NothingChosen => None,
        }
    }

    /// What to record either way. A quiet descent is still a fact worth
    /// having in the app log when somebody comes asking why their
    /// environments will not start.
    pub fn log_line(&self) -> Option<String> {
        match self {
            Self::QueryFailed { what, error } => Some(format!(
                "{what} could not be asked about guests ({error}); no VM was selected, \
                 and this workspace's environments cannot start without one — install \
                 a user-session libvirt, or point the IDE at a provisioner"
            )),
            Self::NothingChosen => None,
            // The loud ones are logged from their note, at warn.
            _ => None,
        }
    }

    /// The reason an environment's row gives for not starting: the note
    /// when there is one, the log line otherwise.
    pub fn reason(&self) -> Option<String> {
        self.note().or_else(|| self.log_line())
    }
}

impl Substrate {
    /// The target a substrate with nothing behind it composes against: a
    /// podman connection that does not exist, so anything that reaches
    /// podman through it — adoption at startup, a stray exec — fails with
    /// podman's own "connection not found" rather than landing on the
    /// host. The refusal every start gets (`Supervisor::reload`) is the
    /// designed path; this is what makes the undesigned ones safe.
    fn unreachable_target(sandboxed: bool) -> PodmanTarget {
        PodmanTarget::connection("taste-ide-no-substrate", sandboxed)
    }

    /// The substrate before the ladder has run: nothing resolved, nothing
    /// said. What the registry holds from construction to reconcile, and
    /// what a window that is not supervising the workspace keeps — its
    /// containers are the supervising window's.
    pub fn unresolved() -> Self {
        Self {
            provider: Provider::None,
            target: Self::unreachable_target(taste_core::podman::sandboxed()),
            note: None,
            log: Some(
                "no substrate has been resolved in this window; the workspace's \
                 containers belong to the window supervising it"
                    .into(),
            ),
            vm: None,
            vm_info: None,
        }
    }

    /// No VM, reached by the given descents — which decide whether
    /// anything is said about it out loud (the first loud one is the note)
    /// and what is recorded (the quiet ones share one log line). Every
    /// environment on it refuses to start with the same reason.
    fn refused_after(sandboxed: bool, descents: &[Descent]) -> Self {
        let logged: Vec<String> = descents.iter().filter_map(Descent::log_line).collect();
        Self {
            provider: Provider::None,
            target: Self::unreachable_target(sandboxed),
            note: descents.iter().find_map(Descent::note),
            log: (!logged.is_empty()).then(|| logged.join("; ")),
            vm: None,
            vm_info: None,
        }
    }

    /// A provisioned VM, brought up, with its facts.
    pub fn vm(vm: &Vm, facts: VmFacts, sandboxed: bool) -> Self {
        Self {
            provider: Provider::Vm {
                domain: vm.domain.clone(),
            },
            target: PodmanTarget::connection(vm.connection(), sandboxed),
            note: None,
            log: None,
            vm: Some(facts),
            vm_info: Some(vm.clone()),
        }
    }

    /// The host's own podman, for the test suites (see [`Provider::Host`]).
    #[doc(hidden)]
    pub fn host_for_tests() -> Arc<Self> {
        Arc::new(Self {
            provider: Provider::Host,
            target: PodmanTarget::local(false),
            note: None,
            log: None,
            vm: None,
            vm_info: None,
        })
    }

    #[doc(hidden)]
    pub fn connection_for_tests(name: &str) -> Arc<Self> {
        Arc::new(Self {
            provider: Provider::Remote {
                connection: name.to_string(),
            },
            target: PodmanTarget::connection(name, false),
            note: None,
            log: None,
            vm: None,
            vm_info: None,
        })
    }

    /// Resolve the substrate for a workspace: the ladder in the module
    /// docs, run for real.
    ///
    /// A rung that was chosen and lost ends the ladder there, saying so as
    /// loudly as [`Descent`] decides; a rung that could not be asked is
    /// recorded and the next is tried. The ladder ends on a VM, a named
    /// connection, or [`Provider::None`] — never on this host.
    pub async fn resolve(workspace_root: &Path) -> Arc<Self> {
        Self::resolve_with(workspace_root, Arc::new(report_download)).await
    }

    /// [`Self::resolve`], telling `report` where the guest image stands as
    /// the ladder brings a VM up — the registry's way of drawing the
    /// download in the window as well as in the log.
    pub async fn resolve_with(
        workspace_root: &Path,
        report: Arc<dyn Fn(taste_core::GuestImageFetch) + Send + Sync>,
    ) -> Arc<Self> {
        let local = PodmanTarget::detect_local();
        let mut quiet: Vec<Descent> = Vec::new();

        // Rung 1: an explicitly named connection. Not checked for
        // existence beyond asking it to answer — a name the user gave is a
        // name they meant, and a typo should say so rather than be quietly
        // replaced by the host.
        if let Ok(name) = std::env::var(CONNECTION_OVERRIDE_ENV) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                let target = PodmanTarget::connection(&name, local.sandboxed());
                return Arc::new(match probe(&target).await {
                    Ok(()) => Self {
                        provider: Provider::Remote { connection: name },
                        target,
                        note: None,
                        log: None,
                        vm: None,
                        vm_info: None,
                    },
                    Err(e) => Self::refused_after(
                        local.sandboxed(),
                        &[Descent::ChosenConnectionFailed {
                            name,
                            error: format!("{e}"),
                        }],
                    ),
                });
            }
        }

        // Rung 2: a VM from this workspace's pool — brought up, or made
        // and brought up. The workspace's first; the registry places
        // environments across the rest (`Pool::place`).
        let pool = Pool::new(workspace_root);
        match pool.ensure_one(report).await {
            Ok((vm, facts)) => return Arc::new(Self::vm(&vm, facts, local.sandboxed())),
            // A probe run: nothing asked, nothing said, nothing run.
            Err(PoolError::Skipped) => quiet.push(Descent::NothingChosen),
            // libvirt is not installed, or its session daemon will not
            // start: nothing was chosen. The IDE's own devcontainer and
            // every host without virtualisation take this branch.
            Err(PoolError::Unavailable(e)) => quiet.push(Descent::QueryFailed {
                what: "libvirt",
                error: format!("{e:#}"),
            }),
            Err(PoolError::AtCapacity {
                running,
                committed_mib,
                host_mib,
            }) => {
                return Arc::new(Self::refused_after(
                    local.sandboxed(),
                    &[Descent::ProvisionerAtCapacity {
                        running,
                        committed_mib,
                        host_mib,
                    }],
                ));
            }
            Err(PoolError::Failed { domain, error }) => {
                let descent = match domain {
                    Some(domain) => Descent::ProvisionedVmFailed {
                        domain,
                        error: format!("{error:#}"),
                    },
                    None => Descent::ProvisionFailed {
                        error: format!("{error:#}"),
                    },
                };
                return Arc::new(Self::refused_after(local.sandboxed(), &[descent]));
            }
        }

        // No rung below: nothing runs, and every environment says why.
        Arc::new(Self::refused_after(local.sandboxed(), &quiet))
    }

    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// The target every podman invocation in the IDE composes against.
    pub fn target(&self) -> &PodmanTarget {
        &self.target
    }

    pub fn connection(&self) -> Option<&str> {
        self.target.connection_name()
    }

    /// Whether the ladder resolved to somewhere containers can run.
    pub fn is_resolved(&self) -> bool {
        !matches!(self.provider, Provider::None)
    }

    /// Why `checkout` cannot run its containers here — the sentence the
    /// environment's row and its reload error carry. Only meaningful when
    /// [`Self::can_host`] is false.
    pub fn refusal(&self, checkout: &taste_core::environment::Checkout) -> String {
        use taste_core::environment::Checkout;
        match (&self.provider, checkout) {
            (Provider::None, _) => self
                .note
                .clone()
                .or_else(|| self.log.clone())
                .unwrap_or_else(|| {
                    "no VM provisioner supplied a VM for this workspace; nothing runs \
                     without one"
                        .into()
                }),
            (Provider::Vm { domain }, Checkout::Remote { vm, .. }) => format!(
                "this environment's checkout is in VM {vm}, and this workspace's pool \
                 has {domain} but not it; the environment is placed anew when the VM is \
                 gone, and cannot run until then"
            ),
            (Provider::Vm { domain }, Checkout::Local(path)) => format!(
                "this environment's checkout is still on this host at {}, and containers \
                 run only in the workspace's VM ({domain}); it is placed there once the \
                 folder is a git repository",
                path.display()
            ),
            (Provider::Remote { connection }, Checkout::Remote { vm, .. }) => format!(
                "this environment's checkout is in VM {vm}, and this workspace runs on the \
                 connection {connection}, which cannot reach it"
            ),
            (Provider::Host, Checkout::Remote { vm, .. }) => {
                format!("this environment's checkout is in VM {vm}, which the host cannot reach")
            }
            (_, _) => format!("{} cannot run this environment", self.provider.describe()),
        }
    }

    /// Why the substrate is what it is, when that needs saying **out
    /// loud** — the user did not get what they chose. See [`Descent`].
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Why the substrate is what it is, when that is worth recording but
    /// not worth interrupting anyone for.
    pub fn log(&self) -> Option<&str> {
        self.log.as_deref()
    }

    pub fn vm_facts(&self) -> Option<&VmFacts> {
        self.vm.as_ref()
    }

    /// The VM this substrate is, when it is one.
    pub fn vm_details(&self) -> Option<&Vm> {
        self.vm_info.as_ref()
    }

    /// Whether a checkout may run its containers here. A checkout in a VM
    /// runs only in that VM, which shares no filesystem with anything; a
    /// checkout on this host runs on a connection the user pointed the IDE
    /// at (whose podman is assumed to see the same files, as a `podman
    /// machine` did) or, in the test suites, on the host. Nothing runs on
    /// a substrate that resolved to nothing.
    pub fn can_host(&self, checkout: &taste_core::environment::Checkout) -> bool {
        use taste_core::environment::Checkout;
        match (&self.provider, checkout) {
            (Provider::None, _) => false,
            (Provider::Vm { domain }, Checkout::Remote { vm, .. }) => domain == vm,
            (Provider::Vm { .. }, Checkout::Local(_)) => false,
            (Provider::Remote { .. } | Provider::Host, Checkout::Remote { .. }) => false,
            (Provider::Remote { .. } | Provider::Host, Checkout::Local(_)) => true,
        }
    }

    /// The substrate as a row in the environment's Resources view.
    ///
    /// `None` for local podman: there is nothing to say that the absence of
    /// a row does not already say. A VM, on the other hand, costs real host
    /// memory that no per-environment number explains — the spike measured
    /// qemu's RSS climbing to the configured ceiling and staying there — so
    /// the fleet's disk-and-memory honesty requires it be shown as its own
    /// line rather than amortised across environments that did not cause
    /// it.
    pub fn resource(&self) -> Option<crate::supervisor::ResourceInfo> {
        use crate::supervisor::{ResourceInfo, ResourceKind};
        match &self.provider {
            Provider::None | Provider::Host => None,
            Provider::Vm { domain } => Some(ResourceInfo {
                kind: ResourceKind::Substrate,
                name: domain.clone(),
                id: domain.clone(),
                status: match &self.vm {
                    Some(facts) => facts.summary(),
                    None => "VM".into(),
                },
            }),
            Provider::Remote { connection } => Some(ResourceInfo {
                kind: ResourceKind::Substrate,
                name: connection.clone(),
                id: connection.clone(),
                status: "remote podman connection".into(),
            }),
        }
    }

    /// A podman command against this substrate, ready to spawn.
    pub fn command(&self, args: &[String]) -> tokio::process::Command {
        let (program, args) = self.target.argv(args.iter().cloned());
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        command
    }

    /// The blocking form, for the two call sites that run before there is a
    /// runtime to await on (startup adoption, the agent-image probe).
    pub fn std_command(&self, args: &[String]) -> std::process::Command {
        let (program, args) = self.target.argv(args.iter().cloned());
        let mut command = std::process::Command::new(program);
        command.args(args);
        command
    }
}

/// The guest image's progress, reported to the app log: each phase as it
/// starts, and the download about every 64 MiB — a gigabyte with no
/// progress line is a hang to whoever is watching.
pub fn report_download(fetch: taste_core::GuestImageFetch) {
    use taste_core::GuestImagePhase;
    const STEP: u64 = 64 * 1024 * 1024;
    let line = match fetch.phase {
        GuestImagePhase::Fetching if fetch.total > 0 => {
            let boundary = fetch.done / STEP != fetch.done.saturating_sub(1024 * 1024) / STEP;
            if !(boundary || fetch.done == fetch.total || fetch.done == 0) {
                return;
            }
            format!(
                "fetching the guest image {}: {} of {} MiB",
                fetch.release,
                fetch.done >> 20,
                fetch.total >> 20
            )
        }
        GuestImagePhase::Fetching | GuestImagePhase::Absent => return,
        GuestImagePhase::Decompressing if fetch.done == 0 => {
            format!("decompressing the guest image {}", fetch.release)
        }
        GuestImagePhase::Decompressing => return,
        GuestImagePhase::Verifying => format!("verifying the guest image {}", fetch.release),
        GuestImagePhase::Ready => format!("the guest image {} is ready", fetch.release),
    };
    taste_core::app_log::push("info", "substrate", &line);
}

/// Can this target answer at all? `podman version` is the cheapest question
/// that proves the whole path — for a connection it opens the ssh session
/// and talks to the far end's service.
pub(crate) async fn probe(target: &PodmanTarget) -> anyhow::Result<()> {
    let (program, args) = target.argv(["version", "--format", "{{.Server.Version}}"]);
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The notice policy, as a pure function over how the ladder descended.
    ///
    /// A chosen substrate that was lost is said out loud, naming what the
    /// user thought they had; a hypervisor that could not be asked is a
    /// log line. Both end on no VM, and both give the environments a
    /// reason.
    #[test]
    fn only_a_lost_choice_earns_a_notice() {
        let connection = Descent::ChosenConnectionFailed {
            name: "workbench".into(),
            error: "connection refused".into(),
        };
        let note = connection.note().expect("a named connection that failed");
        assert!(note.contains("workbench"), "{note}");
        assert!(note.contains(CONNECTION_OVERRIDE_ENV), "{note}");
        assert!(note.contains("cannot start"), "{note}");

        let vm = Descent::ProvisionedVmFailed {
            domain: "taste-799f-k7m2qx".into(),
            error: "did not open ssh".into(),
        };
        let note = vm.note().expect("a VM that would not come up");
        assert!(note.contains("taste-799f-k7m2qx"), "{note}");
        assert!(note.contains("cannot start"), "{note}");

        let failed = Descent::ProvisionFailed {
            error: "qemu-img: no space".into(),
        };
        let note = failed.note().expect("a VM that could not be made");
        assert!(
            note.contains("no space") && note.contains("cannot start"),
            "{note}"
        );

        // Capacity names the other VMs, because they are the reason.
        let full = Descent::ProvisionerAtCapacity {
            running: 2,
            committed_mib: 20 * 1024,
            host_mib: 31 * 1024,
        };
        let note = full.note().expect("a host with no room");
        assert!(note.contains("2 VMs"), "{note}");
        assert!(note.contains("20.0 of 31.0 GiB"), "{note}");
        let one = Descent::ProvisionerAtCapacity {
            running: 1,
            committed_mib: 16 * 1024,
            host_mib: 16 * 1024,
        };
        assert!(
            one.note().unwrap().contains("1 VM already"),
            "{:?}",
            one.note()
        );

        // Never chosen — a log line, not a toast, and still a reason the
        // environments can give.
        let absent = Descent::QueryFailed {
            what: "libvirt",
            error: "running virsh: No such file or directory (os error 2)".into(),
        };
        assert_eq!(absent.note(), None, "nothing was chosen; nothing to shout");
        let logged = absent.log_line().expect("still worth recording");
        assert!(logged.contains("no VM was selected"), "{logged}");
        assert!(logged.contains("libvirt"), "{logged}");
        assert_eq!(absent.reason(), Some(logged));

        assert_eq!(Descent::NothingChosen.note(), None);
        assert_eq!(Descent::NothingChosen.log_line(), None);
        assert_eq!(Descent::NothingChosen.reason(), None);
    }

    /// A ladder that ends without a VM ends on NOTHING: a substrate that
    /// hosts no checkout, composes against a connection that does not
    /// exist, and gives every environment the descent's reason. Two quiet
    /// descents make one log line and no note; one loud one is the note.
    #[test]
    fn a_ladder_without_a_vm_refuses_with_the_reason() {
        use taste_core::environment::Checkout;
        let local_checkout = Checkout::Local("/work/proj".into());
        let quiet = Substrate::refused_after(
            false,
            &[
                Descent::QueryFailed {
                    what: "libvirt",
                    error: "absent".into(),
                },
                Descent::NothingChosen,
            ],
        );
        assert!(!quiet.is_resolved());
        assert_eq!(quiet.provider(), &Provider::None);
        assert_eq!(quiet.note(), None);
        let log = quiet.log().expect("recorded");
        assert!(log.contains("libvirt"), "{log}");
        assert!(!quiet.can_host(&local_checkout));
        assert!(quiet.refusal(&local_checkout).contains("libvirt"));
        assert!(quiet.resource().is_none());
        // Nothing reaches the host through it: the target is a connection
        // podman does not have.
        let (_, args) = quiet.target().argv(["ps"]);
        assert_eq!(
            args[..2],
            ["-c".to_string(), "taste-ide-no-substrate".to_string()]
        );

        let loud = Substrate::refused_after(
            false,
            &[Descent::ProvisionedVmFailed {
                domain: "taste-x".into(),
                error: "boom".into(),
            }],
        );
        assert!(loud.note().is_some_and(|n| n.contains("taste-x")));
        assert_eq!(loud.log(), None);
        assert!(loud.refusal(&local_checkout).contains("taste-x"));

        assert_eq!(Substrate::refused_after(false, &[]).log(), None);
        let unresolved = Substrate::unresolved();
        assert!(!unresolved.is_resolved());
        assert!(unresolved
            .refusal(&local_checkout)
            .contains("resolved in this window"));
    }

    /// Every provider that runs anything reduces to one thing: a name.
    /// This is the property the cloud tier is meant to inherit for free —
    /// a provisioner that returns a connection name needs nothing else.
    #[test]
    fn every_running_provider_is_just_a_connection_name() {
        for provider in [
            Provider::Vm {
                domain: "taste-799f-k7m2qx".into(),
            },
            Provider::Remote {
                connection: "prod-builder".into(),
            },
        ] {
            let name = provider.connection().expect("a name");
            let target = PodmanTarget::connection(name, false);
            let (_, args) = target.argv(["ps"]);
            assert_eq!(args, vec!["-c", name, "ps"]);
            assert!(provider.describe().contains(name));
        }
        assert_eq!(Provider::None.connection(), None);
        assert_eq!(Provider::Host.connection(), None);
    }

    /// The test suites' substrate is the host, composing what the host
    /// always composed, and it is not a rung: nothing about it is a row or
    /// a note.
    #[test]
    fn the_test_substrate_is_the_host_and_adds_nothing() {
        let substrate = Substrate::host_for_tests();
        assert!(substrate.is_resolved());
        assert_eq!(substrate.connection(), None);
        assert!(substrate.note().is_none());
        assert!(substrate.resource().is_none());
        let (program, args) = substrate.target().argv(["ps"]);
        assert_eq!((program.as_str(), args), ("podman", vec!["ps".to_string()]));
    }

    #[tokio::test]
    async fn a_connection_that_cannot_answer_is_reported_not_used() {
        // `podman -c <nonsense>` fails fast; if podman is missing entirely
        // the spawn fails, which is the same answer for this test's
        // purposes: the probe must not report success.
        let target = PodmanTarget::connection("taste-no-such-connection", false);
        assert!(probe(&target).await.is_err());
    }

    /// A VM's row carries what the VM costs the host, because no
    /// per-environment number can: the VM's memory is committed by the VM,
    /// not by the environments inside it. The connection is the domain.
    #[test]
    fn a_provisioned_vm_shows_up_as_its_own_resource_row() {
        let vm = Vm {
            domain: "taste-799f-k7m2qx".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: crate::provision::DomainState::Running,
        };
        let substrate = Substrate::vm(
            &vm,
            VmFacts {
                running: true,
                cpus: 12,
                memory_mib: 10240,
                disk_ceiling_gib: 64,
                host_storage_bytes: Some(3_300_000_000),
            },
            false,
        );
        assert!(substrate.is_resolved());
        assert_eq!(substrate.connection(), Some("taste-799f-k7m2qx"));
        assert_eq!(
            substrate.provider(),
            &Provider::Vm {
                domain: "taste-799f-k7m2qx".into()
            }
        );
        let row = substrate.resource().expect("a VM is worth a row");
        assert_eq!(row.kind, crate::supervisor::ResourceKind::Substrate);
        assert_eq!(row.name, "taste-799f-k7m2qx");
        assert!(row.status.contains("10.0 GiB"), "{}", row.status);
        assert!(row.status.contains("12 vCPU"), "{}", row.status);
        assert_eq!(substrate.vm_details().map(|v| v.ssh_port), Some(40022));
    }

    /// A VM hosts only checkouts that are in it. A checkout on this host
    /// bound into a container in a VM would fail at the bind — there is
    /// no shared filesystem, by decision — so the substrate says no before
    /// podman has to, and says why.
    #[test]
    fn a_vm_hosts_only_the_checkouts_that_are_in_it() {
        use taste_core::environment::Checkout;
        let vm = Vm {
            domain: "taste-a".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: crate::provision::DomainState::Running,
        };
        let on_vm = Substrate::vm(
            &vm,
            VmFacts {
                running: true,
                cpus: 2,
                memory_mib: 4096,
                disk_ceiling_gib: 64,
                host_storage_bytes: None,
            },
            false,
        );
        let local_checkout = Checkout::Local("/work/proj".into());
        let in_this_vm = Checkout::Remote {
            vm: "taste-a".into(),
            path: "/var/home/core/taste/x/i-1".into(),
        };
        let in_another = Checkout::Remote {
            vm: "taste-b".into(),
            path: "/var/home/core/taste/x/i-1".into(),
        };
        assert!(!on_vm.can_host(&local_checkout));
        assert!(on_vm.refusal(&local_checkout).contains("/work/proj"));
        assert!(on_vm.can_host(&in_this_vm));
        assert!(!on_vm.can_host(&in_another));
        assert!(on_vm.refusal(&in_another).contains("taste-b"));
        let host = Substrate::host_for_tests();
        assert!(host.can_host(&local_checkout));
        assert!(!host.can_host(&in_this_vm));
    }
}
