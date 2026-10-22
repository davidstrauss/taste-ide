//! **A VM's disk, marked as holding nothing this machine lacks.**
//!
//! A VM is left on disk when its window closes, so the next launch is a
//! warm boot rather than a fresh one. Each of those disks is up to
//! [`crate::sizing::DISK_GIB`] of the host's, though, and the capacity
//! check counts what every one could still grow into — so a few projects
//! opened once, weeks ago, are enough for a new one to be refused a VM
//! (2026-10-11: 43.9 GiB free, 274.2 GiB claimed by five VMs, four of them
//! idle since September). Deciding which of those disks can go needs
//! knowing what is on them, and only the IDE that owned a VM knows that
//! without booting it.
//!
//! So the owning IDE says so, on disk, where every other IDE on this host
//! can read it (David, 2026-10-11: "leave an indication on disk for other
//! IDE instances to know if the VM that was owned has no unique data on
//! it. That way, each IDE can launch and leave around the disk to
//! accelerate re-launching, but not clutter the host disk in a way that's
//! hard to clean up"):
//!
//! - **Marked at the window's close**, after the last snapshot and sync,
//!   with the workspace's containers stopped first — so nothing the
//!   project runs can write after the look, and the look is the last word
//!   ([`mark_workspace_vms`]). A VM is marked when every checkout on it,
//!   and every submodule in those, has its HEAD, every ref tip, and its
//!   working tree's tree in the repository the folder holds on this host.
//!   Ignored files, volumes, and images are not counted: they are
//!   derived, and an environment IS its refs plus its config
//!   (ENVIRONMENTS → "Uncommitted work, backups, and artifacts").
//! - **Withdrawn before the VM is used again**, by whichever IDE brings it
//!   up (`LibvirtSession::bring_up`) — the one choke point every local VM
//!   passes on its way back into use.
//! - **Honoured by any IDE short of disk** ([`clear_for_room`]): marked
//!   VMs that are shut off and whose owner is not open go, least recently
//!   used first, until the new VM fits. An unmarked disk is never touched; the refusal
//!   names it instead, with what would mark it.
//!
//! The marker is `<domain>.disposable` beside the disk, and it names the
//! disk's inode, so a disk that was replaced under the same name does not
//! inherit it.
//!
//! **The close looks without stopping anything.** It used to stop every
//! container first, so nothing could write after the look; but a relaunch
//! within the shutdown's grace — the common case — then found every job an
//! agent had left running killed (2026-10-11: a mail restore, twice). So
//! the close looks at the VM as it runs, marks it, and leaves beside the
//! marker a re-check ([`recheck_script`]): the deferred shutdown's sleeper
//! runs it just before it shuts the VM down, which is when stopping the
//! containers costs nothing, and it looks again with them stopped and takes
//! the marker back when anything moved since. A relaunch cancels the
//! sleeper and finds the containers up; a shutdown still marks only a VM
//! that was quiet.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::keys::Keys;
use crate::provision::{disks_dir, DomainState, LibvirtSession, Vm};

/// How long the look at one VM may take at close. The close has its own
/// budget, and a VM that does not answer in this is simply not marked.
pub const MARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// What the marker says. Read back to check it still describes the disk
/// beside it; the rest is for a person reading the file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Marker {
    domain: String,
    workspace_root: PathBuf,
    /// When the look was taken, in seconds since the epoch.
    marked_at: u64,
    /// The disk's inode at the time: a disk made again under the same
    /// name is another disk.
    disk_inode: u64,
    /// The checkouts that were looked at, by their place in the VM.
    checkouts: Vec<String>,
}

/// Where a VM's marker lives: beside its disk.
pub fn marker_path(domain: &str) -> PathBuf {
    disks_dir().join(format!("{domain}.disposable"))
}

/// Where a VM's re-check lives, beside its marker: the script the deferred
/// shutdown runs before it shuts the VM down ([`recheck_script`]).
pub fn recheck_path(domain: &str) -> PathBuf {
    disks_dir().join(format!("{domain}.recheck"))
}

/// Take a VM's marker back: it is about to be used, and what it holds is
/// no longer known. Absent is the same as withdrawn.
pub fn withdraw(domain: &str) {
    let _ = std::fs::remove_file(recheck_path(domain));
    match std::fs::remove_file(marker_path(domain)) {
        Ok(()) => tracing::info!("{domain}: no longer marked disposable — it is in use"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("{domain}: could not withdraw its disposable marker: {e}"),
    }
}

fn disk_inode(vm: &Vm) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(vm.disk_path()).ok().map(|m| m.ino())
}

