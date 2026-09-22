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

use std::sync::Arc;

use taste_core::environment::EnvironmentId;
use taste_core::{EventBus, ExecContext};
use taste_devcontainer::provision::{self, DomainState, LibvirtSession, Vm};
use taste_devcontainer::sizing::Sizing;
use taste_devcontainer::{DevcontainerConfig, EnvironmentRegistry, Substrate, SupervisorState};

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
            // A failed run must not destroy its evidence: the guest's
            // serial console is the one record of a boot that did not
            // reach sshd, so it is printed here before anything is removed.
            if std::thread::panicking() {
                match std::fs::read_to_string(vm.serial_log_path()) {
                    Ok(text) => {
                        let lines: Vec<&str> = text.lines().collect();
                        let start = lines.len().saturating_sub(60);
                        eprintln!(
                            "--- serial console of {} (last {} lines) ---",
                            vm.domain,
                            lines.len() - start
                        );
                        for line in &lines[start..] {
                            eprintln!("{line}");
                        }
                        eprintln!("--- end of serial console ---");
                    }
                    Err(e) => eprintln!("no serial console for {}: {e}", vm.domain),
                }
            }
            // TASTE_PROVISION_KEEP=1 leaves the VM defined and running for
            // a person to inspect: `virsh -c qemu:///session console`, or
            // ssh with the workspace's key. Destroy it by hand afterwards.
            if std::env::var("TASTE_PROVISION_KEEP").is_ok_and(|v| v == "1") {
                eprintln!(
                    "TASTE_PROVISION_KEEP=1: leaving {} (ssh 127.0.0.1:{}) for inspection",
                    vm.domain, vm.ssh_port
                );
                return;
            }
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
    // are fresh, and so nothing here touches a real project's state. It is
    // a repository with one commit, because the second half of this test
    // places an environment of it in the VM.
    let workspace = tempfile::tempdir().unwrap();
    let root: &Path = workspace.path();
    {
        let repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
    }
    assert!(libvirt.list(root).await.unwrap().is_empty());

    let sizing = Sizing::for_host();
    eprintln!(
        "creating a VM: {} vCPU, {} MiB, {} GiB disk",
        sizing.vcpus, sizing.memory_mib, sizing.disk_gib
    );
    let started = std::time::Instant::now();
    let vm = libvirt
        .create(
            root,
            &sizing,
            Arc::new(|fetch: taste_core::GuestImageFetch| {
                if fetch.total > 0 && fetch.done % (64 * 1024 * 1024) < 1024 * 1024 {
                    eprintln!(
                        "  guest image ({:?}): {} / {} MiB",
                        fetch.phase,
                        fetch.done >> 20,
                        fetch.total >> 20
                    );
                }
            }),
        )
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
    let keeper_container = tokio::task::spawn_blocking({
        let vm_substrate = vm_substrate.clone();
        let vm = vm.clone();
        let root = root.to_path_buf();
        move || {
            taste_devcontainer::keeper::ensure_container(&vm_substrate, &vm, &root, &|line| {
                println!("keeper build: {line}")
            })
        }
    })
    .await
    .unwrap()
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

    // An agent environment whose checkout is IN the VM: the registry clones
    // the peer here, makes the checkout over there by pushing into it,
    // reads its config through the mirror, snapshots it where the files are
    // and fetches the ref home, starts its baseline container in the VM's
    // podman, and takes it all down again.
    let envs_base = tempfile::tempdir().unwrap();
    let registry = EnvironmentRegistry::new_for_tests(
        root,
        EventBus::new(),
        ExecContext::host_unsandboxed_for_tests(),
        envs_base.path(),
    );
    registry.set_substrate(Arc::new(Substrate::vm(&vm, facts.clone(), false)));
    let env_id = EnvironmentId::parse("i-live").unwrap();
    let placed = tokio::task::spawn_blocking({
        let registry = registry.clone();
        let env_id = env_id.clone();
        move || registry.create(env_id)
    })
    .await
    .unwrap()
    .expect("the environment is placed in the VM");
    assert_eq!(placed.checkout().vm(), Some(vm.domain.as_str()));
    let checkout_path = placed.checkout().path().to_path_buf();
    eprintln!("placed {} at {}", env_id, placed.checkout().describe());
    // The peer is refs and objects: no working tree, HEAD unborn.
    assert!(placed.peer().join(".git").is_dir());
    assert!(
        !placed.peer().join("base.txt").exists(),
        "the peer has no files"
    );
    // The checkout over there has what was committed.
    let files = placed.files();
    let snapshot = tokio::task::spawn_blocking({
        let files = files.clone();
        let placed = placed.clone();
        let checkout_path = checkout_path.clone();
        move || {
            assert_eq!(
                files
                    .read_to_string(&checkout_path.join("base.txt"))
                    .unwrap(),
                "base\n"
            );
            // A config written in the VM is read through the mirror.
            files
                .write(
                    &checkout_path.join(".devcontainer/devcontainer.json"),
                    br#"{"image": "registry.fedoraproject.org/fedora-minimal:44"}"#,
                )
                .unwrap();
            placed.recheck().unwrap();
            assert!(
                DevcontainerConfig::discover(&placed.config_root())
                    .unwrap()
                    .is_some(),
                "the mirror carries the config the VM has"
            );
            // Work in progress over there is snapshotted over there, and
            // the ref comes home to the peer.
            files
                .write(&checkout_path.join("work.txt"), b"in progress\n")
                .unwrap();
            placed
                .snapshot_blocking()
                .expect("snapshotting in the VM")
                .expect("a repository")
        }
    })
    .await
    .unwrap();
    assert!(snapshot.wrote);
    let peer = git2::Repository::open(placed.peer()).unwrap();
    let snapshot_ref = peer
        .find_reference(&taste_git::snapshot_ref(env_id.as_str()))
        .expect("the snapshot ref was fetched into the peer");
    assert_eq!(snapshot_ref.target().unwrap(), snapshot.commit);
    let tree = peer.find_commit(snapshot.commit).unwrap().tree().unwrap();
    assert!(tree.get_path(Path::new("work.txt")).is_ok());
    assert!(tree
        .get_path(Path::new(".devcontainer/devcontainer.json"))
        .is_ok());

    // Its container runs in the VM, with the checkout bound at its own
    // path there — the bind that could never have worked from the host.
    placed
        .reload_baseline()
        .await
        .expect("the baseline comes up in the VM");
    assert!(
        matches!(placed.state(), SupervisorState::Running { .. }),
        "{:?}",
        placed.state()
    );
    assert!(placed.exec().has_exec_target());
    let containers = podman_in(&vm, &["ps", "--format", "{{.Names}}"])
        .await
        .unwrap();
    assert!(
        containers.contains(&format!("-{}", env_id)),
        "the environment's container is in the VM's podman: {containers}"
    );

    let destroyed = registry
        .destroy(&env_id)
        .await
        .expect("destroyed, checkout and container and peer");
    eprintln!("destroy report:{}", destroyed.leftovers_clause());
    assert!(
        destroyed.kept_checkout.is_none(),
        "the checkout in the VM could not be removed:{}",
        destroyed.leftovers_clause()
    );
    let gone = tokio::task::spawn_blocking(move || !files.exists(&checkout_path))
        .await
        .unwrap();
    assert!(gone, "the checkout was removed from the VM");

    // The primary: the folder is placed in the VM too, its uncommitted work
    // with it, and the folder becomes its peer — a commit made over there
    // fast-forwards the folder when it is clean.
    std::fs::write(root.join("wip.txt"), "wip\n").unwrap();
    let first = tokio::task::spawn_blocking({
        let registry = registry.clone();
        let vm = vm.clone();
        move || registry.place_primary(&vm)
    })
    .await
    .unwrap()
    .expect("the primary is placed in the VM");
    assert!(
        first.is_none(),
        "a first placement has no peer sync to report"
    );
    let primary = registry.primary();
    assert_eq!(primary.checkout().vm(), Some(vm.domain.as_str()));
    let primary_path = primary.checkout().path().to_path_buf();
    eprintln!("placed the primary at {}", primary.checkout().describe());
    let (has_base, wip) = tokio::task::spawn_blocking({
        let files = primary.files();
        let path = primary_path.clone();
        move || {
            (
                files.exists(&path.join("base.txt")),
                files.read_to_string(&path.join("wip.txt")).ok(),
            )
        }
    })
    .await
    .unwrap();
    assert!(has_base, "the committed file is over there");
    assert_eq!(
        wip.as_deref(),
        Some("wip\n"),
        "the folder's uncommitted work was restored over there"
    );
    // The folder made clean, a commit made in the VM the way the file tree
    // makes one, and the peer synced: the folder fast-forwards to it, file
    // and all.
    std::fs::remove_file(root.join("wip.txt")).unwrap();
    let committed = tokio::task::spawn_blocking({
        let primary = primary.clone();
        move || {
            let worktree =
                taste_devcontainer::Worktree::for_checkout(&primary.checkout(), primary.files());
            worktree
                .stage(Path::new("wip.txt"))
                .expect("staged over there");
            let id = worktree
                .commit("wip, from the VM")
                .expect("committed over there");
            primary.sync_peer_blocking().expect("the peer synced");
            id
        }
    })
    .await
    .unwrap();
    let folder = git2::Repository::open(root).unwrap();
    assert_eq!(
        folder.head().unwrap().target().unwrap().to_string(),
        committed,
        "the folder fast-forwarded to the commit made in the VM"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("wip.txt")).unwrap(),
        "wip\n",
        "the fast-forward brought the file"
    );
    let again = tokio::task::spawn_blocking({
        let registry = registry.clone();
        let vm = vm.clone();
        move || registry.place_primary(&vm)
    })
    .await
    .unwrap()
    .expect("placing again is a sync");
    let again = again.expect("a checkout that exists is synced, not remade");
    assert!(again.note.is_none(), "{:?}", again.note);

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
    // The test workspace's own state — its keys — goes with the workspace.
    let _ = std::fs::remove_dir_all(taste_core::state::workspace_state_dir(root));
}
