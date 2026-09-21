//! **The substrate: which podman runs an environment's containers.**
//!
//! The IDE's containers used to have exactly one home — rootless podman on
//! the user's own host. This module is the seam that lets them have others,
//! and its whole output is a [`taste_core::PodmanTarget`]: a name podman
//! knows. Everything downstream — lifecycle, builds, the environment
//! channel, `ide_exec`, relocation — takes that name and is otherwise
//! unchanged.
//!
//! # The tiers, and why they are one abstraction
//!
//! | Provider | Where containers run | How it is reached |
//! | --- | --- | --- |
//! | [`Provider::Local`] | the user's host | the local rootless service |
//! | [`Provider::Vm`] | a VM the IDE provisioned for this workspace | the connection the provisioner registered |
//! | [`Provider::Machine`] | a `podman machine` (retired once every environment is on a VM) | the connection `podman machine` registered |
//! | [`Provider::Remote`] | any host with podman on it | a connection over ssh |
//!
//! A cloud VM is not a fifth kind. A provisioner authenticates to GCP/AWS/
//! Azure, creates a host, registers a connection, and hands back a VM —
//! provisioning reduces to *produce a connection*, and nothing below this
//! module learns a new word. That is the reason the substrate is a
//! connection abstraction and not a `--vm` flag.
//!
//! # A pool per workspace, a substrate per environment
//!
//! A workspace's VMs are a pool (`crate::provision`), and an environment
//! is placed on one of them by capacity. So the substrate is a property of
//! an **environment**, not of the workspace: two environments of one
//! workspace may be on two VMs. [`Substrate::resolve`] answers for a
//! workspace — the VM its first environments land on — and placement
//! (`Pool::place`, in the batch that moves checkouts into VMs) answers for
//! the rest.
//!
//! **What the remote tier still waits on is clone locality**, and it is not
//! a detail. Every environment's checkout is a host path bound into its
//! container, and a VM shares no filesystem with the host by decision
//! (docs/ENVIRONMENTS.md → "The topology"). Until a checkout can live where
//! the containers are, an environment routed to a VM fails at its bind.
//! That is why a provisioned VM is *adopted* here — brought up, shown in
//! the Resources view, its cost made visible — while containers keep
//! running locally until the checkouts move.
//!
//! # How the provider is chosen
//!
//! By convention, not configuration (CLAUDE.md → convention over
//! configuration over code). There is no sizing knob, no provider setting,
//! and no per-project substrate:
//!
//! 1. a podman connection named by `TASTE_PODMAN_CONNECTION`, if set — the
//!    alpha seam for pointing the IDE at a host you already registered with
//!    `podman system connection add`;
//! 2. otherwise a VM from this workspace's pool (`crate::pool`) — the first
//!    one libvirt has for it, or a new one when it has none and the host
//!    has room. **Provisioning is automatic**: a host with a user-session
//!    libvirt gets a VM per workspace without being asked, because
//!    configuring nothing is choosing the default provisioner;
//! 3. otherwise the machine named [`crate::machine::MACHINE_NAME`], if one
//!    exists;
//! 4. otherwise the local service, which is what every installation has and
//!    what every installation had before this module existed.
//!
//! # Never degrade silently — and never shout about not degrading
//!
//! A VM that exists for this workspace and will not come up, or a machine
//! that exists and will not start, falls back to local with a note that
//! says so out loud. **A rung that could never have been taken is not a
//! degradation.** Asking libvirt or podman about guests fails outright
//! wherever those subsystems are not installed — the IDE's own
//! devcontainer, a probe run, any host that never wanted a VM — and
//! reporting that made the normal case shout on every launch about
//! infrastructure nobody asked for. [`Descent`] is that distinction, made
//! explicit and testable: a note means *you did not get what you chose*,
//! and nothing else earns one.

use std::path::Path;
use std::sync::Arc;

use taste_core::PodmanTarget;

use crate::machine::{self, Machine};
use crate::pool::{Pool, PoolError};
use crate::provision::{Vm, VmFacts};

/// Which kind of podman service an environment's containers live on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Rootless podman on the user's own host.
    Local,
    /// A VM the IDE provisioned for this workspace. The domain name is also
    /// the connection name.
    Vm { domain: String },
    /// A `podman machine` — a local VM. The name is the machine's, which is
    /// also the connection's.
    Machine { name: String },
    /// Any podman service reachable through a registered connection —
    /// a host the IDE was pointed at rather than one it made.
    Remote { connection: String },
}

impl Provider {
    /// The connection name, or `None` for the local service.
    pub fn connection(&self) -> Option<&str> {
        match self {
            Provider::Local => None,
            Provider::Vm { domain } => Some(domain),
            Provider::Machine { name } => Some(name),
            Provider::Remote { connection } => Some(connection),
        }
    }