/// Whether `vm` carries a marker that still describes its disk.
pub fn is_marked(vm: &Vm) -> bool {
    let Ok(text) = std::fs::read_to_string(marker_path(&vm.domain)) else {
        return false;
    };
    let Ok(marker) = serde_json::from_str::<Marker>(&text) else {
        return false;
    };
    marker.domain == vm.domain && Some(marker.disk_inode) == disk_inode(vm)
}

fn write_marker(vm: &Vm, checkouts: Vec<String>) -> Result<()> {
    let marker = Marker {
        domain: vm.domain.clone(),
        workspace_root: vm.workspace_root.clone(),
        marked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        disk_inode: disk_inode(vm).context("the VM's disk is not there to mark")?,
        checkouts,
    };
    let path = marker_path(&vm.domain);
    let part = path.with_extension("disposable.part");
    std::fs::write(&part, serde_json::to_string_pretty(&marker)? + "\n")
        .with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, &path).with_context(|| format!("writing {}", path.display()))
}

/// The script run in the guest, as the guest user, to report what is on
/// it: per checkout under the workspace's directory and per submodule in
/// each, `CHECKOUT <place>`, then `OBJ <oid>` for HEAD and every ref tip,
/// and `TREE <oid>` for the working tree as `git add -A` would record it
/// — written through a throwaway index, so the checkout's own index and
/// working tree are not touched. `FAIL <what>` where a question could not
/// be answered, and `END` last, so a report cut short is not read as a
/// short one. `NONE` when the VM never had a checkout placed.
///
/// With `stop_containers`, the guest's containers are stopped first, so
/// nothing the project runs writes after the look — with two seconds'
/// grace, not podman's ten: the IDE's own containers run `sleep infinity`,
/// which ignores the polite signal, so every stop waited the whole grace out
/// and the close sat on this step for it.
///
/// Its git runs none of the repository's commands, as the files service's
/// does (`keeper.rs`): attributes from the empty tree, every filter driver a
/// checkout's config defines a passthrough, no fsmonitor, no hooks. And it
/// records the working tree's own bytes, which is how the snapshots this
/// is compared with were taken: under `eol=lf` a CRLF file hashed as its LF
/// form here, a tree no snapshot had, and the VM was kept for content it did
/// not have; a filter the guest could not run (nbstripout's, from
/// `.git/info/attributes`) failed the look outright, and the VM was never
/// marked (2026-10-11).
fn report_script(workspace_dir: &Path, stop_containers: bool) -> String {
    let stop = if stop_containers {
        "podman stop --all --time 2 >/dev/null 2>&1 || true\n"
    } else {
        ""
    };
    format!(
        r#"set -u
{stop}cd '{dir}' 2>/dev/null || {{ echo NONE; echo END; exit 0; }}
export GIT_ATTR_SOURCE=4b825dc642cb6eb9a060e54bf8d69288fbee4904
export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=false
export GIT_CONFIG_KEY_1=core.hooksPath GIT_CONFIG_VALUE_1=/dev/null
# Each filter driver a checkout defines, made a passthrough for its git.
passthrough() {{
  git -C "$1" config --name-only --get-regexp '^filter\..*\.(clean|smudge|process|required)$' 2>/dev/null \
    | sed 's/\.[^.]*$//' | sort -u \
    | while IFS= read -r f; do printf -- '-c %s.clean=cat -c %s.smudge=cat -c %s.process= -c %s.required=false\n' "$f" "$f" "$f" "$f"; done
}}
report() {{
  echo "CHECKOUT $2"
  if h=$(git -C "$1" rev-parse -q --verify HEAD); then echo "OBJ $h"; fi
  git -C "$1" for-each-ref --format='OBJ %(objectname)' || echo "FAIL $2: its refs could not be listed"
  idx=$(mktemp) || {{ echo "FAIL $2: no temporary index"; return; }}
  real=$(git -C "$1" rev-parse --git-path index)
  case "$real" in /*) ;; *) real="$1/$real" ;; esac
  # The checkout's index to start from; none, and the scratch one goes too:
  # an empty file is not an index, and git refuses it.
  if [ -f "$real" ]; then cp "$real" "$idx"; else rm -f "$idx"; fi
  # Word-split on purpose: one `-c` pair per word; a driver's name with a
  # space in it is not one git would take either.
  safe=$(passthrough "$1")
  # shellcheck disable=SC2086
  if GIT_INDEX_FILE="$idx" git $safe -C "$1" add -A >/dev/null 2>&1 \
     && t=$(GIT_INDEX_FILE="$idx" git $safe -C "$1" write-tree); then
    echo "TREE $t"
  else
    echo "FAIL $2: its working tree could not be read"
  fi
  rm -f "$idx"
}}
for c in */; do
  c=${{c%/}}
  [ -e "$c/.git" ] || continue
  report "$c" "$c"
  git -C "$c" submodule foreach --quiet --recursive 'echo "$displaypath"' 2>/dev/null \
    | while IFS= read -r s; do report "$c/$s" "$c/$s"; done
done
echo END
"#,
        dir = workspace_dir.display()
    )
}

/// The re-check the deferred shutdown runs for a marked VM just before it
/// shuts it down: the same look, with the containers stopped now, since the
/// VM is going down; the marker taken back unless the report is the one the
/// close's look saw (`digest`, the SHA-256 of its output). A guest that does
/// not answer reports nothing, which is not that report either. It removes
/// itself.
pub fn recheck_script(vm: &Vm, digest: &str) -> String {
    let keys = Keys::for_workspace(&vm.workspace_root);
    let quote = |arg: &str| format!("'{}'", arg.replace('\'', "'\\''"));
    let ssh: Vec<String> = keys
        .ssh_args(vm.ssh_port, ["sh", "-s"])
        .iter()
        .map(|arg| quote(arg))
        .collect();
    let look = report_script(
        &crate::provision::guest_workspace_dir(&vm.workspace_root),
        true,
    );
    recheck_text(
        &vm.domain,
        &format!("ssh {}", ssh.join(" ")),
        &look,
        digest,
        &marker_path(&vm.domain),
    )
}

/// [`recheck_script`] with what it runs `look` through spelled out:
/// `remote` is a shell command that runs the script on its stdin.
fn recheck_text(domain: &str, remote: &str, look: &str, digest: &str, marker: &Path) -> String {
    let quote = |arg: &str| format!("'{}'", arg.replace('\'', "'\\''"));
    format!(
        "# taste-ide: before {domain}'s deferred shutdown, confirm its disposable marker\n\
         got=$({remote} <<'TASTE_REPORT_EOF' | sha256sum | cut -d' ' -f1\n\
         {look}TASTE_REPORT_EOF\n\
         )\n\
         [ \"$got\" = {digest} ] || rm -f {marker}\n\
         rm -f \"$0\"\n",
        digest = quote(digest),
        marker = quote(&marker.display().to_string()),
    )
}

/// One checkout's part of a report.
#[derive(Debug, Default, PartialEq, Eq)]
struct Checkout {
    place: String,
    commits: Vec<String>,
    tree: Option<String>,
}

/// A report read: its checkouts, and what could not be answered.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    checkouts: Vec<Checkout>,
    failures: Vec<String>,
}

fn parse_report(text: &str) -> Result<Report> {
    let mut report = Report::default();
    let mut ended = false;
    for line in text.lines() {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "CHECKOUT" => report.checkouts.push(Checkout {
                place: rest.to_string(),
                ..Checkout::default()
            }),
            "OBJ" | "TREE" => {
                let Some(checkout) = report.checkouts.last_mut() else {
                    anyhow::bail!("the guest's report named an object before a checkout");
                };
                if word == "OBJ" {
                    checkout.commits.push(rest.to_string());
                } else {
                    checkout.tree = Some(rest.to_string());
                }
            }
            "FAIL" => report.failures.push(rest.to_string()),
            "END" => ended = true,
            _ => {}
        }
    }
    if !ended {
        anyhow::bail!("the guest's report was cut short");
    }
    Ok(report)
}

