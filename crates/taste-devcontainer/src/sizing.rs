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
//! # Why a disk starts small and grows as it fills
//!
//! A sparse disk is a ceiling, and the ceiling is the guest's to reach:
//! everything an agent writes in its VM — a build's output, a runaway log,
//! a clone of something huge — lands in the qcow2 on this host's disk. So
//! the ceilings together never promise more than the disk has: free space,
//! less the floor the whole IDE keeps
//! (`taste_core::environment::MIN_FREE_DISK_BYTES`), must cover what every
//! disk could still grow into (review, 2026-09-23: "focus on preventing
//! resource exhaustion on the desktop, specifically on disk").
//!
//! What that leaves open is how the room is handed out. A disk made at
//! [`DISK_GIB`], or grown to it at every boot, held its whole share of the
//! room whether or not the guest ever wrote it, and four VMs holding 175
//! GiB they had written 68 of left a fifth nothing to start with
//! (2026-10-19). So a VM starts at [`DISK_START_GIB`] — the guest OS, its
//! images, and a working tree fit in eight — and grows by
//! [`DISK_GROW_STEP_GIB`] when its filesystem has under [`DISK_LOW_GIB`]
//! free ([`grown_disk_gib`]), checked at boot and every
//! [`DISK_CHECK_EVERY`] while it runs (David, 2026-10-19: "size these
//! smaller but leverage the upsizing mechanism more readily"). What the
//! VMs might want between them is overcommitted; what they hold is not:
//! the room is given to whichever needs it first, and a disk that cannot
//! grow says so.

use std::path::Path;

use anyhow::{bail, Result};

