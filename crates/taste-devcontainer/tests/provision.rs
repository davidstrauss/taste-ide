//! The provisioner, against real libvirt.
//!
//! Separately gated from every other live test in this repo, and
//! deliberately so: these need **KVM** and a user-session libvirt, which the
//! devcontainer the rest of the suite builds in does not have, and they
//! download a gigabyte and boot a VM. `--ignored` alone is the wrong gate —
//! it is the gate for "needs podman", and podman is everywhere these tests
//! are not.
//!
//! ```sh
//! cargo test -p taste-devcontainer --test provision --no-run   # in the devcontainer
//! TASTE_PROVISION_TESTS=1 ./target/debug/deps/provision-* --ignored --nocapture --test-threads=1
//! ```
//!
//! **This is the first real boot, and it is run by a person.** It fetches
//! the pinned Fedora CoreOS image if this host has never had it (about a
//! gigabyte), creates a VM sized for this host, boots it, and proves the
//! three things the whole design rests on: the guest is a different kernel
//! from the host, podman in the guest answers over the registered
//! connection, and a container in the guest cannot reach the user's own
//! network. Then it takes the VM down again and checks that nothing is
//! left — the connection, the disk, the domain.
//!
//! What it does not clean up is the base image, which is the point of
//! having one: the next VM on this host boots without a download.

use std::path::Path;
use std::time::Duration;

use taste_devcontainer::provision::{self, DomainState, LibvirtSession, Vm};
use taste_devcontainer::sizing::Sizing;
use taste_devcontainer::Substrate;

fn enabled() -> bool {
    std::env::var("TASTE_PROVISION_TESTS").is_ok_and(|v| v == "1")
}

macro_rules! require_provision_tests {
    () => {
        if !enabled() {
            eprintln!(
                "SKIP: set TASTE_PROVISION_TESTS=1 to run the provisioner tests \
                 (they need /dev/kvm, a user-session libvirt, and boot a VM)"
            );
            return;
        }
    };
}

/// Takes the VM down however the test ends, so a failed assertion does not
/// leave a guest running and a gigabyte of disk behind.
struct Cleanup {
    libvirt: LibvirtSession,
    vm: Option<Vm>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(vm) = self.vm.take() {
            let libvirt = self.libvirt.clone();
            let handle = std::thread::spawn(move || {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                runtime.block_on(async {
                    if let Err(e) = libvirt.destroy(&vm).await {
                        eprintln!("cleanup: destroying {}: {e:#}", vm.domain);
                    }
                });
            });
            let _ = handle.join();
        }
    }
}

