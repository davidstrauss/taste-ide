//! The machine provider, against real `podman machine`.
//!
//! Separately gated from every other live test in this repo, and
//! deliberately so: these need **KVM**, which the devcontainer the rest of
//! the suite builds in does not have, and they cost a VM boot. `--ignored`
//! alone is the wrong gate — it is the gate for "needs podman", and podman
//! is everywhere these tests are not.
//!
//! ```sh
//! cargo test -p taste-devcontainer --test machine --no-run   # in the devcontainer
//! TASTE_MACHINE_TESTS=1 ./target/debug/deps/machine-* --ignored --nocapture --test-threads=1
//! ```
//!
//! What is worth testing live here is the part that cannot be tested any
//! other way: **the helper arrangement**. `gvproxy` is absent from an
//! immutable Fedora host and `virtiofsd` is not on `$PATH`, so the IDE
//! fetches one and links the other into a directory it owns and points
//! `containers.conf` at it. Every piece of that is a claim about a real
//! filesystem and a real download, and the spike named it the largest
//! portability risk in the whole direction.

use std::path::Path;

use taste_devcontainer::machine::{self, Helpers, Machine, State};
use taste_devcontainer::Substrate;

fn enabled() -> bool {
    std::env::var("TASTE_MACHINE_TESTS").is_ok_and(|v| v == "1")
}

macro_rules! require_machine_tests {
    () => {
        if !enabled() {
            eprintln!(
                "SKIP: set TASTE_MACHINE_TESTS=1 to run the machine tests \
                 (they need /dev/kvm and boot a VM)"
            );
            return;
        }
    };
}

/// The whole helper arrangement, made from scratch and verified.
///
/// This is the test that says the IDE can run a VM on a host that ships
/// neither helper — the claim the spike said was the real risk. It removes
/// what is there first, so the fetch is genuinely exercised rather than
/// assumed from a previous run.
#[test]
#[ignore = "live: needs TASTE_MACHINE_TESTS=1 (network + a real host)"]
fn the_helpers_are_arranged_in_user_space_and_verified() {
    require_machine_tests!();

    let dir = machine::helpers_dir();
    // Nothing is installed to the host, ever. Assert it about the path
    // itself, because that is the invariant a future edit could lose.
    assert!(
        !dir.starts_with("/usr") && !dir.starts_with("/etc") && !dir.starts_with("/opt"),
        "helpers must live in the IDE's own data dir, not on the host: {}",
        dir.display()
    );

    // Force the fetch path.
    let _ = std::fs::remove_file(dir.join("gvproxy"));
    let _ = std::fs::remove_file(dir.join("virtiofsd"));

    let helpers = Helpers::arrange().expect("the helper arrangement must succeed on this host");
    assert_eq!(helpers.dir(), dir.as_path());

    let gvproxy = dir.join("gvproxy");
    assert!(gvproxy.is_file(), "gvproxy was not fetched");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&gvproxy).unwrap().permissions().mode() & 0o111,
        0o111,
        "podman has to be able to execute it"
    );
    // It is a real binary and it answers, which is more than the hash says.
    let out = std::process::Command::new(&gvproxy)
        .arg("--help")
        .output()
        .expect("running the fetched gvproxy");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(said.contains("Usage"), "gvproxy said: {said}");

    // virtiofsd is LINKED, not copied: it is the host's binary and must
    // track the host's updates.
    let link = dir.join("virtiofsd");
    let target = std::fs::read_link(&link).expect("virtiofsd must be a symlink, not a copy");
    assert!(
        target.is_absolute() && target.exists(),
        "{}",
        target.display()
    );

    // And the config podman is pointed at names exactly this directory.
    let conf = std::fs::read_to_string(dir.join("containers.conf")).unwrap();
    assert!(conf.contains("helper_binaries_dir"), "{conf}");
    assert!(conf.contains(&dir.display().to_string()), "{conf}");

    // Re-arranging is idempotent and does NOT re-download: the hash of the
    // file already there matches the pin, so it is kept.
    let before = std::fs::metadata(&gvproxy).unwrap().modified().unwrap();
    Helpers::arrange().unwrap();
    assert_eq!(
        std::fs::metadata(&gvproxy).unwrap().modified().unwrap(),
        before,
        "a verified helper must not be fetched again"
    );
}

