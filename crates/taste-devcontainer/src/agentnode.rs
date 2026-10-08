//! **The agent's own node, deployed beside the project's container.**
//!
//! Every ACP adapter here is a node program, and so are the MCP bridge and
//! the auth forwarder that ride with it — so an agent could only move into
//! a project's container whose image happened to carry node, and one that
//! did not (a plain Rust or Python image) left the agent outside, where on
//! a bare host there is nothing to run it at all. VS Code has the same
//! problem with its server and answers it by bringing its own node into
//! the container. This is that answer (David, 2026-09-23: "set up the
//! ability to inject our own node runtime as part of deploying the agent
//! into the container").
//!
//! - **Fetched once per VM**, into the workspace's directory there
//!   ([`guest_dir`]), from nodejs.org's official Linux build — pinned by
//!   version and checked against the SHA-256 recorded here, the way the
//!   IDE's other downloads are, so what runs is what was reviewed. The
//!   fetch runs in the VM over the workspace's ssh identity
//!   ([`ensure_in_vm`]), right after the files service is up and before any
//!   environment's container starts.
//! - **Mounted read-only** into every environment's container at
//!   [`IN_CONTAINER`] ([`mount_arg`]), so the project's image is not
//!   modified and the agent cannot modify the runtime.
//! - **Preferred when it runs**: the container's probe asks our node for
//!   its version ([`PROBE`]), which is also what tells a C library it
//!   cannot run on — the official build is glibc's, and an Alpine (musl)
//!   image falls back to its own node or, lacking one, keeps the agent
//!   out, saying why. Alpine is not supported yet, by decision.
//! - **Task rides along** (`taste_core::tasks::IDE_TASK_VERSION`): one
//!   static binary, pinned and summed the same way, fetched in the same
//!   pass, so the Tasks section lists and runs a project's Taskfile whatever
//!   its image carries — the image's own Task first, this one otherwise
//!   (David, 2026-10-04: "Can I have tasks always work in devcontainers,
//!   similarly to how agents always work by the IDE loading in the
//!   support?").
//! - **Versions accumulate.** A bump fetches the new build beside the old
//!   one rather than replacing it, because an agent already running holds
//!   the old path; a VM is disposable, so the old ones go with it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// The node the agent runs on: the current LTS when pinned. Bump it with
/// both sums below, from `https://nodejs.org/dist/v<version>/SHASUMS256.txt`.
pub const NODE_VERSION: &str = "24.21.0";
/// `node-v24.21.0-linux-x64.tar.gz`.
const NODE_SHA256_X64: &str = "6e1db87ef58b8819e5d5402eff1536491b18edd8eb7bee5ef7897876e88dc5ff";
/// `node-v24.21.0-linux-arm64.tar.gz`.
const NODE_SHA256_ARM64: &str = "724282c3b43aec998aa9527380465b45d229e021b58035f5f4f63095eabfe5d5";

/// Where the runtime directory is mounted in an environment's container.
pub const IN_CONTAINER: &str = "/opt/taste-agent";

/// The runtime directory in the VM: beside the workspace's checkouts, so it
/// is keyed like them and goes when the workspace's VM does.
pub fn guest_dir(workspace_root: &Path) -> PathBuf {
    crate::provision::guest_workspace_dir(workspace_root).join(".taste-agent-runtime")
}

/// The `-v` value that mounts [`guest_dir`] at [`IN_CONTAINER`], read-only,
/// with the shared SELinux label every container of the VM may read.
pub fn mount_arg(workspace_root: &Path) -> String {
    format!(
        "{}:{IN_CONTAINER}:ro,z",
        guest_dir(workspace_root).display()
    )
}

/// The fetch, as it runs in the VM (`sh -s`, fed on stdin). Its arguments
/// are the directory, the version, and the two sums, all fixed tokens; it
/// prints the build's directory name. The directory is made first and
/// whatever happens after, because an environment's container mounts it
/// and podman refuses a bind whose source is missing.
///
/// Task comes with it (`taste_core::tasks::IDE_TASK_VERSION`, arguments five
/// to seven), into `task-v<version>-linux-<arch>/task` beside the node, so
/// the project's tasks list and run whatever its image carries. A Task that
/// does not arrive says so on stderr and leaves the node's deploy alone.
const FETCH: &str = r#"set -eu
dir="$1"; ver="$2"; tver="$5"
mkdir -p "$dir"
case "$(uname -m)" in
  x86_64) arch=x64; sum="$3"; tarch=amd64; tsum="$6" ;;
  aarch64) arch=arm64; sum="$4"; tarch=arm64; tsum="$7" ;;
  *) echo "no node build for $(uname -m)" >&2; exit 3 ;;