async fn podman_in(vm: &Vm, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["-c", vm.connection()];
    argv.extend_from_slice(args);
    let output = tokio::process::Command::new("podman")
        .args(&argv)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The whole lifecycle, once, against the machine this runs on.
#[tokio::test]
#[ignore = "live: needs TASTE_PROVISION_TESTS=1 (KVM, libvirt, network, boots a VM)"]
async fn a_vm_is_provisioned_isolates_and_is_taken_down() {
    require_provision_tests!();

    let libvirt = LibvirtSession::new();
    libvirt
        .available()
        .await
        .expect("this host must be able to provision");

    // A workspace of this test's own, so its pool is empty and its keys
    // are fresh, and so nothing here touches a real project's state.
    let workspace = tempfile::tempdir().unwrap();
    let root: &Path = workspace.path();
    assert!(libvirt.list(root).await.unwrap().is_empty());

    let sizing = Sizing::for_host();
    eprintln!(
        "creating a VM: {} vCPU, {} MiB, {} GiB disk",
        sizing.vcpus, sizing.memory_mib, sizing.disk_gib
    );
    let started = std::time::Instant::now();
    let vm = libvirt
        .create(root, &sizing, |done, total| {
            if total > 0 && done % (64 * 1024 * 1024) < 1024 * 1024 {
                eprintln!("  guest image: {} / {} MiB", done >> 20, total >> 20);
            }
        })
        .await
        .expect("create");
    let mut cleanup = Cleanup {
        libvirt: libvirt.clone(),
        vm: Some(vm.clone()),
    };
    eprintln!("defined {} in {:.1?}", vm.domain, started.elapsed());
    assert!(vm.domain.starts_with(&provision::domain_prefix(root)));
    assert_eq!(vm.state, DomainState::ShutOff);

    // The pool sees it, with the port and the workspace read back from
    // libvirt rather than remembered.
    let listed = libvirt.list(root).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].domain, vm.domain);
    assert_eq!(listed[0].ssh_port, vm.ssh_port);
    assert_eq!(
        listed[0].workspace_root.canonicalize().unwrap(),
        root.canonicalize().unwrap()
    );

    // Up, with a connection that answers.
    let booted = std::time::Instant::now();
    let facts = libvirt.ensure_running(&vm).await.expect("boot");
    eprintln!("ready in {:.1?}: {}", booted.elapsed(), facts.summary());
    assert!(facts.running);
    assert_eq!(facts.cpus, u64::from(sizing.vcpus));
    assert_eq!(facts.memory_mib, sizing.memory_mib);
    assert!(facts.host_storage_bytes.is_none_or(|b| b > 0));

    // The guest is a different kernel from the host. That difference IS
    // the isolation the whole design exists to buy.
    let host_kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap()
        .trim()
        .to_string();
    let guest_kernel = podman_in(
        &vm,
        &[
            "run",
            "--rm",
            "registry.fedoraproject.org/fedora-minimal:44",
            "uname",
            "-r",
        ],
    )
    .await
    .expect("a container runs in the guest");
    eprintln!("host kernel {host_kernel}, guest kernel {guest_kernel}");
    assert_ne!(
        guest_kernel, host_kernel,
        "the container must not be on the host kernel"
    );

    // ...and rootless, as core: the container's view of its own uid map
    // says whether podman in the guest is root's or core's.
    let uid_map = podman_in(
        &vm,
        &[
            "run",
            "--rm",
            "registry.fedoraproject.org/fedora-minimal:44",
            "cat",
            "/proc/self/uid_map",
        ],
    )
    .await
    .expect("uid map");
    eprintln!("guest container uid_map: {uid_map}");
    assert!(
        !uid_map
            .split_whitespace()
            .nth(1)
            .is_some_and(|host| host == "0"),
        "container root maps to a real root: podman in the guest is not rootless: {uid_map}"
    );

    // A random VM on the internet cannot reach the user's router; neither
    // may a container in this one. The nftables ruleset rejects the whole
    // of 192.168/16, so the connection is refused at once rather than
    // timing out.
    let lan = podman_in(
        &vm,
        &[
            "run",
            "--rm",
            "registry.fedoraproject.org/fedora-minimal:44",
            "bash",
            "-c",
            "timeout 5 bash -c 'echo > /dev/tcp/192.168.1.1/80' 2>&1; echo exit=$?",
        ],
    )
    .await
    .expect("the probe runs");
    eprintln!("LAN probe: {lan}");
    assert!(
        !lan.contains("exit=0"),
        "a container in the guest reached the user's network: {lan}"
    );

    // The keeper: the baseline image built in the guest, its container
    // up with the workspace directory mounted, and a file written and read
    // back through one exec — the files service a checkout in this VM
    // will be reached through.
    let vm_substrate = Substrate::vm(&vm, facts.clone(), false);
    let keeper_started = std::time::Instant::now();
    let keeper_container = taste_devcontainer::keeper::ensure_container(&vm_substrate, &vm, root)
        .await
        .expect("the keeper container comes up in the guest");
    eprintln!(
        "keeper container {keeper_container} up in {:.1?}",
        keeper_started.elapsed()
    );
    let keeper = tokio::task::spawn_blocking({
        let vm_substrate = vm_substrate.clone();
        let keeper_container = keeper_container.clone();
        move || {
            taste_devcontainer::Keeper::in_container(
                &vm_substrate,
                &keeper_container,
                "the test VM",
            )
        }
    })
    .await
    .unwrap()
    .expect("the keeper answers over podman exec");
    let guest_dir = taste_devcontainer::provision::guest_workspace_dir(root);
    let files = taste_core::Files::Remote(keeper.clone());
    tokio::task::spawn_blocking(move || {
        let probe = guest_dir.join("probe/hello.txt");
        files.write(&probe, b"from the host\n").unwrap();
        assert_eq!(files.read_to_string(&probe).unwrap(), "from the host\n");
        let listed = files.list(&guest_dir.join("probe")).unwrap();
        assert_eq!(listed.len(), 1);
        // The tools the checkout's git and search will run are there.
        let git = files
            .exec(&guest_dir, &["git".into(), "--version".into()])
            .unwrap();
        assert!(git.success(), "{}", git.stderr_utf8());
        let rg = files
            .exec(&guest_dir, &["rg".into(), "--version".into()])
            .unwrap();
        assert!(rg.success(), "{}", rg.stderr_utf8());
        // ...and the file is core's, so an environment container running
        // as uid 1000 can write beside it.
        let owner = files
            .exec(
                &guest_dir,
                &[
                    "stat".into(),
                    "-c".into(),
                    "%u".into(),
                    "probe/hello.txt".into(),
                ],
            )
            .unwrap();
        assert_eq!(owner.stdout_utf8().trim(), "1000", "written as core");
        files.remove(&guest_dir.join("probe"), true).unwrap();
    })
    .await
    .unwrap();
    drop(keeper);

    // The ladder chooses it, by existence, for this workspace.
    let substrate = Substrate::resolve(root).await;
    assert_eq!(
        substrate.connection(),
        Some(vm.domain.as_str()),
        "{:?}",
        substrate.note()
    );
    assert!(substrate.note().is_none(), "{:?}", substrate.note());
    let row = substrate.resource().expect("a VM is worth a row");
    eprintln!("resources row: {} — {}", row.name, row.status);

    // Down: a clean shutdown, then gone.
    libvirt.stop(&vm).await.expect("shutdown");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if libvirt.state(&vm).await.unwrap() == DomainState::ShutOff {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the guest did not shut down"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    libvirt.destroy(&vm).await.expect("destroy");
    cleanup.vm = None;
    assert!(
        libvirt.list(root).await.unwrap().is_empty(),
        "the domain is gone"
    );
    assert!(!vm.disk_path().exists(), "the disk is gone");
    assert!(!vm.ignition_path().exists(), "the ignition is gone");
    let connections = tokio::process::Command::new("podman")
        .args(["system", "connection", "list", "--format", "{{.Name}}"])
        .output()
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&connections.stdout).contains(&vm.domain),
        "the connection is gone"
    );
}