/// A gvproxy that is not the pinned one is replaced, not trusted.
///
/// The hash is checked on every arrange, not only after a download, so a
/// corrupted or substituted binary is self-healing rather than sticky. This
/// is what makes "fetched" an acceptable answer at all.
#[test]
#[ignore = "live: needs TASTE_MACHINE_TESTS=1 (network + a real host)"]
fn a_helper_that_does_not_match_the_pin_is_replaced() {
    require_machine_tests!();
    let dir = machine::helpers_dir();
    let gvproxy = dir.join("gvproxy");
    Helpers::arrange().unwrap();

    // Unlink before writing: a running machine has its gvproxy open, and
    // writing over a live executable is `ETXTBSY`. (The production path
    // never hits this — `ensure_gvproxy` writes a temporary and renames
    // over the target, which replaces the name without touching the inode
    // the running process holds. That is why it is done that way.)
    std::fs::remove_file(&gvproxy).unwrap();
    std::fs::write(&gvproxy, b"#!/bin/sh\necho not gvproxy\n").unwrap();
    Helpers::arrange().expect("a bad helper must be repaired, not accepted");

    let bytes = std::fs::read(&gvproxy).unwrap();
    assert!(
        bytes.len() > 1_000_000,
        "the impostor is still there ({} bytes)",
        bytes.len()
    );
}

/// The machine comes up, reports itself honestly, and hosts containers.
///
/// Sizing is asserted as *bounded and deliberate* rather than as an exact
/// number: it is derived from the host, so a fixed expectation here would
/// only be right on the machine that wrote it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs TASTE_MACHINE_TESTS=1 (KVM, and boots a VM)"]
async fn the_machine_starts_and_reports_what_it_costs() {
    require_machine_tests!();
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: no /dev/kvm on this host");
        return;
    }

    let local = taste_core::PodmanTarget::detect_local();
    let machine = Machine::default_machine(local);

    let facts = machine
        .ensure_running()
        .await
        .expect("the machine must come up (creating it if it is not there)");
    eprintln!("machine facts: {}", facts.summary());

    assert!(facts.running);
    assert!(facts.cpus >= 2, "{facts:?}");
    assert!(facts.memory_mib >= 4096, "{facts:?}");
    assert_eq!(facts.disk_ceiling_gib, machine::DISK_CEILING_GIB);
    // The footprint is measured or honestly absent — never a comforting
    // zero. Disk honesty is a standing requirement of the fleet view.
    assert!(
        facts.host_storage_bytes.is_none_or(|bytes| bytes > 0),
        "a measured footprint of exactly zero is not a measurement"
    );
    assert_eq!(machine.state().await.unwrap(), State::Running);

    // ...and the substrate ladder finds it. This is the convention that
    // replaces a setting: the machine existing IS the choice.
    let substrate = Substrate::resolve().await;
    assert!(
        !substrate.is_local(),
        "a running machine must be chosen: {:?}, note {:?}",
        substrate.provider(),
        substrate.note()
    );
    assert_eq!(substrate.connection(), Some(machine::MACHINE_NAME));
    assert!(substrate.note().is_none(), "{:?}", substrate.note());

    // The row the Resources view shows, with real numbers in it.
    let row = substrate.resource().expect("a machine is worth a row");
    eprintln!("resources row: {} — {}", row.name, row.status);
    assert!(row.status.contains("running"), "{}", row.status);
    assert!(row.status.contains("committed"), "{}", row.status);

    // The guest is a different kernel from the host. That difference IS
    // the isolation the whole substrate exists to buy, so it is asserted
    // rather than assumed.
    let out = substrate
        .std_command(&["info".into(), "--format".into(), "{{.Host.Kernel}}".into()])
        .output()
        .expect("asking the machine for its kernel");
    let guest = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let host = String::from_utf8_lossy(
        &std::process::Command::new("uname")
            .arg("-r")
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    eprintln!("guest kernel {guest} / host kernel {host}");
    assert!(!guest.is_empty());
    assert_ne!(
        guest, host,
        "the machine is running the host's own kernel — that is not a VM"
    );
}
