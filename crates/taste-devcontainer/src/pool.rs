//! **A workspace's VMs, as a pool.**
//!
//! A workspace needs at least one VM to run its environments, and may need
//! more: environments are placed across VMs by what each is granted
//! (`config::Grant`, from the config's `hostRequirements`) against what
//! each VM has left, because a few smaller VMs are easier to obtain than
//! one large one (David, 2026-09-20). The pool is what the ladder asks for a VM, what the window
//! stops when it closes, and what the startup sweep reconciles against
//! libvirt. It holds nothing: every question is answered by enumerating
//! the provisioner by the workspace's name prefix (`provision::domain_prefix`),
//! so there is no list to keep in step with the hypervisor's.
//!
//! # Auto-provisioned, and what stops it
//!
//! A workspace with no VM gets one at reconcile, without being asked
//! (David, 2026-09-20: "auto-provision now for every workspace"). Two
//! things stop that, and both are refusals rather than fallbacks:
//!
//! - **The host is at capacity.** VMs are a memory commitment, and the sum
//!   of the running ones plus this one must leave the host its reserve
//!   (`sizing::room_for`). A third project open on a 32 GiB laptop is told
//!   so, loudly, rather than given a VM that swaps the desktop out.
//! - **A probe run.** `TASTE_PROBE_CHECK` renders and quits; a VM booted
//!   for a screenshot would be a gigabyte of disk and a minute of boot for
//!   a frame nobody watches.
//!
//! A host without libvirt is neither: nothing was chosen there, so the
//! ladder records it and moves on (`substrate::Descent`).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Grant;
use crate::provision::{DomainState, LibvirtSession, Vm, VmFacts};
use crate::sizing::{self, Sizing};

/// Why the pool could not supply a VM. The ladder turns each into the
/// right loudness.
#[derive(Debug)]
pub enum PoolError {
    /// A probe run. Nothing was chosen, nothing is said.
    Skipped,
    /// The provisioner cannot be used on this host at all — libvirt absent,
    /// its daemon not answering. Nothing was chosen; a log line.
    Unavailable(anyhow::Error),
    /// The host has no room for another VM. Chosen, and refused: loud.
    AtCapacity {
        running: usize,
        committed_mib: u64,
        host_mib: u64,
    },
    /// Creating or booting the VM failed. Chosen, and lost: loud.
    Failed {
        domain: Option<String>,
        error: anyhow::Error,
    },
}

/// The host's memory against the VMs it holds ([`Pool::memory_room`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRoom {
    pub committed_mib: u64,
    pub host_mib: u64,
    /// What one more VM of this host's size commits.
    pub vm_mib: u64,
}

impl MemoryRoom {
    /// What is left for VMs once the host keeps its reserve.
    pub fn available_mib(&self) -> u64 {
        self.host_mib
            .saturating_sub(sizing::HOST_RESERVE_MIB)
            .saturating_sub(self.committed_mib)
    }
}

/// What one VM can still take: its size less the guest's own reserve and
/// the grants already placed on it. The pure half of placement, so the
/// choice can be tested without a hypervisor.
pub fn free_in(facts: &VmFacts, used: Grant) -> Grant {
    Grant {
        cpus: u32::try_from(facts.cpus).unwrap_or(u32::MAX),
        memory_mib: facts.memory_mib,
    }
    .minus(sizing::GUEST_RESERVE)
    .minus(used)
}

/// The VM a grant goes on, among `candidates` of `(vm, facts, used)`: the
/// fullest one it still fits in, so environments pack rather than spread
/// and a new VM is the last resort. `None` when it fits nowhere.
pub fn best_fit(
    demand: Grant,
    candidates: &[(Vm, VmFacts, Grant)],
) -> Option<&(Vm, VmFacts, Grant)> {
    candidates
        .iter()
        .filter(|(_, facts, used)| demand.fits(free_in(facts, *used)))
        .min_by_key(|(_, facts, used)| {
            let free = free_in(facts, *used);
            (free.memory_mib, free.cpus)
        })
}