    /// One phrase, for the environment facts and the log.
    pub fn describe(&self) -> String {
        match self {
            Provider::Local => "local rootless podman".into(),
            Provider::Vm { domain } => format!("VM {domain} (local libvirt, KVM)"),
            Provider::Machine { name } => format!("podman machine {name} (local VM, KVM)"),
            Provider::Remote { connection } => {
                format!("remote podman over connection {connection}")
            }
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
/// **A note is a claim that the user did not get what they chose.** That is
/// the whole rule, and it is worth stating because the obvious
/// implementation gets it backwards: it reports every rung that failed,
/// which means the ordinary host — no VM, no machine, neither libvirt nor
/// podman's machine subsystem installed — shouts on every launch about a
/// VM nobody asked for. A ladder that was always going to end on local
/// podman ending on local podman is not a degradation, it is the design.
///
/// The hard half is untouched: a VM the user believes they have and do not
/// is the one substrate failure that must never be quiet.
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
    /// user's other project is why this one is not behind a VM.
    ProvisionerAtCapacity {
        running: usize,
        committed_mib: u64,
        host_mib: u64,
    },
    /// A machine exists — which is what selects it — and would not start.
    /// Chosen, and lost.
    ChosenMachineFailed { name: String, error: String },
    /// A hypervisor could not be asked about guests at all: libvirt or
    /// podman's machine subsystem is absent, or the binary is not there.
    /// Nothing was chosen, because nothing could be — so this is a log
    /// line and not a toast.
    QueryFailed { what: &'static str, error: String },
    /// Nothing was chosen and local is where the ladder always ended. The
    /// overwhelmingly common case, and silent.
    NothingChosen,
}

impl Descent {
    /// What to say out loud, or nothing.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::ChosenConnectionFailed { name, error } => Some(format!(
                "{CONNECTION_OVERRIDE_ENV}={name} did not answer ({error}); \
                 running on local podman instead"
            )),
            Self::ProvisionedVmFailed { domain, error } => Some(format!(
                "the VM {domain} exists for this workspace but could not be brought up \
                 ({error}); this workspace's containers are running on local podman, \
                 NOT behind a VM"
            )),
            Self::ProvisionFailed { error } => Some(format!(
                "could not provision a VM for this workspace ({error}); this \
                 workspace's containers are running on local podman, NOT behind a VM"
            )),
            Self::ProvisionerAtCapacity {
                running,
                committed_mib,
                host_mib,
            } => Some(format!(
                "local libvirt is at capacity: {running} VM{} already committing \
                 {:.1} of {:.1} GiB; this workspace's containers are running on \
                 local podman, NOT behind a VM",
                if *running == 1 { "" } else { "s" },
                *committed_mib as f64 / 1024.0,
                *host_mib as f64 / 1024.0
            )),
            Self::ChosenMachineFailed { name, error } => Some(format!(
                "the podman machine {name} exists but would not start ({error}); \
                 this workspace's containers are running on local podman, \
                 NOT behind a VM"
            )),
            Self::QueryFailed { .. } | Self::NothingChosen => None,
        }
    }

    /// What to record either way. A quiet descent is still a fact worth
    /// having in the app log when somebody comes asking why their VM is
    /// not being used.
    pub fn log_line(&self) -> Option<String> {
        match self {
            Self::QueryFailed { what, error } => Some(format!(
                "{what} could not be asked about guests ({error}); none was selected, \
                 so this workspace's containers run on local podman — which is where \
                 they were going anyway"
            )),
            Self::NothingChosen => None,
            // The loud ones are logged from their note, at warn.
            _ => None,
        }
    }
}

impl Substrate {
    /// The local service — the default, and what a test wants.
    pub fn local() -> Self {
        Self::local_with_note(PodmanTarget::detect_local(), None)
    }

    fn local_with_note(target: PodmanTarget, note: Option<String>) -> Self {
        Self {
            provider: Provider::Local,
            target: target.with_connection(None),
            note,
            log: None,
            vm: None,
            vm_info: None,
        }
    }

    /// Local podman, reached by the given descents — which decide whether
    /// anything is said about it and how loudly. Several quiet descents
    /// (libvirt absent, podman's machine subsystem absent) share one log
    /// line; the first loud one is the note.
    fn local_after(target: PodmanTarget, descents: &[Descent]) -> Self {
        let logged: Vec<String> = descents.iter().filter_map(Descent::log_line).collect();
        Self {
            provider: Provider::Local,
            target: target.with_connection(None),
            note: descents.iter().find_map(Descent::note),
            log: (!logged.is_empty()).then(|| logged.join("; ")),
            vm: None,
            vm_info: None,
        }
    }

