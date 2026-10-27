//! **The golden base: what every VM of a release boots from.**
//!
//! Fedora CoreOS as it ships is a base image a VM spends its first boot
//! making into one of the IDE's: Ignition writes everything
//! [`provision::ignition`] says, the root filesystem grows, and then the
//! keeper's first act is to build the files service's image in it. Every
//! VM did that work again, and every VM's disk held its own copy of what
//! came of it. So the first VM of a release prepares a **golden base**
//! once instead: the base image booted in a VM of its own with that same
//! Ignition, the baseline image built in it, everything that would make it
//! one machine rather than another removed ([`GENERALISE_SCRIPT`]), and the
//! disk flattened and compressed into a standalone qcow2 (David,
//! 2026-10-26: "generate a reusable VM base image and toss out all other
//! source data"). Measured on the prototype (docs/spikes/golden-base.md):
//! 1.18 GiB with the baseline image in it where the base image alone was
//! 1.84, and a first boot of 7 s where it was 32.
//!
//! # A VM made from it
//!
//! has been through Ignition already, and Ignition does not run twice, so
//! what makes it this VM — its hostname, the IDE's key, its host key —
//! arrives as systemd system credentials ([`provision::guest_credentials`],
//! [`provision::GuestBoot::Credentials`]), the interface systemd's IMDS
//! import fills on a cloud too.
//!
//! # Why the builder is nobody's
//!
//! It runs no project's code and no agent, which is what lets the
//! baseline image in it go under every project's checkouts: a copy taken
//! out of a workspace's VM may not (`image::ensure_image_kept`). Its disk
//! is in [`builder_dir`], not among the workspaces' disks, so no window's
//! sweep of other workspaces' VMs takes it for one of theirs, while the
//! memory it holds is still counted with every VM's
//! (`LibvirtSession::list_all_installs`).
//!
//! # When it is made again
//!
//! For each release, once; and when [`REVISION`] moves, which is how a
//! change to what goes into it, or to how it is generalised, gets a new
//! one. A change to the guest's tuning alone does not need one: a VM is
//! brought up to that at every bring-up (`LibvirtSession::
//! tune_guest_shutdown`). The base image it was prepared from is pruned
//! once no VM overlays it (`guest::prune_unused_bases`), so a new golden
//! base is a new download. One that cannot be prepared leaves that VM on
//! the base image with an Ignition, as every VM was before.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use taste_core::{GuestImageFetch, GuestImagePhase};

use crate::guest::GuestImage;
use crate::keys::Keys;
use crate::provision::{self, DomainSpec, DomainState, GuestBoot, GuestSpec, LibvirtSession, Vm};

/// What a golden base is made by. Raised when what goes into one, or how
/// it is generalised, changes, so every release's is made again.
pub const REVISION: u32 = 1;

/// What a builder's domain is named under.
pub const BUILDER_PREFIX: &str = "taste-golden-";

/// The builder's memory: a `microdnf` transaction and a podman build, not
/// a project's workload.
const BUILDER_MEMORY_MIB: u64 = 3072;

/// The most vCPUs the builder is given.
const BUILDER_VCPUS: u32 = 4;

/// How long the builder has to power itself off once generalised. The
/// generalising waits half a minute for XFS before its trim.
const POWER_OFF_WAIT: Duration = Duration::from_secs(120);

/// Where a builder's disk, and its keys' stand-in workspace, live:
/// `guests/builder`, beside the bases and apart from the workspaces' disks.
pub fn builder_dir() -> PathBuf {
    crate::guest::images_dir().join("builder")
}

/// A release's builder disk. Named for the release, so it can be found
/// again: its being here is the preparation being under way.
fn builder_disk(image: &GuestImage) -> PathBuf {
    builder_dir().join(format!("{}-{}.qcow2", image.release, image.arch))
}

/// Whether `image`'s golden base is being prepared on this machine.
pub fn preparing(image: &GuestImage) -> bool {
    builder_disk(image).is_file()
}