/// One workspace's VMs.
#[derive(Debug, Clone)]
pub struct Pool {
    libvirt: LibvirtSession,
    workspace_root: PathBuf,
}

impl Pool {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            libvirt: LibvirtSession::new(),
            workspace_root: workspace_root.to_path_buf(),
        }
    }

    /// The same pool, its provisioner telling each step to `sink`
    /// (`LibvirtSession::with_sink`).
    pub fn with_sink(mut self, sink: crate::provision::StepSink) -> Self {
        self.libvirt = self.libvirt.with_sink(sink);
        self
    }

    pub fn libvirt(&self) -> &LibvirtSession {
        &self.libvirt
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Whether this process may provision at all. A probe run may not.
    pub fn provisioning_allowed() -> bool {
        std::env::var_os("TASTE_PROBE_CHECK").is_none()
    }

    /// Whether the first VM on this host will have to fetch the guest
    /// image first — the one slow step worth telling the user about before
    /// it starts, since it is a gigabyte and happens once per machine.
    pub fn will_download(&self) -> bool {
        Self::provisioning_allowed()
            && virsh_is_installed()
            && crate::guest::image().is_ok_and(|image| !image.base_is_present())
    }

    /// The workspace's VMs, as libvirt has them.
    pub async fn vms(&self) -> Result<Vec<Vm>> {
        self.libvirt.list(&self.workspace_root).await
    }

    /// A running VM for this workspace: the one the primary's checkout is
    /// in when that is known (`registry::pinned_primary_vm`), else the
    /// first of the pool, brought up, or a new one when the pool is empty
    /// and the host has room.
    pub async fn ensure_one(
        &self,
        report: std::sync::Arc<dyn Fn(taste_core::GuestImageFetch) + Send + Sync>,
    ) -> std::result::Result<(Vm, VmFacts), PoolError> {
        if !Self::provisioning_allowed() {
            return Err(PoolError::Skipped);
        }
        self.libvirt
            .available()
            .await
            .map_err(PoolError::Unavailable)?;
        let vms = self.vms().await.map_err(PoolError::Unavailable)?;
        let pinned = crate::registry::pinned_primary_vm(&self.workspace_root);
        let preferred = vms
            .iter()
            .position(|vm| pinned.as_deref() == Some(vm.domain.as_str()))
            .unwrap_or(0);
        let vm = match vms.into_iter().nth(preferred) {
            Some(vm) => vm,
            None => self.make_one(report).await?,
        };
        let facts = self
            .libvirt
            .ensure_running(&vm)
            .await
            .map_err(|error| PoolError::Failed {
                domain: Some(vm.domain.clone()),
                error,
            })?;
        Ok((vm, facts))
    }

    /// One more VM for the pool, whatever the pool holds: sized for the
    /// host, refused when the host has no room for it, defined and
    /// started. What a Rebuild makes before it places anything, so a
    /// rebuild always ends with a replacement; the other callers make one
    /// only when nothing in the pool has room.
    pub async fn make_one(
        &self,
        report: std::sync::Arc<dyn Fn(taste_core::GuestImageFetch) + Send + Sync>,
    ) -> std::result::Result<Vm, PoolError> {
        let sizing = Sizing::for_host();
        self.check_room(&sizing).await?;
        self.libvirt
            .create(&self.workspace_root, &sizing, report)
            .await
            .map_err(|error| PoolError::Failed {
                domain: None,
                error,
            })
    }

    /// A VM for one more environment, chosen by fit.
    ///
    /// `demand` is the environment's grant (`config::Grant`), and
    /// `occupancy` the grants already placed on each VM, by domain. The
    /// fullest VM the grant still fits in takes it ([`best_fit`]); when it
    /// fits nowhere, a new VM is made if the host has room for one and the
    /// grant fits an empty VM of the size the host gives, and refused
    /// otherwise — never oversubscribed, because a VM's capacity is why a
    /// workspace has several (David, 2026-09-20: "some aspects of capacity
    /// don't scale linearly"). Brought up, with its facts, like
    /// [`Self::ensure_one`].
    pub async fn place(
        &self,
        demand: Grant,
        occupancy: &std::collections::HashMap<String, Grant>,
        report: std::sync::Arc<dyn Fn(taste_core::GuestImageFetch) + Send + Sync>,
    ) -> std::result::Result<(Vm, VmFacts), PoolError> {
        if !Self::provisioning_allowed() {
            return Err(PoolError::Skipped);
        }
        self.libvirt
            .available()
            .await
            .map_err(PoolError::Unavailable)?;
        let mut candidates: Vec<(Vm, VmFacts, Grant)> = Vec::new();
        // Nothing is placed in a VM behind the stream: its environments are
        // on their way out of it (`crate::migration`), and a new one would
        // only join them.
        let current = crate::guest::image().ok().map(|image| image.release);
        for vm in self.vms().await.map_err(PoolError::Unavailable)? {
            if let (Some(current), Some(release)) =
                (current.as_deref(), self.libvirt.release_of(&vm).await)
            {
                if crate::guest::release_is_behind(&release, current) {
                    continue;
                }
            }
            let facts = self
                .libvirt
                .facts(&vm)
                .await
                .map_err(|error| PoolError::Failed {
                    domain: Some(vm.domain.clone()),
                    error,
                })?;
            let used = occupancy.get(&vm.domain).copied().unwrap_or(Grant {
                cpus: 0,
                memory_mib: 0,
            });
            candidates.push((vm, facts, used));
        }
        let vm = match best_fit(demand, &candidates) {
            Some((vm, _, _)) => vm.clone(),
            None => {
                let sizing = Sizing::for_host();
                let empty = Grant {
                    cpus: sizing.vcpus,
                    memory_mib: sizing.memory_mib,
                }
                .minus(sizing::GUEST_RESERVE);
                if !demand.fits(empty) {
                    return Err(PoolError::Failed {
                        domain: None,
                        error: anyhow::anyhow!(
                            "a grant of {} does not fit a VM of this host's size ({} after the \
                             guest's reserve); lower the config's hostRequirements",
                            demand.describe(),
                            empty.describe()
                        ),
                    });
                }
                self.check_room(&sizing).await?;
                self.libvirt
                    .create(&self.workspace_root, &sizing, report)
                    .await
                    .map_err(|error| PoolError::Failed {
                        domain: None,
                        error,
                    })?
            }
        };
        let facts = self
            .libvirt
            .ensure_running(&vm)
            .await
            .map_err(|error| PoolError::Failed {
                domain: Some(vm.domain.clone()),
                error,
            })?;
        Ok((vm, facts))
    }

    /// One VM of the pool by name, running, with its facts — for a VM that
    /// holds restored environments and is not the workspace's first.
    pub async fn ensure_vm(&self, domain: &str) -> std::result::Result<(Vm, VmFacts), PoolError> {
        let vms = self.vms().await.map_err(PoolError::Unavailable)?;
        let Some(vm) = vms.into_iter().find(|vm| vm.domain == domain) else {
            return Err(PoolError::Failed {
                domain: Some(domain.to_string()),
                error: anyhow::anyhow!("this workspace's pool has no VM {domain}"),
            });
        };
        let facts = self
            .libvirt
            .ensure_running(&vm)
            .await
            .map_err(|error| PoolError::Failed {
                domain: Some(vm.domain.clone()),
                error,
            })?;
        Ok((vm, facts))
    }

    /// Refuse a new VM the host cannot hold beside the ones already running
    /// — every workspace's, not only this one's, because the memory is the
    /// host's.
    async fn check_room(&self, sizing: &Sizing) -> std::result::Result<(), PoolError> {
        let host_mib = sizing::host_memory_mib().unwrap_or(u64::MAX);
        let all = self
            .libvirt
            .list_all()
            .await
            .map_err(PoolError::Unavailable)?;
        let mut running = 0usize;
        let mut committed_mib = 0u64;
        for vm in all.iter().filter(|vm| vm.state == DomainState::Running) {
            running += 1;
            committed_mib += match self.libvirt.facts(vm).await {
                Ok(facts) => facts.memory_mib,
                // A VM whose size cannot be read is assumed to be one of
                // ours: refusing on a number we could not read is wrong,
                // but so is admitting one we could not count.
                Err(_) => sizing.memory_mib,
            };
        }
        if sizing::room_for(committed_mib, sizing.memory_mib, host_mib) {
            Ok(())
        } else {
            Err(PoolError::AtCapacity {
                running,
                committed_mib,
                host_mib,
            })
        }
    }

    /// The host's memory as the room check counts it: what the running
    /// VMs — every workspace's — commit, what the host has, and what one
    /// more VM would take.
    pub async fn memory_room(&self) -> Result<MemoryRoom> {
        let sizing = Sizing::for_host();
        let host_mib = sizing::host_memory_mib().unwrap_or(0);
        let mut committed_mib = 0u64;
        for vm in self
            .libvirt
            .list_all()
            .await?
            .iter()
            .filter(|vm| vm.state == DomainState::Running)
        {
            committed_mib += self
                .libvirt
                .facts(vm)
                .await
                .map_or(sizing.memory_mib, |facts| facts.memory_mib);
        }
        Ok(MemoryRoom {
            committed_mib,
            host_mib,
            vm_mib: sizing.memory_mib,
        })
    }

    /// Whether the host has room for one more VM of the size it gives.
    pub async fn room_for_one(&self) -> std::result::Result<(), PoolError> {
        self.check_room(&Sizing::for_host()).await
    }

    /// Running Taste VMs that no IDE owns: the domain wears another
    /// workspace's prefix, and either its XML names no folder or nobody
    /// holds that folder's supervision lock. A launch stops these outright
    /// (David, 2026-09-22: "simply stop any local Taste IDE VMs that lack
    /// an active owning IDE"). Each would stop on its own within minutes —
    /// the closed window's sleeper, the guest's own timer — but a launch
    /// wants its room now, and a VM nobody is using is a commitment of the
    /// host's memory for nothing.
    pub async fn unowned_vms(&self) -> Result<Vec<Vm>> {
        let mine = crate::provision::domain_prefix(&self.workspace_root);
        Ok(self
            .libvirt
            .list_all()
            .await?
            .into_iter()
            .filter(|vm| vm.state == DomainState::Running)
            .filter(|vm| !vm.domain.starts_with(&mine))
            .filter(|vm| {
                vm.workspace_root.as_os_str().is_empty()
                    || !taste_core::instance::held_elsewhere(&vm.workspace_root)
            })
            .collect())
    }

    /// ACPI shutdown for every VM of the pool that is running.
    pub async fn stop_all(&self) -> Result<usize> {
        let mut stopped = 0;
        for vm in self.vms().await? {
            if vm.state == DomainState::Running {
                self.libvirt.stop(&vm).await?;
                stopped += 1;
            }
        }
        Ok(stopped)
    }

    /// VMs the IDE made for workspaces that are no longer on this machine —
    /// the folder they name is gone. Reported, never removed: a VM's disk
    /// may hold work, and the removal is one `virsh undefine` the user can
    /// read first.
    pub async fn stale(&self) -> Result<Vec<Vm>> {
        Ok(self
            .libvirt
            .list_all()
            .await?
            .into_iter()
            .filter(|vm| !vm.workspace_root.as_os_str().is_empty() && !vm.workspace_root.exists())
            .collect())
    }
}

