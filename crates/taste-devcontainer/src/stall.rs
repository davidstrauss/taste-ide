//! A container the guest kernel has stopped giving CPU to, found and let go.
//!
//! Every container the IDE runs has a CPU quota (`cpu.max`, the VM's grant
//! to it). After the host slept, a guest's kernel can leave a container's
//! quota throttled for good: the period passes, the cgroup is counted as
//! throttled in it, and not a microsecond of CPU is given, period after
//! period. Everything in the container stops — and every `podman exec`
//! into it, since the exec's process joins the cgroup and waits with the
//! rest, holding the container's lock while it does, so `podman ps` waits
//! behind it too. remise's primary sat that way for forty minutes after a
//! three-hour suspend (2026-11-19): the IDE's hosting probe never came
//! back, and its chat said "Starting Claude Code" for as long. Writing the
//! cgroup's own `cpu.max` back to it, unchanged, restarts the quota's
//! accounting, and the container ran at once.
//!
//! So a check runs in each local VM: each container's `usage_usec` and
//! `nr_throttled` are read twice, a couple of seconds apart, and a cgroup
//! whose throttled count rose while its usage did not move is that state
//! and no other — a container at its quota is throttled AFTER using it, so
//! a healthy one's usage always moves with its throttling. Its `cpu.max`,
//! and its children's, are written back as they are. A false match would
//! cost nothing: the write changes no value.

use anyhow::{Context, Result};

use crate::keys::Keys;
use crate::provision::Vm;

/// How often each VM is checked.
pub const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The check, run as the guest's user (rootless podman's cgroups are
/// delegated to it, so no sudo): `$1` is the cgroup root and `$2` the
/// seconds between the two readings. Prints each scope it let go.
pub const SCRIPT: &str = r#"set -u
root="${1:-/sys/fs/cgroup}"; gap="${2:-2}"
reading() { awk '/^usage_usec /{u=$2} /^nr_throttled /{t=$2} END{print u+0, t+0}' "$1/cpu.stat" 2>/dev/null; }
quota() { [ -r "$1/cpu.max" ] && [ "$(cut -d' ' -f1 "$1/cpu.max")" != max ]; }
first=""
for scope in "$root"/user.slice/user-*.slice/user@*.service/user.slice/libpod-*.scope; do
  [ -d "$scope" ] || continue
  for g in "$scope" "$scope"/*/; do
    g="${g%/}"
    quota "$g" || continue
    first="$first$scope $g $(reading "$g")
"
  done
done
[ -n "$first" ] || exit 0
sleep "$gap"
printf '%s' "$first" | while read -r scope g used throttled; do
  set -- $(reading "$g")
  [ "${1:-}" = "$used" ] && [ "${2:-0}" -gt "$throttled" ] || continue
  for f in "$scope/cpu.max" "$scope"/*/cpu.max; do
    [ -w "$f" ] || continue
    v="$(cat "$f")" && printf '%s\n' "$v" > "$f"
  done
  echo "${scope##*/}"
done | sort -u
"#;

/// Run the check in `vm`, and say which containers' scopes it let go.
/// Blocking: an ssh round trip and the check's own two seconds.
pub fn unstick(keys: &Keys, vm: &Vm) -> Result<Vec<String>> {
    let (program, argv) = keys.ssh_argv(vm.ssh_port, ["sh", "-s"]);
    let mut child = std::process::Command::new(&program)
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("checking VM {}'s containers for a stalled quota", vm.domain))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(SCRIPT.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "checking VM {}'s containers for a stalled quota: {}",
            vm.domain,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_stat(dir: &Path, used: u64, throttled: u64) {
        std::fs::write(
            dir.join("cpu.stat"),
            format!(
                "usage_usec {used}\nuser_usec {used}\nnr_periods 9\nnr_throttled {throttled}\n"
            ),
        )
        .unwrap();
    }

    /// A scope as rootless podman makes it: the scope and its `container`
    /// child, each with a quota.
    fn scope(root: &Path, name: &str) -> std::path::PathBuf {
        let scope = root.join(format!(
            "user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{name}.scope"
        ));
        std::fs::create_dir_all(scope.join("container")).unwrap();
        for dir in [scope.clone(), scope.join("container")] {
            std::fs::write(dir.join("cpu.max"), "400000 100000\n").unwrap();
            write_stat(&dir, 1000, 10);
        }
        scope
    }

    /// The check, between whose two readings `meanwhile` changes the
    /// counters as the kernel would; what it printed.
    fn run(root: &Path, meanwhile: impl FnOnce() + Send + 'static) -> Vec<String> {
        let child = std::process::Command::new("sh")
            .args(["-c", SCRIPT, "check"])
            .arg(root)
            .arg("1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        meanwhile();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_container_throttled_without_running_is_let_go() {
        let root = tempfile::tempdir().unwrap();
        let stuck = scope(root.path(), "stuck");
        let busy = scope(root.path(), "busy");
        let idle = scope(root.path(), "idle");
        let unlimited = scope(root.path(), "unlimited");
        for dir in [unlimited.clone(), unlimited.join("container")] {
            std::fs::write(dir.join("cpu.max"), "max 100000\n").unwrap();
        }
        let written = |dir: &Path| {
            std::fs::metadata(dir.join("container/cpu.max"))
                .unwrap()
                .modified()
                .unwrap()
        };
        let before = [&stuck, &busy, &idle, &unlimited].map(|d| written(d));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (s, b, u) = (stuck.clone(), busy.clone(), unlimited.clone());
        let printed = run(root.path(), move || {
            // Throttled period after period, and not a microsecond used.
            write_stat(&s.join("container"), 1000, 40);
            // At its quota: throttled, and using it.
            write_stat(&b.join("container"), 900_000, 40);
            // A quota of none is never throttled; this is not that state.
            write_stat(&u.join("container"), 1000, 40);
            // `idle` uses nothing and is not throttled: nothing to do.
        });
        assert_eq!(printed, vec!["libpod-stuck.scope".to_string()]);
        // Written back as it was, the scope's and its child's.
        for dir in [stuck.clone(), stuck.join("container")] {
            assert_eq!(
                std::fs::read_to_string(dir.join("cpu.max")).unwrap(),
                "400000 100000\n"
            );
        }
        let after = [&stuck, &busy, &idle, &unlimited].map(|d| written(d));
        assert_ne!(before[0], after[0], "the stuck one is written");
        assert_eq!(before[1..], after[1..], "and nothing else is");
    }

    #[test]
    fn no_containers_is_no_work() {
        let root = tempfile::tempdir().unwrap();
        assert!(run(root.path(), || {}).is_empty());
    }
}
