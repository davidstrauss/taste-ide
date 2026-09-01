//! The baseline environment, against real podman.
//!
//! The unit tests say what the supervisor *decides*; these say what podman
//! actually does with that decision, which is the half that matters for the
//! two claims the design rests on:
//!
//! - a workspace with **no devcontainer config at all** comes up anyway,
//!   with a real container an agent can run commands in; and
//! - that container's view of the checkout is **read-only**, so the shell
//!   safe mode now has cannot edit project source the mediated write path
//!   would have refused.
//!
//! Neither is provable in a unit test: the first is a build and a run, and
//! the second is a mount flag that either holds in the kernel or does not.
//! An assertion that `"ro,Z"` appears in an argv proves we *asked*.
//!
//! `#[ignore]`d because it needs podman. It costs no tokens, touches no
//! credential, and cleans up the container it starts.
//!
//! Run it where podman is reachable:
//!
//! ```sh
//! cargo test -p taste-devcontainer --test baseline -- --ignored --nocapture
//! ```
//!
//! On a machine set up the way CLAUDE.md describes — cargo in a container,
//! podman only on the host — build in the devcontainer and run outside:
//!
//! ```sh
//! cargo test -p taste-devcontainer --test baseline --no-run   # in the container
//! ./target/debug/deps/baseline-* --ignored --nocapture --test-threads=1
//! ```

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use taste_core::{ConfigAuthority, EventBus, ExecContext};
use taste_devcontainer::{EnvironmentIdentity, Substrate, Supervisor, SupervisorState};

/// The substrate this run exercises.
///
/// Local by default — the claim under test is about the baseline, not about
/// where it runs. But the substrate work made "where" a real variable, and
/// the cheapest honest proof that the baseline comes up **inside a VM** is
/// to run this same suite against one:
///
/// ```sh
/// TASTE_PODMAN_CONNECTION=taste-ide ./target/debug/deps/baseline-* --ignored
/// ```
///
/// Nothing in the test bodies knows which it is, which is the point: if the
/// abstraction leaked, these would need two versions.
fn substrate() -> Arc<Substrate> {
    match std::env::var("TASTE_PODMAN_CONNECTION") {
        Ok(name) if !name.trim().is_empty() => Substrate::connection_for_tests(name.trim()),
        _ => Substrate::local_for_tests(),
    }
}

/// A directory a container on this substrate can actually bind.
///
/// `/tmp` cannot be shared into a podman machine — `podman machine init
/// --volume` refuses that destination outright, and the default share is
/// `$HOME:$HOME`. Real workspaces and clones always live under `$HOME` or
/// `$XDG_STATE_HOME`, so this only ever bites fixtures; it bites them the
/// moment the suite is pointed at a machine, which is when it is most
/// confusing.
fn tempdir() -> tempfile::TempDir {
    match std::env::var("TASTE_PODMAN_CONNECTION") {
        Ok(name) if !name.trim().is_empty() => {
            let base = std::path::PathBuf::from(std::env::var("HOME").expect("HOME"))
                .join(".cache/taste-ide/tests");
            std::fs::create_dir_all(&base).unwrap();
            tempfile::Builder::new()
                .prefix("baseline-")
                .tempdir_in(&base)
                .unwrap()
        }
        _ => tempfile::tempdir().unwrap(),
    }
}

fn podman(args: &[&str]) -> std::process::Output {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    substrate()
        .std_command(&owned)
        .output()
        .unwrap_or_else(|e| panic!("running `podman {}`: {e}", args.join(" ")))
}

/// What this host's SELinux is doing, for the record: a read-only bind
/// proven on `Permissive` proves less than one proven on `Enforcing`.
fn selinux_mode() -> String {
    Command::new("getenforce")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|mode| !mode.is_empty())
        .unwrap_or_else(|| "<no getenforce>".into())
}

/// Removes whatever the supervisor started, however the test ends.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = podman(&["rm", "-f", "-t", "1", &self.0]);
    }
}