/// Is `virsh` on the host at all? A `PATH` walk, which is what the shell
/// would do; through the sandbox it asks the host's `PATH` by running it.
fn virsh_is_installed() -> bool {
    if taste_core::podman::sandboxed() {
        return std::process::Command::new("flatpak-spawn")
            .args(["--host", "sh", "-c", "command -v virsh"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("virsh").is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe run never provisions: a VM for a screenshot is a gigabyte
    /// and a minute for a frame nobody watches.
    #[tokio::test]
    async fn a_probe_run_is_skipped_before_anything_is_asked() {
        // `TASTE_PROBE_CHECK` is process-global; this test sets it and
        // restores it, and runs alone against it.
        let before = std::env::var_os("TASTE_PROBE_CHECK");
        std::env::set_var("TASTE_PROBE_CHECK", "1");
        assert!(!Pool::provisioning_allowed());
        let pool = Pool::new(Path::new("/work/proj"));
        assert!(!pool.will_download());
        assert!(matches!(
            pool.ensure_one(std::sync::Arc::new(|_| {})).await,
            Err(PoolError::Skipped)
        ));
        match before {
            Some(v) => std::env::set_var("TASTE_PROBE_CHECK", v),
            None => std::env::remove_var("TASTE_PROBE_CHECK"),
        }
    }

    /// The choice `place` makes, as the pure function it is: the fullest
    /// VM the grant fits in, and none when it fits nowhere.
    #[test]
    fn placement_is_the_fullest_vm_the_grant_fits_in() {
        let vm = |domain: &str| Vm {
            domain: domain.into(),
            ssh_port: 40001,
            workspace_root: "/work/proj".into(),
            state: DomainState::Running,
        };
        let facts = VmFacts {
            running: true,
            cpus: 12,
            memory_mib: 10240,
            disk_ceiling_gib: 64,
            host_storage_bytes: None,
        };
        let grant = |cpus: u32, memory_mib: u64| Grant { cpus, memory_mib };
        // 10240 - 1024 reserve = 9216 MiB and 11 CPUs to grant per VM.
        let candidates = vec![
            (vm("taste-a-x"), facts.clone(), grant(2, 4096)),
            (vm("taste-a-y"), facts.clone(), grant(0, 0)),
            (vm("taste-a-z"), facts.clone(), grant(8, 8192)),
        ];
        let chosen = best_fit(Grant::DEFAULT, &candidates).map(|(vm, _, _)| vm.domain.as_str());
        assert_eq!(
            chosen,
            Some("taste-a-x"),
            "the fullest one with room, not the empty one"
        );
        let big = best_fit(grant(8, 8192), &candidates).map(|(vm, _, _)| vm.domain.as_str());
        assert_eq!(
            big,
            Some("taste-a-y"),
            "only the empty one has room for a big grant"
        );
        assert!(
            best_fit(grant(12, 4096), &candidates).is_none(),
            "more CPUs than any VM grants"
        );
        assert_eq!(free_in(&facts, grant(2, 4096)), grant(9, 5120));
    }

    /// A stale VM is one whose workspace folder is gone; a VM the IDE did
    /// not make (no workspace in its metadata) is not judged at all.
    #[test]
    fn staleness_is_a_missing_folder_not_a_missing_label() {
        let gone = Vm {
            domain: "taste-a-1".into(),
            ssh_port: 40001,
            workspace_root: "/definitely/not/here/proj".into(),
            state: DomainState::ShutOff,
        };
        let unlabelled = Vm {
            workspace_root: PathBuf::new(),
            ..gone.clone()
        };
        let here = Vm {
            workspace_root: std::env::temp_dir(),
            ..gone.clone()
        };
        let stale: Vec<&Vm> = [&gone, &unlabelled, &here]
            .into_iter()
            .filter(|vm| !vm.workspace_root.as_os_str().is_empty() && !vm.workspace_root.exists())
            .collect();
        assert_eq!(stale, vec![&gone]);
    }
}
