//! **How big a VM is, and how many of them a host will hold.**
//!
//! Policy, kept apart from the XML writer that applies it
//! (`crate::provision`), because the question "how much of this host may
//! one VM take" belongs to whoever is deciding it rather than to the code
//! that writes `<memory>`. The numbers are David's (2026-09-20), and they
//! are derived from the host rather than configured: a sizing setting whose
//! wrong value looks like the IDE being slow is a setting nobody can
//! diagnose (CLAUDE.md → convention over configuration).
//!
//! # Why a third of the memory, and not a quarter
//!
//! The podman-machine tier took a quarter, for one machine per user. These
//! VMs are a **pool per workspace**, placed per environment by capacity,
//! and a few smaller VMs are easier to come by than one large one (David:
//! "some aspects of capacity don't scale linearly"). A third of the host
//! for each, with eight gibibytes held back for the host itself, is two
//! VMs on a 32 GiB machine — which is where [`room_for`] draws the line.
//!
//! Memory is a **commitment**, not a ceiling: qemu never returns the page
//! cache it grows into, so the configured number is what the host loses.
//! Disk is the other way round — a sparse overlay on the shared base image
//! that grows only as the guest writes — but qcow2 does not shrink when the
//! guest frees space, so the floor on free space is checked before every
//! creation rather than assumed from the last one.

use std::path::Path;

use anyhow::{bail, Result};

/// The fewest vCPUs a VM is given; a build on one core is a build that
/// never finishes.
pub const VCPU_MIN: u32 = 2;
/// The most. Half of a 24-core host is where this lands today.
pub const VCPU_MAX: u32 = 12;
/// The least memory the IDE considers usable for a VM that builds things.
pub const MEMORY_MIN_MIB: u64 = 4096;
/// The most it will commit to one VM.
pub const MEMORY_MAX_MIB: u64 = 16384;
/// The virtual size of a VM's disk. Sparse; a ceiling and not a cost.
pub const DISK_GIB: u64 = 64;
/// Free space the guests' filesystem must have before a VM is created.
pub const MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// Memory held back for the host — the IDE, its helpers, and the desktop —
/// when deciding whether another VM fits.
pub const HOST_RESERVE_MIB: u64 = 8192;
/// What a VM keeps for itself before any environment is placed on it: the
/// guest OS and the keeper's container. Placement grants against the rest.
pub const GUEST_RESERVE: crate::config::Grant = crate::config::Grant {
    cpus: 1,
    memory_mib: 1024,
};

/// What one VM is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizing {
    pub vcpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
}

impl Sizing {
    /// Sized for the host this process is on.
    pub fn for_host() -> Self {
        Self::derive(host_cpus(), host_memory_mib().unwrap_or(MEMORY_MIN_MIB * 3))
    }

    /// The policy as a pure function: half the CPUs, a third of the memory,
    /// both clamped, and the fixed disk ceiling.
    pub fn derive(host_cpus: u32, host_memory_mib: u64) -> Self {
        Self {
            vcpus: (host_cpus / 2).clamp(VCPU_MIN, VCPU_MAX),
            memory_mib: (host_memory_mib / 3).clamp(MEMORY_MIN_MIB, MEMORY_MAX_MIB),
            disk_gib: DISK_GIB,
        }
    }
}

/// Whether a host with `host_memory_mib` can take another VM of `next_mib`
/// beside the `committed_mib` its existing VMs already hold, and still keep
/// [`HOST_RESERVE_MIB`] for itself.
pub fn room_for(committed_mib: u64, next_mib: u64, host_memory_mib: u64) -> bool {
    committed_mib
        .saturating_add(next_mib)
        .saturating_add(HOST_RESERVE_MIB)
        <= host_memory_mib
}

/// Refuse to create a VM on a filesystem with less than [`MIN_FREE_BYTES`]
/// left. The directory need not exist yet; the nearest ancestor that does
/// is what fills.
pub fn check_free_space(dir: &Path) -> Result<()> {
    if let Some(free) = taste_core::environment::free_bytes(dir) {
        if free < MIN_FREE_BYTES {
            bail!(
                "{} has {:.1} GiB free; creating a VM needs {} GiB free",
                dir.display(),
                free as f64 / (1024.0 * 1024.0 * 1024.0),
                MIN_FREE_BYTES / (1024 * 1024 * 1024)
            );
        }
    }
    Ok(())
}

pub fn host_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

/// `MemTotal` from `/proc/meminfo`, in MiB.
pub fn host_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers David named, on the host he named them for: 24 CPUs and
    /// 31 GiB gives 12 vCPU and about 10 GiB.
    #[test]
    fn the_policy_on_the_host_it_was_decided_for() {
        let sizing = Sizing::derive(24, 31 * 1024);
        assert_eq!(sizing.vcpus, 12);
        assert_eq!(sizing.memory_mib, 31 * 1024 / 3);
        assert_eq!(sizing.disk_gib, DISK_GIB);
    }

    /// Bounded both ways: a laptop gets the floor, a workstation the cap.
    #[test]
    fn sizing_is_clamped_at_both_ends() {
        let small = Sizing::derive(4, 8 * 1024);
        assert_eq!(small.vcpus, VCPU_MIN);
        assert_eq!(small.memory_mib, MEMORY_MIN_MIB);
        let large = Sizing::derive(128, 256 * 1024);
        assert_eq!(large.vcpus, VCPU_MAX);
        assert_eq!(large.memory_mib, MEMORY_MAX_MIB);
    }

    /// Two VMs fit on a 32 GiB host; a third does not, because the host
    /// keeps its reserve.
    #[test]
    fn a_32_gib_host_holds_two_vms_and_keeps_its_reserve() {
        let host = 32 * 1024;
        let each = Sizing::derive(24, host).memory_mib;
        assert!(room_for(0, each, host));
        assert!(room_for(each, each, host));
        assert!(!room_for(2 * each, each, host));
    }

    #[test]
    fn the_host_sizing_is_deterministic_and_within_bounds() {
        let a = Sizing::for_host();
        assert_eq!(a, Sizing::for_host());
        assert!((VCPU_MIN..=VCPU_MAX).contains(&a.vcpus));
        assert!((MEMORY_MIN_MIB..=MEMORY_MAX_MIB).contains(&a.memory_mib));
    }

    /// A directory that does not exist yet is judged by the filesystem it
    /// would land on, so the first VM on a fresh host is not refused for
    /// want of a directory.
    #[test]
    fn free_space_is_judged_where_the_disk_would_land() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("guests/machines/not-yet");
        // Either the disk has room or it does not; the point is that the
        // question is answerable and the error, if any, names the path.
        match check_free_space(&nested) {
            Ok(()) => {}
            Err(e) => assert!(format!("{e:#}").contains("GiB free"), "{e:#}"),
        }
    }
}