/// What on a VM this host does not have, by checkout, in words; empty
/// when nothing. A checkout's place is `<environment>[/<submodule path>]`,
/// and the repository it is held to is the folder's — or, for a
/// submodule, the folder's clone at the same path — since every
/// environment's refs reach the folder.
fn unique_content(report: &Report, workspace_root: &Path) -> Vec<String> {
    let mut reasons = report.failures.clone();
    for checkout in &report.checkouts {
        let sub: PathBuf = checkout.place.split('/').skip(1).collect();
        let host = workspace_root.join(&sub);
        let repository = taste_git::GitWorkspace::discover(&host)
            .filter(|git| git.workdir().canonicalize().ok() == host.canonicalize().ok());
        let Some(git) = repository else {
            reasons.push(format!(
                "{}: no repository at {} on this machine",
                checkout.place,
                host.display()
            ));
            continue;
        };
        let missing = checkout
            .commits
            .iter()
            .filter(|oid| !git.has_object(oid))
            .count();
        if missing > 0 {
            reasons.push(format!(
                "{}: {missing} commit{} not on this machine",
                checkout.place,
                if missing == 1 { "" } else { "s" }
            ));
        }
        match &checkout.tree {
            Some(tree) if git.has_object(tree) => {}
            Some(_) => reasons.push(format!(
                "{}: uncommitted changes not on this machine",
                checkout.place
            )),
            None => reasons.push(format!("{}: its working tree was not read", checkout.place)),
        }
    }
    reasons
}