    /// The host's own podman, reached the way `other` reaches its podman
    /// binary — through the sandbox or not. What a checkout on this host
    /// runs on when the workspace's substrate is a VM it cannot be in.
    pub fn local_like(other: &Substrate) -> Self {
        Self::local_with_note(PodmanTarget::local(other.target().sandboxed()), None)
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

    #[doc(hidden)]
    pub fn local_for_tests() -> Arc<Self> {
        Arc::new(Self::local_with_note(PodmanTarget::local(false), None))
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
    /// Every rung that fails falls to the next one, saying so as loudly as
    /// [`Descent`] decides, and the bottom rung — local podman — is the one
    /// that cannot fail, because it is what the IDE did before any of this
    /// existed.
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
                    Err(e) => Self::local_after(
                        local,
                        &[Descent::ChosenConnectionFailed {
                            name,
                            error: format!("{e}"),
                        }],
                    ),
                });
            }
        }

        // Rung 2: a VM from this workspace's pool — brought up, or made
        // and brought up. A pool with several answers with the first by
        // name here, and per environment once placement lands.
        let pool = Pool::new(workspace_root);
        match pool.ensure_one(report).await {
            Ok((vm, facts)) => return Arc::new(Self::vm(&vm, facts, local.sandboxed())),
            // A probe run: nothing asked, nothing said.
            Err(PoolError::Skipped) => {}
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
                return Arc::new(Self::local_after(
                    local,
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
                return Arc::new(Self::local_after(local, &[descent]));
            }
        }

        // Rung 3: the IDE's own machine, if the user has created one.
        // Absent is the common case and is not a fault to report.
        let machine = Machine::default_machine(local.clone());
        match machine.state().await {
            Ok(machine::State::Absent) => {}
            Ok(_) => {
                return Arc::new(match machine.ensure_running().await {
                    Ok(facts) => Self {
                        provider: Provider::Machine {
                            name: machine::MACHINE_NAME.to_string(),
                        },
                        target: PodmanTarget::connection(machine::MACHINE_NAME, local.sandboxed()),
                        note: None,
                        log: None,
                        vm: Some(facts),
                        vm_info: None,
                    },
                    Err(e) => Self::local_after(
                        local,
                        &[Descent::ChosenMachineFailed {
                            name: machine::MACHINE_NAME.to_string(),
                            error: format!("{e:#}"),
                        }],
                    ),
                });
            }
            Err(e) => quiet.push(Descent::QueryFailed {
                what: "podman machine",
                error: format!("{e:#}"),
            }),
        }

        Arc::new(Self::local_after(local, &quiet))
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

    pub fn is_local(&self) -> bool {
        matches!(self.provider, Provider::Local)
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

    /// Whether a checkout may run its containers here. A checkout on this
    /// host can be bound into a container on this host, a machine (which
    /// shares the home directory), or a connection the user pointed the IDE
    /// at; it cannot be bound into a container in a VM, which shares no
    /// filesystem with anything. A checkout in a VM runs only in that VM.
    pub fn can_host(&self, checkout: &taste_core::environment::Checkout) -> bool {
        match (&self.provider, checkout) {
            (Provider::Vm { domain }, taste_core::environment::Checkout::Remote { vm, .. }) => {
                domain == vm
            }
            (Provider::Vm { .. }, taste_core::environment::Checkout::Local(_)) => false,
            (_, taste_core::environment::Checkout::Remote { .. }) => false,
            (_, taste_core::environment::Checkout::Local(_)) => true,
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
            Provider::Local => None,
            Provider::Vm { domain } => Some(ResourceInfo {
                kind: ResourceKind::Substrate,
                name: domain.clone(),
                id: domain.clone(),
                status: match &self.vm {
                    Some(facts) => facts.summary(),
                    None => "VM".into(),
                },
            }),
            Provider::Machine { name } => Some(ResourceInfo {
                kind: ResourceKind::Substrate,
                name: name.clone(),
                id: self.connection().unwrap_or_default().to_string(),
                status: match &self.vm {
                    Some(facts) => facts.summary(),
                    None => "machine".into(),
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
    /// Both halves matter and they pull in opposite directions, which is why
    /// they are pinned together: a chosen substrate that was lost must
    /// always be said out loud, and a substrate nobody chose must never be.
    #[test]
    fn only_a_lost_choice_earns_a_notice() {
        // Chosen and lost — loud, all of them, and each names the thing
        // the user thought they had.
        let connection = Descent::ChosenConnectionFailed {
            name: "workbench".into(),
            error: "connection refused".into(),
        };
        let note = connection.note().expect("a named connection that failed");
        assert!(note.contains("workbench"), "{note}");
        assert!(note.contains(CONNECTION_OVERRIDE_ENV), "{note}");

        let vm = Descent::ProvisionedVmFailed {
            domain: "taste-799f-k7m2qx".into(),
            error: "did not open ssh".into(),
        };
        let note = vm.note().expect("a VM that would not come up");
        assert!(note.contains("taste-799f-k7m2qx"), "{note}");
        assert!(
            note.contains("NOT behind a VM"),
            "a VM the user believes they have and do not: {note}"
        );

        let machine = Descent::ChosenMachineFailed {
            name: "taste-ide".into(),
            error: "no gvproxy".into(),
        };
        let note = machine.note().expect("a machine that would not start");
        assert!(note.contains("taste-ide"), "{note}");
        assert!(note.contains("NOT behind a VM"), "{note}");

        let failed = Descent::ProvisionFailed {
            error: "qemu-img: no space".into(),
        };
        let note = failed.note().expect("a VM that could not be made");
        assert!(
            note.contains("no space") && note.contains("NOT behind a VM"),
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
        assert!(note.contains("NOT behind a VM"), "{note}");
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

        // Never chosen — silent. This is the branch that used to toast on
        // every launch of the IDE's own devcontainer, where neither libvirt
        // nor podman's machine subsystem is installed.
        for absent in [
            Descent::QueryFailed {
                what: "libvirt",
                error: "running virsh: No such file or directory (os error 2)".into(),
            },
            Descent::QueryFailed {
                what: "podman machine",
                error: "running podman machine: No such file or directory (os error 2)".into(),
            },
        ] {
            assert_eq!(absent.note(), None, "the normal local case must not shout");
            let logged = absent.log_line().expect("still worth recording");
            assert!(logged.contains("none was selected"), "{logged}");
        }

        assert_eq!(Descent::NothingChosen.note(), None);
        assert_eq!(Descent::NothingChosen.log_line(), None);
    }

    /// Two quiet descents make one log line and no note; one loud one among
    /// them is the note.
    #[test]
    fn descents_compose_into_one_log_line_and_at_most_one_note() {
        let quiet = Substrate::local_after(
            PodmanTarget::local(false),
            &[
                Descent::QueryFailed {
                    what: "libvirt",
                    error: "absent".into(),
                },
                Descent::QueryFailed {
                    what: "podman machine",
                    error: "absent".into(),
                },
            ],
        );
        assert!(quiet.is_local());
        assert_eq!(quiet.note(), None);
        let log = quiet.log().expect("recorded");
        assert!(
            log.contains("libvirt") && log.contains("podman machine"),
            "{log}"
        );

        let loud = Substrate::local_after(
            PodmanTarget::local(false),
            &[Descent::ProvisionedVmFailed {
                domain: "taste-x".into(),
                error: "boom".into(),
            }],
        );
        assert!(loud.note().is_some_and(|n| n.contains("taste-x")));
        assert_eq!(loud.log(), None);

        assert_eq!(
            Substrate::local_after(PodmanTarget::local(false), &[]).log(),
            None
        );
    }

    /// The default is the host, and the host composes exactly what it
    /// always composed. Every installation that never asks for a VM must
    /// see no change at all.
    #[test]
    fn the_default_substrate_is_the_host_and_adds_nothing() {
        let substrate = Substrate::local_for_tests();
        assert!(substrate.is_local());
        assert_eq!(substrate.connection(), None);
        assert!(substrate.note().is_none());
        assert!(
            substrate.resource().is_none(),
            "there is no substrate row when the substrate is the host itself"
        );
        let (program, args) = substrate.target().argv(["ps"]);
        assert_eq!((program.as_str(), args), ("podman", vec!["ps".to_string()]));
    }

    /// Every provider that is not the host reduces to one thing: a name.
    /// This is the property the cloud tier is meant to inherit for free —
    /// a provisioner that returns a connection name needs nothing else.
    #[test]
    fn every_non_local_provider_is_just_a_connection_name() {
        for provider in [
            Provider::Vm {
                domain: "taste-799f-k7m2qx".into(),
            },
            Provider::Machine {
                name: "taste-ide".into(),
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
        assert_eq!(Provider::Local.connection(), None);
    }

    /// A substrate that could not be reached must SAY so and run locally —
    /// never claim a VM it does not have.
    #[test]
    fn a_failed_substrate_falls_back_loudly() {
        let fallen = Substrate::local_with_note(
            PodmanTarget::local(false),
            Some("the VM taste-x exists for this workspace but could not be brought up".into()),
        );
        assert!(fallen.is_local(), "it really did fall back");
        let note = fallen.note().expect("a fallback without a reason is a lie");
        assert!(note.contains("could not be brought up"), "{note}");
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
        assert!(!substrate.is_local());
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
    /// podman has to.
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
        assert!(on_vm.can_host(&in_this_vm));
        assert!(!on_vm.can_host(&in_another));
        let local = Substrate::local_for_tests();
        assert!(local.can_host(&local_checkout));
        assert!(!local.can_host(&in_this_vm));
    }
}