/// Run as root in the builder once its image is built: everything that
/// would make every VM booted from the base the same machine, removed —
/// its host keys, the builder's key, its hostname, its machine-id (to
/// `uninitialized`, so systemd makes a new one from the VM's own SMBIOS
/// UUID and treats the boot as a first one), the random seed, systemd's
/// credential secret, NetworkManager's leases, and the logs — then synced,
/// left the half-minute XFS needs before freed extents can be discarded
/// (`provision::GUEST_TIDY_SERVICE`), trimmed so the flattened base holds
/// only what is in use, and powered off.
pub const GENERALISE_SCRIPT: &str = r#"set -e
rm -f /etc/ssh/ssh_host_*
rm -f /var/home/core/.ssh/authorized_keys.d/*
rm -f /etc/hostname
echo uninitialized > /etc/machine-id
rm -f /var/lib/systemd/random-seed /var/lib/systemd/credential.secret
rm -rf /var/lib/NetworkManager/* /var/log/journal/* /var/log/audit/*
sync
sleep 30
fstrim --all --quiet-unsupported
echo generalised
systemctl poweroff
"#;

/// `image`'s golden base, prepared from `source` (the verified base image)
/// if this machine does not have it. One preparation at a time on this
/// machine: a second caller waits for the first and finds it done.
pub async fn ensure(
    session: &LibvirtSession,
    image: &GuestImage,
    source: &Path,
    report: Arc<dyn Fn(GuestImageFetch) + Send + Sync>,
) -> Result<PathBuf> {
    let golden = image.golden_path();
    let _lock = crate::guest::lock_file(golden.with_extension("qcow2.lock")).await?;
    if golden.is_file() {
        return Ok(golden);
    }
    let started = Instant::now();
    let fetch = |phase| GuestImageFetch {
        release: image.release.clone(),
        phase,
        done: 0,
        total: 0,
    };
    report(fetch(GuestImagePhase::Preparing));
    let made = prepare(session, image, source, &golden).await;
    report(fetch(GuestImagePhase::Ready));
    let made = made?;
    session.say(
        "golden",
        format!(
            "the golden base for {} is ready in {}s: {}",
            image.release,
            started.elapsed().as_secs(),
            made.display()
        ),
    );
    // The base image it was prepared from has done its work: gone at once
    // unless a VM still overlays it.
    let freed = crate::guest::prune_unused_bases();
    if freed > 0 {
        session.say(
            "golden",
            format!(
                "removed {} MiB of base images no VM is built on",
                freed >> 20
            ),
        );
    }
    Ok(made)
}

async fn prepare(
    session: &LibvirtSession,
    image: &GuestImage,
    source: &Path,
    golden: &Path,
) -> Result<PathBuf> {
    let root = builder_dir();
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    sweep_builders(session).await;
    let keys = Keys::for_workspace(&root);
    keys.ensure().await?;
    let domain = format!(
        "{BUILDER_PREFIX}{}",
        taste_git::random_slug(provision::NAME_ENTROPY)?
    );
    let vm = Vm {
        domain: domain.clone(),
        ssh_port: provision::free_loopback_port()?,
        workspace_root: root.clone(),
        state: DomainState::ShutOff,
        cloud: None,
    };
    let disk = builder_disk(image);
    let outcome = build(session, image, source, golden, &keys, &vm, &disk).await;
    // Whatever happened, the builder goes: its domain, its disk, its
    // Ignition, its connection, and its known_hosts line. Its console log
    // is kept for a failure to be read from, and the next build replaces
    // it.
    if let Err(e) = discard(session, &vm, &disk, &keys).await {
        tracing::warn!("removing the golden base's builder {domain}: {e:#}");
    }
    outcome
}

async fn build(
    session: &LibvirtSession,
    image: &GuestImage,
    source: &Path,
    golden: &Path,
    keys: &Keys,
    vm: &Vm,
    disk: &Path,
) -> Result<PathBuf> {
    let domain = &vm.domain;
    session.say(
        domain,
        format!(
            "preparing the golden base for {} in a VM of its own",
            image.release
        ),
    );
    let _ = std::fs::remove_file(disk);
    session
        .run(
            "qemu-img",
            vec![
                "create".into(),
                "-q".into(),
                "-f".into(),
                "qcow2".into(),
                "-b".into(),
                source.display().to_string(),
                "-F".into(),
                "qcow2".into(),
                disk.display().to_string(),
                format!("{}G", crate::sizing::DISK_START_GIB),
            ],
        )
        .await
        .context("creating the builder's disk")?;
    // The same Ignition as any VM of a workspace, so the base carries
    // what every one of them would have been given at its first boot.
    let ignition = provision::ignition(&GuestSpec {
        ssh_public_key: keys.identity_public()?,
        hostname: domain.clone(),
        deny_private_networks: true,
        host_key: Some(keys.host_key()?),
        metadata_server: false,
    })?;
    provision::write_private(&vm.ignition_path(), &ignition)?;
    let xml = provision::domain_xml(&DomainSpec {
        name: domain.clone(),
        vcpus: crate::sizing::Sizing::for_host().vcpus.min(BUILDER_VCPUS),
        memory_mib: BUILDER_MEMORY_MIB,
        disk: disk.display().to_string(),
        boot: GuestBoot::Ignition(vm.ignition_path().display().to_string()),
        ssh_port: vm.ssh_port,
        workspace_root: vm.workspace_root.display().to_string(),
        serial_log: vm.serial_log_path().display().to_string(),
    })?;
    let xml_path = keys.dir().join(format!("{domain}.xml"));
    std::fs::write(&xml_path, xml).with_context(|| format!("writing {}", xml_path.display()))?;
    let defined = session
        .virsh(&["define", &xml_path.display().to_string()])
        .await;
    let _ = std::fs::remove_file(&xml_path);
    defined.context("defining the builder")?;
    keys.record_host(vm.ssh_port)?;
    session.start(vm).await?;
    session.wait_ready(vm, provision::READY_TIMEOUT).await?;

    // The files service's image, built as a VM of a workspace would build
    // it, so its tag is the one that VM looks for.
    let facts = session.facts(vm).await?;
    let substrate = crate::substrate::Substrate::vm(vm, facts, session.sandboxed);
    let config = crate::baseline::ensure_baseline_config()?;
    let lines = session.clone();
    let line_domain = domain.clone();
    let tag = tokio::task::spawn_blocking(move || {
        crate::image::ensure_image_with(&substrate, &config, "golden", &|line| {
            lines.say(&line_domain, line)
        })
    })
    .await
    .context("building the files service's image")??;
    session.say(domain, format!("built {tag}; generalising"));

    generalise(keys, vm).await?;
    let deadline = Instant::now() + POWER_OFF_WAIT;
    while session.state(vm).await? != DomainState::ShutOff {
        if Instant::now() > deadline {
            bail!(
                "{domain} did not power off within {}s of being generalised",
                POWER_OFF_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Flattened onto the base image it overlays and compressed, under a
    // name of its own until it is whole.
    let part = golden.with_extension("qcow2.part");
    let _ = std::fs::remove_file(&part);
    session.say(domain, "flattening and compressing the golden base");
    let converted = session
        .run(
            "qemu-img",
            vec![
                "convert".into(),
                "-c".into(),
                "-O".into(),
                "qcow2".into(),
                "-o".into(),
                "compression_type=zstd".into(),
                disk.display().to_string(),
                part.display().to_string(),
            ],
        )
        .await;
    if let Err(e) = converted {
        let _ = std::fs::remove_file(&part);
        return Err(e).context("flattening the golden base");
    }
    std::fs::rename(&part, golden).with_context(|| format!("installing {}", golden.display()))?;
    Ok(golden.to_path_buf())
}

/// [`GENERALISE_SCRIPT`], run as root over ssh. The power-off at its end
/// cuts the connection, so success is the line it prints first.
async fn generalise(keys: &Keys, vm: &Vm) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let (program, argv) = keys.ssh_argv(vm.ssh_port, ["sudo", "-n", "sh", "-s"]);
    let mut child = tokio::process::Command::new(&program)
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running sh in the builder")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(GENERALISE_SCRIPT.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !String::from_utf8_lossy(&output.stdout).contains("generalised") {
        bail!(
            "generalising the builder failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The builder removed: stopped, undefined, its disk and Ignition deleted,
/// its podman connection and `known_hosts` line forgotten.
async fn discard(session: &LibvirtSession, vm: &Vm, disk: &Path, keys: &Keys) -> Result<()> {
    if session
        .state(vm)
        .await
        .is_ok_and(|s| s != DomainState::ShutOff)
    {
        let _ = session.virsh(&["destroy", &vm.domain]).await;
    }
    let _ = session.virsh(&["undefine", &vm.domain]).await;
    for path in [disk.to_path_buf(), vm.ignition_path()] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("removing {}: {e}", path.display()),
        }
    }
    let podman = taste_core::podman::PodmanTarget::local(session.sandboxed);
    let (program, args) = podman.argv(["system", "connection", "remove", &vm.domain]);
    let _ = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    keys.forget_host(vm.ssh_port)
}

/// Builders a preparation cut off left behind — the IDE quit or crashed
/// mid-way — stopped and undefined. Called under the preparation's lock,
/// so none of them is another window's still at work.
async fn sweep_builders(session: &LibvirtSession) {
    let Ok(names) = session.virsh(&["list", "--all", "--name"]).await else {
        return;
    };
    for name in names
        .lines()
        .map(str::trim)
        .filter(|n| n.starts_with(BUILDER_PREFIX))
    {
        let Ok(xml) = session.virsh(&["dumpxml", name]).await else {
            continue;
        };
        // Only this install's: its disk is in this install's builder dir.
        if !provision::disk_from_xml(&xml).is_some_and(|disk| disk.starts_with(builder_dir())) {
            continue;
        }
        let _ = session.virsh(&["destroy", name]).await;
        let _ = session.virsh(&["undefine", name]).await;
        tracing::info!("removed {name}, a golden base's builder left from an earlier run");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A builder can never be taken for a workspace's VM: a workspace's
    /// prefix is `taste-` and sixteen hex digits.
    #[test]
    fn a_builder_is_named_apart_from_every_workspace() {
        let workspace = provision::domain_prefix(Path::new("/home/u/Projects/ws"));
        assert!(!BUILDER_PREFIX.starts_with(&workspace));
        assert!(!workspace.starts_with(BUILDER_PREFIX));
        assert!(BUILDER_PREFIX.starts_with("taste-"));
    }

    /// What makes one machine is removed, and the machine-id is left for
    /// systemd to make anew; the power-off is last and announced first.
    #[test]
    fn generalising_removes_what_makes_one_machine() {
        for line in [
            "rm -f /etc/ssh/ssh_host_*",
            "rm -f /var/home/core/.ssh/authorized_keys.d/*",
            "rm -f /etc/hostname",
            "echo uninitialized > /etc/machine-id",
        ] {
            assert!(GENERALISE_SCRIPT.contains(line), "{line}");
        }
        let said = GENERALISE_SCRIPT.find("echo generalised").unwrap();
        let off = GENERALISE_SCRIPT.find("systemctl poweroff").unwrap();
        assert!(said < off);
        assert!(GENERALISE_SCRIPT.trim_end().ends_with("systemctl poweroff"));
    }

    /// The builder's disk is outside the workspaces' disks, so a sweep of
    /// those never sees it, and it is named for its release.
    #[test]
    fn a_builders_disk_is_apart_from_the_workspaces() {
        let image = crate::guest::pinned_for("x86_64").unwrap();
        let disk = builder_disk(&image);
        assert!(!disk.starts_with(provision::disks_dir()));
        assert!(disk.starts_with(crate::guest::images_dir()));
        assert!(disk.to_string_lossy().contains(&image.release));
    }

    /// A golden base's name gives its release back, as a base image's
    /// does, so a VM made from it is flagged when the stream moves on.
    #[test]
    fn a_golden_base_names_its_release() {
        let image = crate::guest::pinned_for("x86_64").unwrap();
        let path = image.golden_path();
        assert_eq!(
            provision::release_from_base_name(&path.display().to_string()),
            Some(image.release.clone())
        );
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!("-r{REVISION}.qcow2")));
    }
}