/// What a look at a VM came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Marked: everything on it is on this machine.
    Marked { checkouts: usize },
    /// Not marked, and why.
    Kept { reasons: Vec<String> },
}

/// Look at a running VM — stopping its containers first when asked — and
/// mark it when nothing on it is missing here, or take back any marker it
/// had when something is.
pub async fn look_and_mark(vm: &Vm, stop_containers: bool, recheck: bool) -> Result<Verdict> {
    let (report, stdout) = match look(vm, stop_containers).await {
        Ok(look) => look,
        Err(e) => {
            withdraw(&vm.domain);
            return Err(e);
        }
    };
    let reasons = unique_content(&report, &vm.workspace_root);
    if reasons.is_empty() {
        let places: Vec<String> = report.checkouts.iter().map(|c| c.place.clone()).collect();
        let count = places.len();
        write_marker(vm, places)?;
        if recheck {
            use sha2::Digest;
            let digest: String = sha2::Sha256::digest(&stdout)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let path = recheck_path(&vm.domain);
            std::fs::write(&path, recheck_script(vm, &digest))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(Verdict::Marked { checkouts: count })
    } else {
        withdraw(&vm.domain);
        Ok(Verdict::Kept { reasons })
    }
}

/// What `env`'s checkout on `vm` — and the submodules in it — holds that
/// its peer here, `peer`, lacks, in words; empty when nothing, or when it
/// has no checkout there. The peer, not the folder, because an agent
/// environment's commits and snapshots are fetched into its own (the
/// folder holds only the branches it publishes); the primary's peer is
/// the folder. A submodule of an agent environment's is held to a clone
/// its refs-only peer does not have, so its old checkout is kept: safe,
/// at a VM's cost. A move asks this of the VM it leaves before it deletes
/// the old copy (2026-10-22's audit: deleted unchecked, what was written
/// after the move's snapshot went with it). The VM's containers are left
/// running and no marker is written or taken back: other environments may
/// still be using it.
pub async fn unique_to(vm: &Vm, env: &str, peer: &Path) -> Result<Vec<String>> {
    let (report, _) = look(vm, false).await?;
    let mine = |place: &str| place == env || place.starts_with(&format!("{env}/"));
    let report = Report {
        checkouts: report
            .checkouts
            .into_iter()
            .filter(|checkout| mine(&checkout.place))
            .collect(),
        failures: report
            .failures
            .into_iter()
            .filter(|failure| mine(failure.split(':').next().unwrap_or_default()))
            .collect(),
    };
    Ok(unique_content(&report, peer))
}

/// The look itself: the report script run in the guest over ssh, its
/// report read, and its output kept for a digest.
async fn look(vm: &Vm, stop_containers: bool) -> Result<(Report, Vec<u8>)> {
    let script = report_script(
        &crate::provision::guest_workspace_dir(&vm.workspace_root),
        stop_containers,
    );
    let keys = Keys::for_workspace(&vm.workspace_root);
    let (program, argv) = keys.ssh_argv(vm.ssh_port, ["sh", "-s"]);
    let mut child = tokio::process::Command::new(&program)
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("running sh in the guest")?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(script.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "the guest did not report: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let report = parse_report(&String::from_utf8_lossy(&output.stdout))?;
    Ok((report, output.stdout))
}

/// The window's close: every running VM of the workspace looked at as it
/// runs, and marked when nothing on it is missing here — the marker to be
/// confirmed, with the containers stopped, just before the VM's deferred
/// shutdown ([`recheck_script`]). A VM that does not answer within [`MARK_TIMEOUT`] is left as it
/// is — unmarked, since bringing it up withdrew any marker it had.
pub async fn mark_workspace_vms(workspace_root: &Path) -> Vec<(String, Result<Verdict>)> {
    let prefix = crate::provision::domain_prefix(workspace_root);
    let vms = match LibvirtSession::new().list_all().await {
        Ok(vms) => vms,
        Err(e) => return vec![(String::new(), Err(e))],
    };
    // All at once: each VM is its own machine, and the close waits on the
    // slowest of them rather than on their sum.
    let mut looks = tokio::task::JoinSet::new();
    for vm in vms
        .into_iter()
        .filter(|vm| vm.domain.starts_with(&prefix) && vm.state == DomainState::Running)
    {
        looks.spawn(async move {
            let verdict =
                match tokio::time::timeout(MARK_TIMEOUT, look_and_mark(&vm, false, true)).await {
                    Ok(verdict) => verdict,
                    Err(_) => Err(anyhow::anyhow!(
                        "did not answer within {}s",
                        MARK_TIMEOUT.as_secs()
                    )),
                };
            (vm.domain, verdict)
        });
    }
    let mut verdicts = Vec::new();
    while let Some(done) = looks.join_next().await {
        if let Ok(verdict) = done {
            verdicts.push(verdict);
        }
    }
    verdicts.sort_by(|a, b| a.0.cmp(&b.0));
    verdicts
}

/// A VM on this host that may be removed to make room: shut off, marked,
/// and not owned by an IDE that is open — which could bring it up at any
/// moment, and would withdraw the marker as it did.
fn may_clear(vm: &Vm) -> bool {
    vm.state == DomainState::ShutOff
        && is_marked(vm)
        && !(!vm.workspace_root.as_os_str().is_empty()
            && taste_core::instance::held_elsewhere(&vm.workspace_root))
}

fn allocated(vm: &Vm) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(vm.disk_path()).map_or(0, |m| m.blocks() * 512)
}

/// When a VM was last used: its disk's modification time. A running guest
/// writes its disk throughout, and its shutdown writes it last, so this is
/// when it last ran — whoever marked it, and however (David, 2026-10-11:
/// "prune using LRU VMs until it has enough space"). A disk whose time
/// cannot be read sorts first.
fn last_used(vm: &Vm) -> std::time::SystemTime {
    std::fs::metadata(vm.disk_path())
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// `vms` in the order they are cleared: least recently used first.
fn oldest_first(vms: &mut [Vm], used: impl Fn(&Vm) -> std::time::SystemTime) {
    vms.sort_by_key(|vm| used(vm));
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// The disk a new VM gets ([`crate::sizing::disk_for_new_vm`]), making
/// room first when there is not enough: marked VMs that may go
/// ([`may_clear`]) are removed, least recently used first ([`last_used`]),
/// until the new one fits —
/// and then the base images no disk is built on any more. Still short,
/// the refusal names the VMs that were kept and what would mark them.
/// `retiring` is the VM a move replaces, whose room is not counted.
pub async fn clear_for_room(libvirt: &LibvirtSession, retiring: Option<&str>) -> Result<u64> {
    let dir = disks_dir();
    let first = match crate::sizing::disk_for_new_vm(&dir, retiring) {
        Ok(gib) => return Ok(gib),
        Err(e) => e,
    };
    let mut vms = libvirt.list_all().await.unwrap_or_default();
    oldest_first(&mut vms, last_used);
    let mut cleared = false;
    for vm in vms.iter().filter(|vm| may_clear(vm)) {
        let size = allocated(vm);
        match libvirt.destroy(vm).await {
            Ok(()) => {
                cleared = true;
                libvirt.say(
                    &vm.domain,
                    format!(
                        "removed to make room: {:.1} GiB, and nothing on it that this machine \
                         lacks — its window marked it so when it closed ({})",
                        gib(size),
                        folder_name(&vm.workspace_root)
                    ),
                );
            }
            Err(e) => tracing::warn!("{}: could not be removed to make room: {e:#}", vm.domain),
        }
        if crate::sizing::disk_for_new_vm(&dir, retiring).is_ok() {
            break;
        }
    }
    if cleared {
        let freed = crate::guest::prune_unused_bases();
        if freed > 0 {
            tracing::info!(
                "removed base images no VM is built on: {:.1} GiB",
                gib(freed)
            );
        }
    }
    match crate::sizing::disk_for_new_vm(&dir, retiring) {
        Ok(gib) => Ok(gib),
        Err(still) => {
            let kept = kept_summary(&libvirt.list_all().await.unwrap_or_default());
            let error = if cleared { still } else { first };
            Err(match kept {
                Some(kept) => anyhow::anyhow!("{error:#}; {kept}"),
                None => error,
            })
        }
    }
}

fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a workspace no longer on this machine".to_string())
}

/// The shut-off VMs nothing has marked, grouped by project, in words: what
/// is taking the room, and what would let it go.
fn kept_summary(vms: &[Vm]) -> Option<String> {
    let mut groups: Vec<(String, usize, u64)> = Vec::new();
    for vm in vms
        .iter()
        .filter(|vm| vm.state == DomainState::ShutOff && !is_marked(vm))
    {
        let name = folder_name(&vm.workspace_root);
        let size = allocated(vm);
        match groups.iter_mut().find(|(n, _, _)| *n == name) {
            Some(group) => {
                group.1 += 1;
                group.2 += size;
            }
            None => groups.push((name, 1, size)),
        }
    }
    if groups.is_empty() {
        return None;
    }
    let listed: Vec<String> = groups
        .iter()
        .map(|(name, count, size)| {
            format!(
                "{name} ({count} VM{}, {:.1} GiB)",
                if *count == 1 { "" } else { "s" },
                gib(*size)
            )
        })
        .collect();
    Some(format!(
        "kept, since nothing has marked them as holding nothing this machine lacks: {}; \
         opening and closing a project marks its VMs",
        listed.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The least recently used VM is cleared first, whatever its size.
    #[test]
    fn the_least_recently_used_vm_goes_first() {
        let vm = |domain: &str| Vm {
            domain: domain.into(),
            ssh_port: 40001,
            workspace_root: "/work/proj".into(),
            state: DomainState::ShutOff,
            cloud: None,
        };
        let day = |n: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(n * 86_400);
        let mut vms = vec![vm("taste-a-week"), vm("taste-a-month"), vm("taste-a-day")];
        oldest_first(&mut vms, |vm| match vm.domain.as_str() {
            "taste-a-month" => day(1),
            "taste-a-week" => day(24),
            _ => day(30),
        });
        let order: Vec<&str> = vms.iter().map(|vm| vm.domain.as_str()).collect();
        assert_eq!(order, ["taste-a-month", "taste-a-week", "taste-a-day"]);
    }

    #[test]
    fn a_report_cut_short_is_not_read_as_a_short_one() {
        assert!(parse_report("CHECKOUT primary\nOBJ abc\n").is_err());
        let report = parse_report("CHECKOUT primary\nOBJ abc\nTREE def\nEND\n").unwrap();
        assert_eq!(report.checkouts.len(), 1);
        assert_eq!(report.checkouts[0].commits, vec!["abc"]);
        assert_eq!(report.checkouts[0].tree.as_deref(), Some("def"));
        assert_eq!(parse_report("NONE\nEND\n").unwrap(), Report::default());
    }

    /// The judgement against a real repository: what is in it is not
    /// unique, and a commit or a working tree that is not is.
    #[test]
    fn only_what_the_folder_lacks_is_unique() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let commit = repo
            .commit(Some("HEAD"), &sig, &sig, "first", &tree, &[])
            .unwrap();

        let here = Report {
            checkouts: vec![Checkout {
                place: "primary".into(),
                commits: vec![commit.to_string()],
                tree: Some(tree_id.to_string()),
            }],
            failures: vec![],
        };
        assert!(unique_content(&here, dir.path()).is_empty());

        let absent = "0123456789abcdef0123456789abcdef01234567".to_string();
        let elsewhere = Report {
            checkouts: vec![Checkout {
                place: "i-0001".into(),
                commits: vec![commit.to_string(), absent.clone()],
                tree: Some(absent),
            }],
            failures: vec![],
        };
        let reasons = unique_content(&elsewhere, dir.path());
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("1 commit not on this machine"));
        assert!(reasons[1].contains("uncommitted changes"));

        let failed = Report {
            failures: vec!["primary: its refs could not be listed".into()],
            ..Report::default()
        };
        assert_eq!(unique_content(&failed, dir.path()).len(), 1);
    }

    /// A submodule is held to the folder's clone at its path, and a path
    /// with no repository of its own there is unique, never the parent's.
    #[test]
    fn a_submodule_is_held_to_its_own_clone() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        let report = Report {
            checkouts: vec![Checkout {
                place: "primary/lib".into(),
                commits: vec![],
                tree: None,
            }],
            failures: vec![],
        };
        let reasons = unique_content(&report, dir.path());
        assert!(reasons[0].contains("no repository at"), "{reasons:?}");
    }

    /// The look reads a checkout as the snapshots do: a filter its own
    /// `.git/info/attributes` names, which cannot run here, does not fail it,
    /// and a CRLF file under `eol=lf` is recorded as its bytes.
    #[test]
    fn the_look_runs_none_of_the_checkouts_commands_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("primary");
        std::fs::create_dir_all(&checkout).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&checkout)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "filter.nb.clean", "/nowhere/python -m nbstripout"]);
        git(&["config", "filter.nb.required", "true"]);
        std::fs::create_dir_all(checkout.join(".git/info")).unwrap();
        std::fs::write(checkout.join(".git/info/attributes"), "*.ipynb filter=nb\n").unwrap();
        std::fs::write(checkout.join(".gitattributes"), "* text=auto eol=lf\n").unwrap();
        std::fs::write(checkout.join("a.ipynb"), "{}\n").unwrap();
        std::fs::write(checkout.join("m.eml"), "To: a\r\n\r\nbody\r\n").unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(report_script(dir.path(), false))
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&out.stdout);
        assert!(!report.contains("FAIL"), "{report}");
        let tree = report
            .lines()
            .find_map(|l| l.strip_prefix("TREE "))
            .expect("a tree")
            .to_string();
        let blob = git(&["rev-parse", &format!("{tree}:m.eml")]);
        let bytes = std::process::Command::new("git")
            .current_dir(&checkout)
            .args(["cat-file", "blob", &blob])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(bytes, b"To: a\r\n\r\nbody\r\n");
    }

    #[test]
    fn the_report_script_looks_in_the_workspace_and_stops_containers_first_when_asked() {
        let script = report_script(Path::new("/var/home/core/taste/abc123"), true);
        assert!(script.contains("cd '/var/home/core/taste/abc123'"));
        let stop = script.find("podman stop --all").unwrap();
        let look = script.find("report \"$c\"").unwrap();
        assert!(stop < look);
        assert!(!report_script(Path::new("/x"), false).contains("podman stop"));
        assert!(script.trim_end().ends_with("echo END"));
    }

    /// The re-check keeps the marker when the look reports what the close's
    /// did, takes it back when the checkout moved since or the guest did not
    /// answer, and removes itself either way.
    #[test]
    fn the_recheck_keeps_the_marker_only_for_the_report_the_close_saw() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("guest");
        let checkout = workspace.join("primary");
        std::fs::create_dir_all(&checkout).unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .current_dir(&checkout)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        std::fs::write(checkout.join("a.txt"), "one\n").unwrap();
        let look = report_script(&workspace, false);
        let report = std::process::Command::new("sh")
            .arg("-c")
            .arg(&look)
            .output()
            .unwrap()
            .stdout;
        let digest: String = sha2::Sha256::digest(&report)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let marker = dir.path().join("vm.disposable");
        let run = |remote: &str| {
            std::fs::write(&marker, "").unwrap();
            let script = dir.path().join("vm.recheck");
            std::fs::write(&script, recheck_text("vm", remote, &look, &digest, &marker)).unwrap();
            assert!(std::process::Command::new("sh")
                .arg(&script)
                .status()
                .unwrap()
                .success());
            assert!(!script.exists(), "the re-check removes itself");
            marker.exists()
        };
        assert!(run("sh -s"), "nothing moved: the marker stays");
        assert!(!run("false"), "no answer: the marker goes");
        std::fs::write(checkout.join("a.txt"), "two\n").unwrap();
        assert!(!run("sh -s"), "the checkout moved: the marker goes");
    }
}
