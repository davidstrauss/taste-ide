//! **A workspace's VMs, as a pool.**
//!
//! A workspace needs at least one VM to run its environments, and may need
//! more: environments are placed across VMs by capacity, because a few
//! smaller VMs are easier to obtain than one large one (David,
//! 2026-09-20). The pool is what the ladder asks for a VM, what the window
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

    pub fn libvirt(&self) -> &LibvirtSession {
        &self.libvirt
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

    /// A running VM for this workspace: the first of the pool, brought up,
    /// or a new one when the pool is empty and the host has room.
    pub async fn ensure_one(
        &self,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> std::result::Result<(Vm, VmFacts), PoolError> {
        if !Self::provisioning_allowed() {
            return Err(PoolError::Skipped);
        }
        self.libvirt
            .available()
            .await
            .map_err(PoolError::Unavailable)?;
        let vms = self.vms().await.map_err(PoolError::Unavailable)?;
        let vm = match vms.into_iter().next() {
            Some(vm) => vm,
            None => {
                let sizing = Sizing::for_host();
                self.check_room(&sizing).await?;
                self.libvirt
                    .create(&self.workspace_root, &sizing, progress)
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
            pool.ensure_one(|_, _| {}).await,
            Err(PoolError::Skipped)
        ));
        match before {
            Some(v) => std::env::set_var("TASTE_PROBE_CHECK", v),
            None => std::env::remove_var("TASTE_PROBE_CHECK"),
        }
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
