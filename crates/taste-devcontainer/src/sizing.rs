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
//!
//! # Why a disk is sized to what is free
//!
//! A sparse disk is a ceiling, and the ceiling is the guest's to reach:
//! everything an agent writes in its VM — a build's output, a runaway log,
//! a clone of something huge — lands in the qcow2 on this host's disk. Two
//! 64 GiB ceilings on a disk with 50 GiB free is a desktop an agent can
//! fill. So a VM's virtual size is what the disk can actually give it
//! ([`disk_for_new_vm`]): what is free, less the floor the whole IDE keeps
//! (`taste_core::environment::MIN_FREE_DISK_BYTES`), less what the VMs
//! already made could still grow into — at most [`DISK_GIB`], and refused
//! below [`DISK_MIN_GIB`] (review, 2026-09-23: "focus on preventing
//! resource exhaustion on the desktop, specifically on disk").

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
/// The largest virtual size a VM's disk is given. Sparse; a ceiling and
/// not a cost — but a ceiling the guest can reach, which is why it is
/// lowered to what the host can give ([`disk_for_new_vm`]).
pub const DISK_GIB: u64 = 64;
/// The smallest disk worth making: the guest OS, an image or two, and a
/// build.
pub const DISK_MIN_GIB: u64 = 16;
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

/// The virtual size, in GiB, a new VM's disk in `dir` can be given without
/// the VMs together being able to take the disk under the floor: what is
/// free, less `taste_core::environment::MIN_FREE_DISK_BYTES`, less the
/// room every disk already in `dir` could still grow into, capped at
/// [`DISK_GIB`]. Refused below [`DISK_MIN_GIB`], naming the numbers. A
/// filesystem that will not say how full it is gets [`DISK_GIB`], as the
/// IDE's other free-space checks allow what they cannot measure.
pub fn disk_for_new_vm(dir: &Path) -> Result<u64> {
    let Some(free) = taste_core::environment::free_bytes(dir) else {
        return Ok(DISK_GIB);
    };
    disk_for(free, unclaimed_bytes(dir))
}

const GIB: u64 = 1024 * 1024 * 1024;

fn disk_for(free: u64, unclaimed: u64) -> Result<u64> {
    let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
    let headroom = free.saturating_sub(floor).saturating_sub(unclaimed);
    let gib = (headroom / GIB).min(DISK_GIB);
    if gib < DISK_MIN_GIB {
        bail!(
            "this disk has {:.1} GiB free, and the VMs already made could still grow into \
             {:.1} GiB of it; with {} GiB kept free for the desktop, a new VM would get \
             {gib} GiB, and it needs {DISK_MIN_GIB} GiB. Free some space, or remove a \
             workspace's VMs",
            free as f64 / GIB as f64,
            unclaimed as f64 / GIB as f64,
            floor / GIB,
        );
    }
    Ok(gib)
}

/// What the qcow2 disks in `dir` could still take from the host: each one's
/// virtual size less what it already occupies.
pub fn unclaimed_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "qcow2"))
        .filter_map(|e| {
            let size = qcow2_virtual_size(&e.path())?;
            let taken = e.metadata().ok()?.blocks() * 512;
            Some(size.saturating_sub(taken))
        })
        .sum()
}

/// A qcow2's virtual size, from its header: the magic `QFI\xfb`, then the
/// size as a big-endian u64 at offset 24 (the format's documented layout).
/// `None` for anything that is not a qcow2.
pub fn qcow2_virtual_size(path: &Path) -> Option<u64> {
    use std::io::Read;
    let mut header = [0u8; 32];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    if &header[..4] != b"QFI\xfb" {
        return None;
    }
    Some(u64::from_be_bytes(header[24..32].try_into().ok()?))
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

    /// A new VM gets what the disk can give once the floor and the other
    /// VMs' room are set aside, at most the ceiling, and is refused when
    /// that is too little to be worth making.
    #[test]
    fn a_new_disk_is_sized_to_what_the_host_can_give() {
        let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
        assert_eq!(disk_for(500 * GIB, 0).unwrap(), DISK_GIB);
        assert_eq!(disk_for(floor + 40 * GIB, 0).unwrap(), 40);
        assert_eq!(disk_for(floor + 100 * GIB, 64 * GIB).unwrap(), 36);
        let refused = disk_for(floor + 20 * GIB, 10 * GIB).unwrap_err();
        assert!(
            format!("{refused:#}").contains("needs 16 GiB"),
            "{refused:#}"
        );
    }

    #[test]
    fn a_qcow2_header_names_its_virtual_size() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("vm.qcow2");
        let mut header = vec![0u8; 64];
        header[..4].copy_from_slice(b"QFI\xfb");
        header[24..32].copy_from_slice(&(64 * GIB).to_be_bytes());
        std::fs::write(&disk, &header).unwrap();
        assert_eq!(qcow2_virtual_size(&disk), Some(64 * GIB));
        assert!(unclaimed_bytes(dir.path()) > 63 * GIB);
        std::fs::write(dir.path().join("other.qcow2"), b"not a disk at all, no").unwrap();
        assert_eq!(qcow2_virtual_size(&dir.path().join("other.qcow2")), None);
    }
}