/// The fewest vCPUs a VM is given; a build on one core is a build that
/// never finishes.
pub const VCPU_MIN: u32 = 2;
/// The host's threads a VM leaves for the desktop. A vCPU is time, not a
/// commitment — an idle one costs the host nothing, and a contended host
/// is slower rather than down — so a VM is given every thread but these
/// (David, 2026-10-17: "Unlike memory pressure, CPU pressure isn't
/// catastrophic"). The two keep the IDE's own frames ahead of a build
/// that has every vCPU busy.
pub const VCPU_HOST_RESERVE: u32 = 2;
/// The least memory the IDE considers usable for a VM that builds things.
pub const MEMORY_MIN_MIB: u64 = 4096;
/// The most it will commit to one VM.
pub const MEMORY_MAX_MIB: u64 = 16384;
/// The largest virtual size a VM's disk grows to. Sparse; a ceiling and
/// not a cost — but a ceiling the guest can reach, which is why a disk
/// gets there only by filling ([`grown_disk_gib`]).
pub const DISK_GIB: u64 = 64;
/// The virtual size a VM's disk is made at, and the least a new VM is
/// made with. The base image is a 10 GiB disk, so an overlay on it is no
/// smaller; a VM that had run a project's container for a week held 7.7
/// GiB of it (2026-10-19), and the rest is the first growth's to cover.
pub const DISK_START_GIB: u64 = 12;
/// The guest's free space under which its disk grows.
pub const DISK_LOW_GIB: u64 = 6;
/// What a disk grows by when it is short: enough that the next growth is
/// not the next check's.
pub const DISK_GROW_STEP_GIB: u64 = 8;
/// How often a running VM's disk is looked at. Mostly a stat of its qcow2
/// on this host; the guest is asked only when that cannot rule a shortage
/// out ([`may_be_short`]).
pub const DISK_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
/// The most of a guest's data that may be in the base image rather than
/// its overlay — the OS as shipped, about 2 GiB — and of its disk that is
/// not its root filesystem's free space (the boot and EFI partitions, the
/// filesystem's own reserve), so that an overlay's allocation, plus this,
/// bounds how full the guest's filesystem can be. Four left about a
/// gibibyte unaccounted for (review, 2026-10-20).
const BASE_DATA_BOUND_GIB: u64 = 5;
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

    /// The policy as a pure function: the CPUs less the desktop's, a third
    /// of the memory clamped, and the disk a VM starts at.
    pub fn derive(host_cpus: u32, host_memory_mib: u64) -> Self {
        Self {
            vcpus: host_cpus.saturating_sub(VCPU_HOST_RESERVE).max(VCPU_MIN),
            memory_mib: (host_memory_mib / 3).clamp(MEMORY_MIN_MIB, MEMORY_MAX_MIB),
            disk_gib: DISK_START_GIB,
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

/// The virtual size, in GiB, a new VM's disk in `dir` is made at:
/// [`DISK_START_GIB`], when what is free, less
/// `taste_core::environment::MIN_FREE_DISK_BYTES`, less the room every disk
/// already in `dir` could still grow into, covers it, and refused, naming
/// the numbers, when it does not. `retiring` is a VM this one replaces —
/// a move's old VM, removed once the move is done — whose room is not
/// counted against its replacement. A filesystem that will not say how
/// full it is gets [`DISK_START_GIB`], as the IDE's other free-space
/// checks allow what they cannot measure.
pub fn disk_for_new_vm(dir: &Path, retiring: Option<&str>) -> Result<u64> {
    let Some(free) = taste_core::environment::free_bytes(dir) else {
        return Ok(DISK_START_GIB);
    };
    disk_for(free, unclaimed_bytes(dir, retiring))
}

const GIB: u64 = 1024 * 1024 * 1024;

fn disk_for(free: u64, unclaimed: u64) -> Result<u64> {
    let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
    let headroom = free.saturating_sub(floor).saturating_sub(unclaimed);
    let gib = headroom / GIB;
    if gib < DISK_START_GIB {
        bail!(
            "this disk has {:.1} GiB free, and the VMs already made could still grow into \
             {:.1} GiB of it; with {} GiB kept free for the desktop, a new VM would get \
             {gib} GiB, and it needs {DISK_START_GIB} GiB. Free some space, or remove a \
             workspace's VMs",
            free as f64 / GIB as f64,
            unclaimed as f64 / GIB as f64,
            floor / GIB,
        );
    }
    Ok(DISK_START_GIB)
}

/// Whether a disk of `virtual_size` with `allocated` bytes in its overlay
/// could have a guest under [`DISK_LOW_GIB`] free. The guest's data is in
/// the overlay or the base under it, so when the two together leave the
/// low-water mark clear, it is clear, and nobody needs to ask the guest.
pub fn may_be_short(virtual_size: u64, allocated: u64) -> bool {
    virtual_size.saturating_sub(allocated) < (DISK_LOW_GIB + BASE_DATA_BOUND_GIB) * GIB
}

/// The virtual size, in GiB, a disk of `current` bytes whose guest has
/// `guest_free` bytes free grows to, when it is short: by
/// [`DISK_GROW_STEP_GIB`], or by what is left once the floor, what every
/// disk — this one included — could still grow into, and a new VM's
/// [`DISK_START_GIB`] are set aside, capped at [`DISK_GIB`]. Holding a new
/// VM's start back means a workspace with no VM yet can still have one
/// (2026-10-13: a disk that took that last share left another project's
/// VM nothing to start with). `None` when it is not short, or there is no
/// room to grow by a gibibyte.
pub fn grown_disk_gib(current: u64, guest_free: u64, free: u64, unclaimed: u64) -> Option<u64> {
    if guest_free >= DISK_LOW_GIB * GIB {
        return None;
    }
    let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
    let current_gib = current / GIB;
    let room_gib = free
        .saturating_sub(floor)
        .saturating_sub(unclaimed)
        .saturating_sub(DISK_START_GIB * GIB)
        / GIB;
    let target = current_gib
        .saturating_add(DISK_GROW_STEP_GIB.min(room_gib))
        .min(DISK_GIB);
    (target > current_gib).then_some(target)
}

/// What the qcow2 disks in `dir` could still take from the host: each one's
/// virtual size less what it already occupies. `except` is a domain whose
/// disk is left out.
pub fn unclaimed_bytes(dir: &Path, except: Option<&str>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "qcow2"))
        .filter(|e| except.is_none_or(|domain| e.path().file_stem() != Some(domain.as_ref())))
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
    /// 31 GiB gives about 10 GiB, and every thread but the desktop's two.
    #[test]
    fn the_policy_on_the_host_it_was_decided_for() {
        let sizing = Sizing::derive(24, 31 * 1024);
        assert_eq!(sizing.vcpus, 22);
        assert_eq!(sizing.memory_mib, 31 * 1024 / 3);
        assert_eq!(sizing.disk_gib, DISK_START_GIB);
    }

    /// Bounded both ways: a laptop gets the floor, a workstation the cap.
    #[test]
    fn sizing_is_clamped_at_both_ends() {
        let small = Sizing::derive(3, 8 * 1024);
        assert_eq!(small.vcpus, VCPU_MIN);
        assert_eq!(small.memory_mib, MEMORY_MIN_MIB);
        let large = Sizing::derive(128, 256 * 1024);
        assert_eq!(large.vcpus, 126);
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
        assert!(a.vcpus >= VCPU_MIN);
        assert!((MEMORY_MIN_MIB..=MEMORY_MAX_MIB).contains(&a.memory_mib));
    }

    /// A disk grows only when its guest is short, by a step or by what is
    /// left once a new VM's start is held back, never past the ceiling,
    /// and never so far that every disk together could take the host under
    /// its floor.
    #[test]
    fn a_disk_grows_when_its_guest_is_short_and_the_host_has_room() {
        let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
        let short = (DISK_LOW_GIB - 1) * GIB;
        assert_eq!(
            grown_disk_gib(12 * GIB, DISK_LOW_GIB * GIB, floor + 500 * GIB, 0),
            None,
            "not short"
        );
        assert_eq!(
            grown_disk_gib(12 * GIB, short, floor + 500 * GIB, 2 * GIB),
            Some(12 + DISK_GROW_STEP_GIB),
            "a step"
        );
        assert_eq!(
            grown_disk_gib(12 * GIB, short, floor + (DISK_START_GIB + 3) * GIB, 0),
            Some(15),
            "what is left past a new VM's start"
        );
        assert_eq!(
            grown_disk_gib(12 * GIB, short, floor + DISK_START_GIB * GIB, 0),
            None,
            "a new VM's start is not grown into"
        );
        assert_eq!(
            grown_disk_gib(12 * GIB, short, floor + 40 * GIB, 30 * GIB),
            None,
            "the other disks' room is not grown into"
        );
        assert_eq!(
            grown_disk_gib(DISK_GIB * GIB, 0, floor + 500 * GIB, 0),
            None,
            "at the ceiling"
        );
        assert_eq!(
            grown_disk_gib((DISK_GIB - 3) * GIB, 0, floor + 500 * GIB, 0),
            Some(DISK_GIB),
            "up to the ceiling"
        );
    }

    /// The guest is asked only when the overlay and the base under it
    /// could hold enough to leave it short.
    #[test]
    fn only_a_disk_near_full_is_looked_into() {
        assert!(!may_be_short(64 * GIB, 8 * GIB));
        assert!(may_be_short(12 * GIB, 3 * GIB));
        assert!(may_be_short(12 * GIB, 13 * GIB));
    }

    #[test]
    fn a_new_disk_starts_small_and_is_refused_without_room_for_that() {
        let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
        assert_eq!(disk_for(500 * GIB, 0).unwrap(), DISK_START_GIB);
        assert_eq!(
            disk_for(floor + 100 * GIB, 88 * GIB).unwrap(),
            DISK_START_GIB
        );
        let refused = disk_for(floor + 20 * GIB, 10 * GIB).unwrap_err();
        assert!(
            format!("{refused:#}").contains("needs 12 GiB"),
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
        assert!(unclaimed_bytes(dir.path(), None) > 63 * GIB);
        assert_eq!(unclaimed_bytes(dir.path(), Some("vm")), 0);
        std::fs::write(dir.path().join("other.qcow2"), b"not a disk at all, no").unwrap();
        assert_eq!(qcow2_virtual_size(&dir.path().join("other.qcow2")), None);
    }
}