esac
name="node-v$ver-linux-$arch"
tname="task-v$tver-linux-$tarch"
if [ -x "$dir/$name/bin/node" ] && [ -x "$dir/$tname/task" ]; then echo "$name"; exit 0; fi
tmp=$(mktemp -d "$dir/.fetch.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [ ! -x "$dir/$name/bin/node" ]; then
  curl -fsSL --retry 3 -o "$tmp/node.tar.gz" "https://nodejs.org/dist/v$ver/$name.tar.gz"
  echo "$sum  $tmp/node.tar.gz" | sha256sum -c --quiet -
  tar -xzf "$tmp/node.tar.gz" -C "$tmp"
  mv "$tmp/$name" "$dir/$name" || [ -x "$dir/$name/bin/node" ]
fi
if [ ! -x "$dir/$tname/task" ]; then
  mkdir -p "$tmp/$tname" &&
    curl -fsSL --retry 3 -o "$tmp/task.tar.gz" \
      "https://github.com/go-task/task/releases/download/v$tver/task_linux_$tarch.tar.gz" &&
    echo "$tsum  $tmp/task.tar.gz" | sha256sum -c --quiet - &&
    tar -xzf "$tmp/task.tar.gz" -C "$tmp/$tname" task &&
    { mv "$tmp/$tname" "$dir/$tname" || [ -x "$dir/$tname/task" ]; } ||
    echo "task $tver was not deployed" >&2
fi
echo "$name"
"#;

/// Make sure the VM at `ssh_port` has the pinned node and Task in
/// [`guest_dir`], fetching them the first time. Blocking; the node build's
/// directory name back, and what the fetch said on its way — Task not
/// arriving, which the node's deploy goes on without.
pub fn ensure_in_vm(ssh_port: u16, workspace_root: &Path) -> Result<(String, Option<String>)> {
    let keys = crate::keys::Keys::for_workspace(workspace_root);
    let dir = guest_dir(workspace_root).display().to_string();
    // ssh joins its arguments into one line for the guest's shell, so each
    // must be a token that shell leaves alone; all of them are ours.
    use taste_core::tasks::{IDE_TASK_SHA256_AMD64, IDE_TASK_SHA256_ARM64, IDE_TASK_VERSION};
    for token in [
        &dir,
        NODE_VERSION,
        NODE_SHA256_X64,
        NODE_SHA256_ARM64,
        IDE_TASK_VERSION,
        IDE_TASK_SHA256_AMD64,
        IDE_TASK_SHA256_ARM64,
    ] {
        if !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
        {
            bail!("{token} is not a token the guest's shell can be handed as it is");
        }
    }
    let (program, args) = keys.ssh_argv(
        ssh_port,
        [
            "sh".to_string(),
            "-s".into(),
            "--".into(),
            dir,
            NODE_VERSION.into(),
            NODE_SHA256_X64.into(),
            NODE_SHA256_ARM64.into(),
            IDE_TASK_VERSION.into(),
            IDE_TASK_SHA256_AMD64.into(),
            IDE_TASK_SHA256_ARM64.into(),
        ],
    );
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh to deploy the agent's node into the guest")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("ssh took no stdin")?;
        stdin.write_all(FETCH.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "deploying node {NODE_VERSION} for the agent: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok((name, (!said.is_empty()).then_some(said)))
}

/// Run in an environment's container: the `bin` directory of the pinned
/// node, when it is mounted and actually runs there, printed; exit 1
/// otherwise. Running it is the test, because a build this C library
/// cannot load is present and useless.
pub const PROBE: &str = r#"for d in /opt/taste-agent/node-v24.21.0-linux-*; do
  if [ -x "$d/bin/node" ] && "$d/bin/node" --version >/dev/null 2>&1; then echo "$d/bin"; exit 0; fi
done
exit 1"#;

/// Run in an environment's container: the directory of the IDE's pinned
/// Task, when it is mounted and runs there, printed; exit 1 otherwise.
/// What goes last on an agent's `PATH` (`taste_acp::Relocation::task_dir`)
/// and an agent's `ide_exec` shell lines'.
pub fn task_probe() -> String {
    format!(
        r#"for d in {IN_CONTAINER}/task-v{}-linux-*; do
  if [ -x "$d/task" ] && "$d/task" --version >/dev/null 2>&1; then echo "$d"; exit 0; fi
done
exit 1"#,
        taste_core::tasks::IDE_TASK_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_probe_looks_for_the_pinned_task() {
        assert!(task_probe().contains(&format!(
            "{IN_CONTAINER}/task-v{}-linux-",
            taste_core::tasks::IDE_TASK_VERSION
        )));
    }

    /// The probe looks for the version that is pinned, not a stale copy of
    /// its number.
    #[test]
    fn the_probe_looks_for_the_pinned_version() {
        assert!(PROBE.contains(&format!("{IN_CONTAINER}/node-v{NODE_VERSION}-linux-")));
    }

    #[test]
    fn the_sums_are_sha256_hex() {
        for sum in [NODE_SHA256_X64, NODE_SHA256_ARM64] {
            assert_eq!(sum.len(), 64);
            assert!(sum.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn the_runtime_is_mounted_read_only_beside_the_checkouts() {
        let root = Path::new("/home/u/project");
        let mount = mount_arg(root);
        assert!(mount.ends_with(&format!(":{IN_CONTAINER}:ro,z")), "{mount}");
        assert!(guest_dir(root).starts_with(crate::provision::guest_workspace_dir(root)));
    }

    /// The fetch script, run for real against a directory that already
    /// holds the node and the Task: it answers with the name and fetches
    /// nothing.
    #[test]
    fn a_vm_that_has_the_build_is_not_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            _ => return,
        };
        use taste_core::tasks::{IDE_TASK_SHA256_AMD64, IDE_TASK_SHA256_ARM64, IDE_TASK_VERSION};
        let tarch = if arch == "x64" { "amd64" } else { "arm64" };
        let name = format!("node-v{NODE_VERSION}-linux-{arch}");
        let bin = dir.path().join(&name).join("bin");
        let task_dir = dir
            .path()
            .join(format!("task-v{IDE_TASK_VERSION}-linux-{tarch}"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for exe in [bin.join("node"), task_dir.join("task")] {
            std::fs::write(&exe, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = Command::new("sh")
            .args(["-c", FETCH, "fetch"])
            .args([
                dir.path().to_str().unwrap(),
                NODE_VERSION,
                NODE_SHA256_X64,
                NODE_SHA256_ARM64,
                IDE_TASK_VERSION,
                IDE_TASK_SHA256_AMD64,
                IDE_TASK_SHA256_ARM64,
            ])
            // No network in this test: a fetch would fail, which is the
            // point — none is made.
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), name);
    }
}
