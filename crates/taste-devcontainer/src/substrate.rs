//! **The substrate: which podman runs this workspace's containers.**
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
//! | [`Provider::Machine`] | a local VM, behind KVM | the connection `podman machine` registered |
//! | [`Provider::Remote`] | any host with podman on it | a connection over ssh |
//!
//! A cloud VM is not a fourth kind. A provisioner authenticates to GCP/AWS/
//! Azure, creates a host, registers a connection, and hands back
//! `Provider::Remote` — provisioning reduces to *produce a connection*, and
//! nothing below this module learns a new word. That is the reason the
//! substrate is a connection abstraction and not a `--vm` flag: the machine
//! is the tier that had to work first, not the tier the design is for.
//!
//! **What the remote tier still waits on is clone locality**, and it is not
//! a detail. Every environment's checkout is a host path bound into its
//! container. A podman machine shares `$HOME` over virtiofs, so those paths
//! exist on both sides and nothing has to move. A genuinely foreign host
//! has no such share: the clone would have to live *there*, and mediated
//! publish would have to cross the wire. That work is deliberately out of
//! this batch — see `docs/ENVIRONMENTS.md` → "Remote substrate". The
//! transport is proven; the file topology is not.
//!
//! # How the provider is chosen
//!
//! By convention, not configuration (CLAUDE.md → convention over
//! configuration over code). There is no sizing knob, no provider setting,
//! and no per-project substrate:
//!
//! 1. a podman connection named by `TASTE_PODMAN_CONNECTION`, if set — the
//!    alpha seam for pointing the IDE at a host you already registered with
//!    `podman system connection add`, and what the remote tier is verified
//!    through until a provisioner exists;
//! 2. otherwise the machine named [`crate::machine::MACHINE_NAME`], if one
//!    exists — creating it is a deliberate act, so its existence *is* the
//!    choice;
//! 3. otherwise the local service, which is what every installation has and
//!    what every installation had before this module existed.
//!
//! # Never degrade silently
//!
//! A machine that exists but cannot be started — no KVM, no gvproxy, no
//! virtiofsd — falls back to local, and [`Substrate::note`] carries the
//! reason into the environment facts and the log. The substrate spike is
//! explicit about this: the helper binaries are absent from an immutable
//! host image, so the failure is *expected* on some hosts and must be
//! legible when it happens. An IDE that quietly ran on the host after the
//! user asked for a VM would be telling them their agents are behind KVM
//! when they are not.

use std::sync::Arc;

use taste_core::PodmanTarget;

use crate::machine::{self, Machine, MachineFacts};

/// Which kind of podman service the workspace's containers live on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Rootless podman on the user's own host.
    Local,
    /// A `podman machine` — a local VM. The name is the machine's, which is
    /// also the connection's.
    Machine { name: String },
    /// Any podman service reachable through a registered connection. The
    /// machine is a special case of this that the IDE also creates.
    Remote { connection: String },
}

impl Provider {
    /// The connection name, or `None` for the local service.
    pub fn connection(&self) -> Option<&str> {
        match self {
            Provider::Local => None,
            Provider::Machine { name } => Some(name),
            Provider::Remote { connection } => Some(connection),
        }
    }