/// A workspace with a git checkout in it and deliberately no
/// `.devcontainer/` — the state that used to mean "nothing can run here".
fn workspace_without_a_config() -> tempfile::TempDir {
    let dir = tempdir();
    std::fs::write(
        dir.path().join("README.md"),
        "# a repo with no devcontainer\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    dir
}

fn supervisor(root: &Path) -> Arc<Supervisor> {
    let exec = ExecContext::host_unsandboxed_for_tests();
    let substrate = substrate();
    exec.set_podman_target(substrate.target().clone());
    Supervisor::new_outside_container_for_tests(
        EnvironmentIdentity::primary(root),
        EventBus::new(),
        exec,
        substrate,
    )
}

/// Run a command in the environment's container through its own
/// `ExecContext` — the same route `ide_exec` and agent terminals take.
fn exec_in_environment(sup: &Supervisor, argv: &[&str]) -> std::process::Output {
    let spec = sup.exec().resolve(argv[0], &argv[1..], false);
    Command::new(&spec.program)
        .args(&spec.args)
        .output()
        .expect("spawning through the environment's ExecContext")
}

/// The whole baseline claim, end to end: a repo with no devcontainer gets a
/// container anyway, it is the baseline, commands run in it, and the
/// checkout is read-only inside it while an IDE-mediated write to the
/// config scope still lands and is visible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman on this machine"]
async fn a_workspace_with_no_config_gets_a_baseline_it_can_run_in() {
    eprintln!("SELinux: {}", selinux_mode());
    let workspace = workspace_without_a_config();
    let root = workspace.path();
    let sup = supervisor(root);
    let _cleanup = Cleanup(sup.container_name());

    // Before anything runs: no project config, and no exec target.
    sup.recheck().unwrap();
    assert_eq!(sup.state(), SupervisorState::NoConfig);
    assert!(
        !sup.exec().has_exec_target(),
        "nothing is running yet, so there is nowhere to run"
    );

    // Bring the environment up. There is no config to read, so this is the
    // baseline rung or nothing.
    sup.reload().await.expect("the baseline must come up");

    assert!(
        matches!(sup.state(), SupervisorState::Running { .. }),
        "state was {:?}; log:\n{}",
        sup.state(),
        sup.logs_tail(40).join("\n")
    );
    assert_eq!(
        sup.config_authority(),
        ConfigAuthority::Baseline,
        "a workspace with no devcontainer runs the IDE's config, not the project's"
    );

    // The mode, as the rest of the IDE reads it: safe mode (so writes stay
    // confined and the tree stays locked), with somewhere to run.
    assert!(
        !sup.exec().is_container(),
        "the baseline is not container mode"
    );
    assert!(
        sup.exec().has_exec_target(),
        "but it IS an exec target — that is the point"
    );
    assert!(sup.exec().is_baseline());

    // `ide_exec`'s route, for real.
    let out = exec_in_environment(&sup, &["sh", "-c", "node --version && git --version"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exec failed: {out:?}");
    assert!(
        text.contains("v") && text.contains("git version"),
        "the baseline must carry node and git: {text}"
    );

    // The checkout is visible at its real host path — the cwd invariant
    // that lets a session survive the move to a project devcontainer.
    let out = exec_in_environment(&sup, &["cat", &format!("{}/src/main.rs", root.display())]);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("fn main"),
        "the checkout must be readable at its host path: {out:?}"
    );

    // ...and read-only. This is the assertion the mount flag exists for:
    // the shell safe mode now has must not be able to edit project source.
    let out = exec_in_environment(
        &sup,
        &[
            "sh",
            "-c",
            &format!("echo pwned >> {}/src/main.rs", root.display()),
        ],
    );
    assert!(
        !out.status.success(),
        "the checkout must be READ-ONLY in the baseline; the write succeeded"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
        "fn main() {}\n",
        "and nothing may have reached the host copy"
    );

    // Creating a new project file is refused for the same reason.
    let out = exec_in_environment(
        &sup,
        &["sh", "-c", &format!("touch {}/planted", root.display())],
    );
    assert!(
        !out.status.success(),
        "the bind is read-only for creates too"
    );
    assert!(!root.join("planted").exists());

    // The repair loop's write, host-side and IDE-mediated: authoring the
    // config is precisely what safe mode is for, and `write_allowed` is
    // what permits it. The mount does not, and must not need to.
    let dc = root.join(".devcontainer");
    std::fs::create_dir_all(&dc).unwrap();
    let config_path = dc.join("devcontainer.json");
    assert!(
        taste_core::policy::write_allowed(root, true, &config_path),
        "safe mode exists so this write is allowed"
    );
    assert!(
        !taste_core::policy::write_allowed(root, true, &root.join("src/main.rs")),
        "and so this one is not"
    );
    std::fs::write(&config_path, "{\"image\": \"registry.example/img:1\"}\n").unwrap();

    // The agent sees its own edit through the read-only bind, immediately.
    let out = exec_in_environment(
        &sup,
        &[
            "cat",
            &format!("{}/.devcontainer/devcontainer.json", root.display()),
        ],
    );
    assert!(
        out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains("registry.example/img:1"),
        "an IDE-mediated write must be visible in the container: {out:?}"
    );

    // And now the loop closes: a healthy project config beside a running
    // baseline reads as drift, which is what raises the banner and makes
    // `devcontainer_reload` ask the user to apply it.
    sup.recheck().unwrap();
    assert!(
        sup.pending_changes(),
        "a repaired project config must show up as something to apply"
    );

    sup.stop().await.unwrap();
    assert!(
        !sup.exec().has_exec_target(),
        "stopping drops to the last rung: nowhere to run, and never the host"
    );
}

/// The ladder's first rung still wins. A workspace WITH a usable config
/// gets the project's container and full container mode — the baseline is
/// a fallback, not a new default, and this is the test that would fail if
/// it had quietly become one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman on this machine"]
async fn a_workspace_with_a_usable_config_still_gets_container_mode() {
    let workspace = workspace_without_a_config();
    let root = workspace.path();
    let dc = root.join(".devcontainer");
    std::fs::create_dir_all(&dc).unwrap();
    // Built rather than pulled, so the test needs no registry.
    std::fs::write(
        dc.join("Containerfile"),
        "FROM registry.fedoraproject.org/fedora-minimal@sha256:\
         df76793c3a7152c653dc9715f6926f4a12fa85018b1fc315288cff6f755c4bb6\n\
         RUN microdnf install -y --setopt=install_weak_deps=0 --nodocs coreutils \
         && microdnf clean all\n",
    )
    .unwrap();
    std::fs::write(
        dc.join("devcontainer.json"),
        "{\"name\": \"project\", \"build\": {\"dockerfile\": \"Containerfile\"}, \
         \"workspaceFolder\": \"/workspace\"}\n",
    )
    .unwrap();

    let sup = supervisor(root);
    let _cleanup = Cleanup(sup.container_name());

    sup.recheck().unwrap();
    assert_eq!(sup.state(), SupervisorState::ConfigDetected);

    sup.reload().await.unwrap_or_else(|e| {
        panic!(
            "project config failed: {e:#}\n{}",
            sup.logs_tail(40).join("\n")
        )
    });
    assert_eq!(
        sup.config_authority(),
        ConfigAuthority::Project,
        "a usable project config is what runs"
    );
    assert!(sup.exec().is_container(), "that is container mode");
    assert!(!sup.exec().is_baseline());

    // And in container mode the workspace is writable, which is the
    // difference the whole authority split exists to express.
    let out = exec_in_environment(
        &sup,
        &[
            "sh",
            "-c",
            &format!("echo edited >> {}/src/main.rs", root.display()),
        ],
    );
    assert!(
        out.status.success(),
        "container mode writes to the checkout: {out:?}"
    );
    assert!(std::fs::read_to_string(root.join("src/main.rs"))
        .unwrap()
        .contains("edited"));

    sup.stop().await.unwrap();
}

/// A project config the security validator refuses must not run, and must
/// not leave the environment dead either: it falls to the baseline, and the
/// reason is in the log where the agent repairing it can read it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs podman on this machine"]
async fn a_refused_config_falls_to_the_baseline_and_says_why() {
    let workspace = workspace_without_a_config();
    let root = workspace.path();
    let dc = root.join(".devcontainer");
    std::fs::create_dir_all(&dc).unwrap();
    std::fs::write(
        dc.join("devcontainer.json"),
        "{\"image\": \"registry.example/img:1\", \
         \"mounts\": [\"source=/etc,target=/host-etc,type=bind\"]}\n",
    )
    .unwrap();

    let sup = supervisor(root);
    let _cleanup = Cleanup(sup.container_name());

    sup.reload().await.expect("the baseline must still come up");
    assert_eq!(sup.config_authority(), ConfigAuthority::Baseline);
    assert!(
        matches!(sup.state(), SupervisorState::Running { .. }),
        "a refused config leaves a usable environment, not a dead one"
    );

    let log = sup.logs_tail(40).join("\n");
    assert!(
        log.contains("baseline environment:") && log.contains("refused"),
        "the refusal must be readable by whoever has to fix it:\n{log}"
    );

    // The mount it asked for is not there — the refusal was real, not a
    // warning it got to proceed past.
    let out = exec_in_environment(&sup, &["test", "-d", "/host-etc"]);
    assert!(!out.status.success(), "the refused bind must not exist");

    sup.stop().await.unwrap();
}