    /// One phrase, for the environment facts and the log.
    pub fn describe(&self) -> String {
        match self {
            Provider::Local => "local rootless podman".into(),
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
    /// Why the substrate is what it is, when that is not obvious — a
    /// machine that would not start, a connection that did not answer.
    /// Surfaced, never swallowed.
    note: Option<String>,
    /// The machine's own numbers, when the provider is one. Held so the
    /// environment facts can be honest about what the substrate costs
    /// without asking podman again on a UI thread.
    machine: Option<MachineFacts>,
}

/// The environment variable that points the IDE at an already-registered
/// podman connection.
pub const CONNECTION_OVERRIDE_ENV: &str = "TASTE_PODMAN_CONNECTION";

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
            machine: None,
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
            machine: None,
        })
    }

    /// Resolve the substrate: the ladder in the module docs, run for real.
    ///
    /// Every rung that fails falls to the next one **with a note**, and the
    /// bottom rung — local podman — is the one that cannot fail, because it
    /// is what the IDE did before any of this existed.
    pub async fn resolve() -> Arc<Self> {
        let local = PodmanTarget::detect_local();

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
                        machine: None,
                    },
                    Err(e) => Self::local_with_note(
                        local,
                        Some(format!(
                            "{CONNECTION_OVERRIDE_ENV}={name} did not answer ({e}); \
                             running on local podman instead"
                        )),
                    ),
                });
            }
        }

        // Rung 2: the IDE's own machine, if the user has created one.
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
                        machine: Some(facts),
                    },
                    Err(e) => Self::local_with_note(
                        local,
                        Some(format!(
                            "the podman machine {} exists but would not start ({e:#}); \
                             this workspace's containers are running on local podman, \
                             NOT behind a VM",
                            machine::MACHINE_NAME
                        )),
                    ),
                });
            }
            Err(e) => {
                return Arc::new(Self::local_with_note(
                    local,
                    Some(format!(
                        "could not ask podman about machines ({e:#}); \
                         running on local podman"
                    )),
                ));
            }
        }

        Arc::new(Self::local_with_note(local, None))
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

    /// Why the substrate is what it is, when that needs saying.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    pub fn machine_facts(&self) -> Option<&MachineFacts> {
        self.machine.as_ref()
    }

    /// The substrate as a row in the environment's Resources view.
    ///
    /// `None` for local podman: there is nothing to say that the absence of
    /// a row does not already say. A machine, on the other hand, costs real
    /// host memory that no per-environment number explains — the spike
    /// measured qemu's RSS climbing to the configured ceiling and staying
    /// there — so the fleet's disk-and-memory honesty requires it be shown
    /// as its own line rather than amortised across environments that did
    /// not cause it.
    pub fn resource(&self) -> Option<crate::supervisor::ResourceInfo> {
        use crate::supervisor::{ResourceInfo, ResourceKind};
        match &self.provider {
            Provider::Local => None,
            Provider::Machine { name } => Some(ResourceInfo {
                kind: ResourceKind::Substrate,
                name: name.clone(),
                id: self.connection().unwrap_or_default().to_string(),
                status: match &self.machine {
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

/// Can this target answer at all? `podman version` is the cheapest question
/// that proves the whole path — for a connection it opens the ssh session
/// and talks to the far end's service.
async fn probe(target: &PodmanTarget) -> anyhow::Result<()> {
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
    /// never claim a VM it does not have. The spike calls this out because
    /// the helper binaries are genuinely absent on an immutable host, so
    /// this path is expected rather than exotic.
    #[test]
    fn a_failed_substrate_falls_back_loudly() {
        let fallen = Substrate::local_with_note(
            PodmanTarget::local(false),
            Some("the podman machine taste-ide exists but would not start (no gvproxy)".into()),
        );
        assert!(fallen.is_local(), "it really did fall back");
        let note = fallen.note().expect("a fallback without a reason is a lie");
        assert!(note.contains("would not start"), "{note}");
    }

    #[tokio::test]
    async fn a_connection_that_cannot_answer_is_reported_not_used() {
        // `podman -c <nonsense>` fails fast; if podman is missing entirely
        // the spawn fails, which is the same answer for this test's
        // purposes: the probe must not report success.
        let target = PodmanTarget::connection("taste-no-such-connection", false);
        assert!(probe(&target).await.is_err());
    }

    /// A machine's row carries what the machine costs the host, because no
    /// per-environment number can: the VM's memory is committed by the VM,
    /// not by the environments inside it.
    #[test]
    fn a_machine_substrate_shows_up_as_its_own_resource_row() {
        let substrate = Substrate {
            provider: Provider::Machine {
                name: "taste-ide".into(),
            },
            target: PodmanTarget::connection("taste-ide", false),
            note: None,
            machine: Some(MachineFacts {
                running: true,
                cpus: 8,
                memory_mib: 7936,
                disk_ceiling_gib: 64,
                host_storage_bytes: Some(3_300_000_000),
            }),
        };
        let row = substrate.resource().expect("a machine is worth a row");
        assert_eq!(row.kind, crate::supervisor::ResourceKind::Substrate);
        assert_eq!(row.name, "taste-ide");
        assert!(row.status.contains("7.8 GiB"), "{}", row.status);
    }
}
