//! **The peer and its checkout, kept in step over ssh.**
//!
//! An environment whose checkout is in a VM has two repositories: the
//! checkout over there, and its **peer** on this host holding the refs
//! (docs/ENVIRONMENTS.md → "The topology: remote by default"). They are
//! related by git's own transport — push and fetch over the VM's ssh
//! forward — and never by a file-level sync: divergence is then an
//! ordinary branch relationship the review surfaces already draw.
//!
//! # The `git` CLI, on purpose
//!
//! libgit2 here is built without ssh (`Cargo.toml`), and stays so: the
//! user's own push already goes through the `git` CLI so that ssh, its
//! keys, and its prompts are the system's. This module runs the same CLI
//! with the **workspace's** identity and `known_hosts`
//! (`crate::keys`), never the user's — the VM is the project's, and so is
//! the trust. `receive-pack` and `upload-pack` run in the VM, which is the
//! untrusted side; this host runs only its own client.
//!
//! # No credential enters the VM
//!
//! The push carries refs. Nothing here ever runs `git push` FROM the VM to
//! anywhere: the user's remote is reached from the peer, on this host,
//! with the user's keys, which is the "no push, ever" the standard makes
//! (CLAUDE.md → the boundary is the host).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use taste_core::podman::host_argv;

use taste_core::files::Files;

use crate::keys::Keys;
use crate::provision::{Vm, GUEST_USER};

/// The refspecs a checkout's peer tracks: the branches, the tags, and the
/// IDE's own refs — snapshots among them.
pub const PEER_REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/taste/*:refs/taste/*",
];

/// The `ssh://` URL of a repository at `path` in `vm`.
pub fn guest_url(vm: &Vm, path: &Path) -> String {
    format!(
        "ssh://{GUEST_USER}@127.0.0.1:{}{}",
        vm.ssh_port,
        path.display()
    )
}

/// Push the peer's refs into the checkout at `path` in `vm`.
pub fn push_to_guest(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    path: &Path,
    refspecs: &[&str],
) -> Result<()> {
    push_to_guest_with_progress(peer, vm, keys, path, refspecs, &mut |_| {})
}

/// [`push_to_guest`], handing each of git's progress lines to
/// `progress` as it is printed — the seed of a fresh checkout is the
/// whole repository over the VM's ssh forward, which is minutes of
/// silence without them (David, 2026-09-22: "It still seems to hang a
/// long time without feedback on checking out the workspace").
pub fn push_to_guest_with_progress(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    path: &Path,
    refspecs: &[&str],
    progress: &mut dyn FnMut(&GitProgress),
) -> Result<()> {
    let mut args = vec!["push".to_string(), "--progress".into(), guest_url(vm, path)];
    args.extend(refspecs.iter().map(|s| s.to_string()));
    git_streaming(peer, keys, &args, progress)
        .with_context(|| format!("pushing to {} in {}", path.display(), vm.domain))
}

/// One of git's progress lines — `Writing objects:  45% (1364/3031),
/// 40.20 MiB | 12.00 MiB/s`, or `remote: Resolving deltas: 100%
/// (800/800), done.` — read into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProgress {
    /// Git's own name for the phase: `Writing objects`.
    pub phase: String,
    /// The phase runs at the other end (`remote: `) — in the VM.
    pub remote: bool,
    pub percent: Option<u8>,
    /// Done of total, where git counts.
    pub count: Option<(u64, u64)>,
    /// How much has crossed so far, as git words it (`40.20 MiB`); the
    /// rate is left off, since it changes at every print.
    pub bytes: Option<String>,
    /// The phase's last print.
    pub done: bool,
}

impl GitProgress {
    /// The line read as a progress print, or `None` for anything else —
    /// `Total …`, `Delta compression using …`, an error.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.replace("\x1b[K", "");
        let line = line.trim();
        let (remote, line) = match line.strip_prefix("remote:") {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let (phase, rest) = line.split_once(':')?;
        if phase.is_empty() || !phase.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') {
            return None;
        }
        let rest = rest.trim();
        let done = rest.ends_with("done.");
        let percent = rest
            .split_once('%')
            .and_then(|(n, _)| n.trim().parse::<u8>().ok());
        let count = rest
            .split_once('(')
            .and_then(|(_, after)| after.split_once(')'))
            .and_then(|(inside, _)| inside.split_once('/'))
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)));
        let bytes = rest
            .split_once("), ")
            .map(|(_, after)| after.split('|').next().unwrap_or(after))
            .map(|b| {
                b.trim_end_matches("done.")
                    .trim()
                    .trim_end_matches(',')
                    .trim()
            })
            .filter(|b| b.ends_with("iB") || b.ends_with(" bytes"))
            .map(str::to_string);
        // A phase with none of these is not progress: `Enumerating
        // objects: 3031, done.` has its done, anything without is words.
        if percent.is_none() && !done {
            return None;
        }
        Some(Self {
            phase: phase.to_string(),
            remote,
            percent,
            count,
            bytes,
            done,
        })
    }
}

impl GitProgress {
    /// The print as a person reads it: `sending objects, 45% (1364 of
    /// 3031), 40.20 MiB`. The phase in plain words, the measures git gave,
    /// and nothing that changes faster than the percentage does.
    pub fn words(&self) -> String {
        let phase = match (self.remote, self.phase.as_str()) {
            (false, "Enumerating objects" | "Counting objects") => "counting objects".to_string(),
            (false, "Writing objects") => "sending objects".to_string(),
            (true, "Resolving deltas") => "indexing them in the VM".to_string(),
            (true, "Unpacking objects") => "unpacking them in the VM".to_string(),
            (true, other) => format!("{} in the VM", other.to_lowercase()),
            (false, other) => other.to_lowercase(),
        };
        let mut out = phase;
        if let Some(percent) = self.percent {
            out.push_str(&format!(", {percent}%"));
        }
        if let Some((done, total)) = self.count.filter(|(_, total)| *total > 0) {
            out.push_str(&format!(" ({done} of {total})"));
        }
        if let Some(bytes) = &self.bytes {
            out.push_str(&format!(", {bytes}"));
        }
        if self.done {
            out.push_str(", done");
        }
        out
    }
}

/// Paces progress prints to what a person can read: a new phase and a
/// phase's end at once, and otherwise one a second — a percentage that
/// moves every frame is flicker, not information.
#[derive(Debug, Default)]
pub struct ProgressPace {
    last: Option<std::time::Instant>,
    phase: Option<(bool, String)>,
}

impl ProgressPace {
    pub fn due(&mut self, p: &GitProgress) -> bool {
        let phase = (p.remote, p.phase.clone());
        let now = std::time::Instant::now();
        let due = p.done
            || self.phase.as_ref() != Some(&phase)
            || self
                .last
                .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_secs(1));
        if due {
            self.last = Some(now);
            self.phase = Some(phase);
        }
        due
    }
}

/// [`git`] for a transfer: stdout discarded, stderr read as it is
/// written, each progress print (git redraws with `\r`) handed to
/// `progress`, and the rest kept for the error.
fn git_streaming(
    peer: &Path,
    keys: &Keys,
    args: &[String],
    progress: &mut dyn FnMut(&GitProgress),
) -> Result<()> {
    use std::io::Read;
    let mut argv = taste_git::private::cli_prefix(peer);
    argv.extend(args.iter().cloned());
    let (program, argv) = host_argv(taste_core::podman::sandboxed(), "git", argv);
    let mut child = std::process::Command::new(&program)
        .args(&argv)
        .env("GIT_SSH_COMMAND", keys.git_ssh_command())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let mut stderr = child.stderr.take().context("git's stderr")?;
    let mut said: Vec<String> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut take = |segment: &[u8], said: &mut Vec<String>| {
        let text = String::from_utf8_lossy(segment);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match GitProgress::parse(text) {
            Some(p) => progress(&p),
            None => {
                if said.len() >= 40 {
                    said.remove(0);
                }
                said.push(text.to_string());
            }
        }
    };
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading git's stderr"),
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(at) = pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
            let segment: Vec<u8> = pending.drain(..=at).collect();
            take(&segment, &mut said);
        }
    }
    take(&pending, &mut said);
    let status = child
        .wait()
        .with_context(|| format!("waiting for {program}"))?;
    if !status.success() {
        bail!(
            "git {}: {}",
            args.first().map(String::as_str).unwrap_or_default(),
            said.join("\n")
        );
    }
    Ok(())
}

/// Fetch the checkout's refs at `path` in `vm` into the peer. The peer's
/// HEAD is parked on an unborn branch (`taste_git::strip_worktree`), so
/// every branch may be updated.
pub fn fetch_from_guest(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    path: &Path,
    refspecs: &[&str],
) -> Result<()> {
    // Never into submodules: git would fetch a commit made in one from the
    // submodule's own remote, which does not have it, and fail the whole
    // fetch. A submodule is synced as a pair of its own (`sync_submodules`).
    let mut args = vec![
        "fetch".to_string(),
        "--quiet".into(),
        "--update-head-ok".into(),
        "--no-recurse-submodules".into(),
        guest_url(vm, path),
    ];
    args.extend(refspecs.iter().map(|s| s.to_string()));
    fetch_within_floor(peer, keys, &args)
        .with_context(|| format!("fetching from {} in {}", path.display(), vm.domain))?;
    Ok(())
}

/// Why `bytes` more on the disk holding `dir` would take it under the
/// desktop's floor (`taste_core::environment::MIN_FREE_DISK_BYTES`), or
/// `None` when they fit. A disk that will not say how full it is is let
/// through, as the IDE's other free-space checks do.
pub fn room_for(dir: &Path, bytes: u64, doing: &str) -> Option<String> {
    let floor = taste_core::environment::MIN_FREE_DISK_BYTES;
    let free = taste_core::environment::free_bytes(dir)?;
    if free.saturating_sub(bytes) >= floor {
        return None;
    }
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    Some(format!(
        "{doing} needs {:.1} GiB and this disk has {:.1} GiB free; the IDE keeps {:.0} GiB \
         free for the desktop, so nothing was written — free some space and it continues",
        gib(bytes),
        gib(free),
        gib(floor)
    ))
}

/// A fetch from a checkout, which is the agent's: it decides how many bytes
/// come back, and they land in this host's repository before anything can
/// look at them. So the fetch is refused on a disk already under the floor,
/// and stopped — asked to stop, and killed if it will not — the moment the
/// disk crosses it, its half-received pack removed after (review,
/// 2026-09-23). Polled rather than capped: a pack's size is not known until
/// it has arrived, and the floor is the quantity that actually matters.
fn fetch_within_floor(peer: &Path, keys: &Keys, args: &[String]) -> Result<()> {
    if let Some(reason) = room_for(peer, 0, "fetching from the VM") {
        bail!("{reason}");
    }
    let mut argv = taste_git::private::cli_prefix(peer);
    argv.extend(args.iter().cloned());
    let (program, argv) = host_argv(taste_core::podman::sandboxed(), "git", argv);
    let started = std::time::SystemTime::now();
    let mut child = std::process::Command::new(&program)
        .args(&argv)
        .env("GIT_SSH_COMMAND", keys.git_ssh_command())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    // Read as it comes, so a chatty stderr cannot fill the pipe and stall
    // the fetch the loop below is waiting on.
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
            text
        })
    });
    let mut stopped = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if stopped.is_none() {
            if let Some(reason) = room_for(peer, 0, "fetching from the VM") {
                // SIGTERM first: under the Flatpak the child is
                // `flatpak-spawn`, which forwards it to the host's git and
                // would leave that git running if killed outright.
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                stopped = Some((reason, std::time::Instant::now()));
            }
        } else if stopped
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > std::time::Duration::from_secs(5))
        {
            let _ = child.kill();
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    let stderr = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if let Some((reason, _)) = stopped {
        if let Some(git) = taste_git::GitWorkspace::discover(peer) {
            remove_partial_packs(git.git_dir(), started);
        }
        bail!("{reason}");
    }
    if !status.success() {
        bail!("git fetch: {}", stderr.trim());
    }
    Ok(())
}

/// The packs a stopped fetch was receiving (`objects/pack/tmp_pack_*`)
/// made since it started; nothing older, which is not this fetch's.
fn remove_partial_packs(git_dir: &Path, since: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(git_dir.join("objects/pack")) else {
        return;
    };
    for entry in entries.flatten() {
        let partial = entry.file_name().to_string_lossy().starts_with("tmp_");
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| at >= since);
        if partial && recent {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The namespace a peer keeps its checkout's branches under while they
/// are compared with its own: `refs/taste/vm/<branch>`.
pub const VM_BRANCH_NAMESPACE: &str = "refs/taste/vm/";

/// The refspecs that bring a checkout's state home to a peer that is the
/// user's own folder: branches into the comparison namespace (the folder
/// has branches of its own), tags into one of their own, and of the IDE's
/// refs only Personal's snapshot, which is what the mirror reads
/// ([`VM_SNAPSHOT_NAMESPACE`]).
///
/// Nothing else of `refs/taste` comes from the VM, because nothing else of
/// it is the checkout's: the backlog (`refs/taste/issues`) is filed in the
/// folder, the other environments' snapshots come from their own VMs, and
/// the mirror's baseline is the folder's alone. This fetched
/// `+refs/taste/*` once, which put the checkout's seed-time copy of the
/// backlog over the folder's on every sync — issues filed since, gone —
/// and let the agent in the checkout rewrite any of them with plain git
/// (review, 2026-09-23). Tags land apart and only a tag the folder does not
/// have is made from one (`adopt_tags`): forced into `refs/tags`, the
/// checkout could replace a release tag of the user's.
pub const PRIMARY_SYNC_REFSPECS: [&str; 3] = [
    "+refs/heads/*:refs/taste/vm/*",
    "+refs/tags/*:refs/taste/vm-tags/*",
    "+refs/taste/snapshot/*:refs/taste/vm-snapshot/*",
];

/// The remote every other environment's checkout has for Personal: its
/// branches, as the folder last fetched them (`refs/taste/vm/*`), pushed
/// in by the IDE whenever they move ([`share_personal`]), so a plain `git
/// pull` there takes what was committed in Personal. The checkout reaches
/// nothing to fetch from — no host, no other VM — so the remote is the
/// checkout itself (`url = .`), its fetch mapping the IDE-filled
/// `refs/remotes/personal/*` onto themselves: `git status` counts against
/// Personal before anyone fetches, and `git pull` merges what is there
/// (David, 2026-10-04: "When I commit something to my personal env, I'd
/// like to be able to easily pull it into a secondary/agent env with a
/// simple pull").
pub const PERSONAL_REMOTE: &str = "personal";

/// Where [`share_personal`] puts Personal's branches in a checkout. One
/// destination: a push writes a source ref to one of them only.
pub const PERSONAL_SHARE_REFSPECS: [&str; 1] = ["+refs/taste/vm/*:refs/remotes/personal/*"];

/// Run in an environment's checkout after [`PERSONAL_SHARE_REFSPECS`] have
/// been pushed: the `personal` remote, and — when the branch checked out
/// has no upstream of its own and Personal has a branch of that name, which
/// is how an environment starts (`EnvironmentRegistry::create_on`) — that
/// branch as its upstream. An upstream the user or the agent set is kept.
pub const PERSONAL_REMOTE_SCRIPT: &str = r#"set -e
git config remote.personal.url .
git config remote.personal.fetch '+refs/remotes/personal/*:refs/remotes/personal/*'
branch=$(git symbolic-ref --short -q HEAD) || exit 0
git config "branch.$branch.remote" >/dev/null && exit 0
git rev-parse -q --verify "refs/remotes/personal/$branch" >/dev/null || exit 0
git config "branch.$branch.remote" personal
git config "branch.$branch.merge" "refs/remotes/personal/$branch"
"#;

/// Push Personal's branches from the folder (`folder`, the primary's peer)
/// into the checkout at `path` in `vm`, pruning what Personal no longer
/// has, then set its `personal` remote up ([`PERSONAL_REMOTE_SCRIPT`]).
pub fn share_personal(
    folder: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
) -> Result<()> {
    let mut args = vec!["push".to_string(), "--prune".into(), guest_url(vm, path)];
    args.extend(PERSONAL_SHARE_REFSPECS.iter().map(|s| s.to_string()));
    git_streaming(folder, keys, &args, &mut |_| {})
        .with_context(|| format!("giving {} Personal's branches", path.display()))?;
    let out = files.exec(
        path,
        &["sh".into(), "-c".into(), PERSONAL_REMOTE_SCRIPT.into()],
    )?;
    if !out.success() {
        bail!(
            "setting up the personal remote in {}: {}",
            path.display(),
            out.stderr_utf8().trim()
        );
    }
    Ok(())
}

/// Where a submodule's repository is handed to Personal's checkout:
/// inside its `.git`, so it is no part of the working tree, by the
/// submodule's name.
const SUBMODULE_STAGING: &str = ".git/taste/submodules";

/// Run in Personal's checkout for one submodule (name, path, staging,
/// mode, head), once the folder's clone of it has been pushed to the
/// staging repository: cloned from there into its path and absorbed into
/// the checkout's `.git` as `git submodule update` would leave it, then
/// put where the folder's clone is — `branch:<name>` on that branch, else
/// detached at the folder's HEAD — so the two start out agreeing and the
/// mirror carries every change after ([`sync_submodules`]). Nothing over
/// the network, no credential; the URL is the checkout's own
/// (`.git/config`), never `.gitmodules`, which is the project's. A
/// submodule already there is moved only by `meet`, at the two's first
/// meeting ([`share_submodules`]), and only when it holds no work of its
/// own (exit 4 when it does); after that, where it stands is the mirror's
/// to follow. With `check` as the mode, it only asks: 0 when the
/// submodule is there, 1 when it is not — which is how a sync that
/// changes nothing pushes nothing.
pub const SUBMODULE_SCRIPT: &str = r#"set -e
name="$1"; path="$2"; staging="$3"; mode="$4"; head="$5"
if [ -e "$path/.git" ]; then
  [ "$mode" = meet ] || exit 0
  [ -z "$(git -C "$path" status --porcelain)" ] || exit 4
  git -C "$path" fetch -q "$staging" "+refs/heads/*:refs/remotes/origin/*"
else
  [ "$mode" = check ] && exit 1
  rmdir "$path" 2>/dev/null || true
  git clone -q --no-checkout "$staging" "$path"
  git config "submodule.$name.url" "$staging"
  git submodule --quiet absorbgitdirs -- "$path"
fi
case "$head" in
  branch:*) b="${head#branch:}"; git -C "$path" checkout -q -B "$b" "origin/$b" ;;
  *) git -C "$path" checkout -q --detach origin/taste-folder-head ;;
esac
"#;

/// Give Personal's checkout the submodules the user has checked out in the
/// folder (David, 2026-10-05: "When I cloned into my folder (to basically
/// manually handle the submodule checkout), it doesn't seem to sync to the
/// VM workspace"). The mirror carries files and leaves a submodule's
/// pointer alone, so its contents never arrived; and nothing in the VM can
/// fetch it, holding none of the user's keys. So each one goes as a
/// repository: the folder's clone pushed, with the workspace's identity,
/// into a staging repository in the checkout's `.git`, and the checkout's
/// own `git submodule update` run against it — which writes the relative
/// paths git uses, so the container and the files service read one
/// submodule. A submodule not checked out in the folder is left alone.
fn share_submodules(
    git: &taste_git::GitWorkspace,
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
) -> Result<()> {
    for (name, sub) in git.submodules() {
        let clone = peer.join(&sub);
        if !clone.join(".git").exists() {
            continue;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("the submodule name {name:?} is not one this can stage");
        }
        let staging = path.join(SUBMODULE_STAGING).join(format!("{name}.git"));
        let folder_git = taste_git::GitWorkspace::discover(&clone);
        let head = match folder_git.as_ref().and_then(|g| g.branch_name()) {
            Some(branch) => format!("branch:{branch}"),
            None => "detached".to_string(),
        };
        let script = |mode: &str| {
            files.exec(
                path,
                &[
                    "sh".into(),
                    "-c".into(),
                    SUBMODULE_SCRIPT.into(),
                    "taste-submodule".into(),
                    name.clone(),
                    sub.display().to_string(),
                    staging.display().to_string(),
                    mode.into(),
                    head.clone(),
                ],
            )
        };
        // There already: left as it is, except at the two's first meeting
        // — before the submodule's mirror has recorded anything — when
        // Personal's copy is not where the folder's clone is. One placed
        // before the pair existed was put at the parent's recorded
        // commit, detached, and the mirror would otherwise follow THAT
        // and take the folder's clone off its branch.
        let present = script("check")?.status == 0;
        if present {
            let first_meeting = folder_git
                .as_ref()
                .is_some_and(|g| matches!(g.read_ref(taste_git::mirror::MIRROR_REF), Ok(None)));
            let here = folder_git.as_ref().and_then(|g| g.checked_out());
            let there = files
                .read_to_string(&checkout_git_dir(files, &path.join(&sub)).join("HEAD"))
                .ok()
                .and_then(|h| taste_git::mirror::Head::parse(&h));
            if !first_meeting || here.is_none() || here == there {
                continue;
            }
        }
        let made = files.exec(
            path,
            &[
                "git".into(),
                "init".into(),
                "-q".into(),
                "--bare".into(),
                staging.display().to_string(),
            ],
        )?;
        if !made.success() {
            bail!(
                "making {}: {}",
                staging.display(),
                made.stderr_utf8().trim()
            );
        }
        let args: Vec<String> = [
            "push",
            "--force",
            "--quiet",
            &guest_url(vm, &staging),
            "+refs/heads/*:refs/heads/*",
            "+refs/remotes/origin/*:refs/heads/origin/*",
            "+refs/tags/*:refs/tags/*",
            "+HEAD:refs/heads/taste-folder-head",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        git_streaming(&clone, keys, &args, &mut |_| {})
            .with_context(|| format!("pushing the folder's {} to Personal", sub.display()))?;
        let out = script(if present { "meet" } else { "update" })?;
        match out.status {
            0 => {}
            4 => tracing::info!(
                "Personal's {} has uncommitted work; it stays where it is",
                sub.display()
            ),
            _ => bail!(
                "checking out {} in Personal: {}",
                sub.display(),
                out.stderr_utf8().trim()
            ),
        }
    }
    Ok(())
}

/// The ref Personal's detached HEAD is pinned to in the checkout, so a
/// fetch can bring its commit, and where it lands in the folder.
const CHECKOUT_HEAD_PIN: &str = "refs/taste/head";
const VM_HEAD: &str = "refs/taste/vm-head";

/// Where the checkout's snapshots land; only Personal's is taken from here
/// (`adopt_snapshot`). A glob, because a fetch naming one ref fails
/// outright on a checkout that has not been snapshotted yet.
const VM_SNAPSHOT_NAMESPACE: &str = "refs/taste/vm-snapshot/";

/// Where the checkout's tags land in the folder before `adopt_tags`.
const VM_TAG_NAMESPACE: &str = "refs/taste/vm-tags/";

/// Where the folder's branch lands in the checkout while the checkout
/// fast-forwards to it (`push_ahead_into_checkout`); deleted after.
const PEER_STAGING: &str = "refs/taste/peer";

/// The refspecs that seed a checkout in the VM from the user's folder:
/// branches, tags, the IDE's refs (never the peer's own comparison
/// namespace), and the remote-tracking refs the sync flow rebases onto
/// over there.
pub const PRIMARY_SEED_REFSPECS: [&str; 8] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/taste/*:refs/taste/*",
    "^refs/taste/vm/*",
    "^refs/taste/vm-tags/*",
    "^refs/taste/vm-snapshot/*",
    "^refs/taste/mirror/*",
    "+refs/remotes/*:refs/remotes/*",
];

/// The peer's remote-tracking refs, for a checkout about to rebase onto
/// one: fetched on this host with the user's keys, then pushed over.
pub const REMOTES_REFSPEC: &str = "+refs/remotes/*:refs/remotes/*";

/// What syncing the primary's peer with its checkout did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerSync {
    pub branch: Option<String>,
    /// The folder's working tree was fast-forwarded to the checkout's
    /// branch.
    pub fast_forwarded: bool,
    /// The folder's branch was ahead of the checkout's and was pushed in.
    pub pushed: bool,
    /// Commits the folder has that the checkout does not, still.
    pub host_ahead: usize,
    /// Commits the checkout has that the folder's working tree does not,
    /// still — because the folder was dirty, or on another branch.
    pub host_behind: usize,
    /// Why the folder was left where it was, when it was.
    pub note: Option<String>,
    /// The folder was made the checkout's mirror (`taste_git::mirror`):
    /// its branch, index, and working tree now match the checkout's.
    pub mirrored: bool,
    /// The folder's own changes, written into the checkout this pass: the
    /// checkout has to be snapshotted and synced again for them to come
    /// back as agreement.
    pub sent: usize,
    /// Paths changed on both sides since the last mirror, to different
    /// content: nothing was written, and the user is asked.
    pub conflicts: Vec<PathBuf>,
    /// The checkout's changes written into the folder this pass.
    pub received: usize,
    /// The folder was switched onto the checkout's branch this pass.
    pub switched: bool,
}

/// Bring the user's folder — the primary's peer — up to date with the
/// checkout in the VM, and the checkout up to date with the folder.
///
/// The checkout is the working copy of record; the folder is a peer
/// (docs/ENVIRONMENTS.md → "The topology"). Its branches are fetched into
/// `refs/taste/vm/` and compared: a branch that is not checked out is
/// simply moved to what the checkout has; the checked-out one is
/// **fast-forwarded when the folder is clean** and left alone with a note
/// otherwise (David, 2026-09-20: fast-forward it when clean). A folder
/// whose branch is AHEAD — the user committed here with another tool — is
/// pushed into the checkout, which `receive.denyCurrentBranch=updateInstead`
/// lets land when the checkout's tree is clean. Two sides that both moved
/// are an ordinary divergence: named, and left for the person.
pub fn sync_primary_peer(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
) -> Result<PeerSync> {
    sync_primary_peer_with(peer, vm, keys, files, path, false, &|_, _, _| {})
}

/// Told of each of the folder's changes as it is sent to the checkout:
/// how many are done, of how many, and which is next.
pub type OnSend<'a> = &'a dyn Fn(usize, usize, &Path);

/// [`sync_primary_peer`], with `force` to write the checkout's side over
/// paths both sides changed — the user's answer to a conflict.
///
/// Then the folder is made the checkout's MIRROR (`taste_git::mirror`):
/// HEAD onto the checkout's branch, and its working tree onto the
/// checkout's latest snapshot, both ways — the folder's own changes go to
/// the checkout first, and a path both changed is a conflict, asked about
/// (David, 2026-09-23: "I want Taste to keep my local checkout/working
/// copy updated from the VM data"; "If there isn't a conflict, I want the
/// sync to be two-way").
pub fn sync_primary_peer_with(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    force: bool,
    on_send: OnSend,
) -> Result<PeerSync> {
    use taste_git::mirror::Head;
    // What Personal's HEAD names, from its own file. A detached one is
    // pinned to a ref first, so the fetch brings its commit: a commit no
    // branch holds is otherwise one the folder never receives.
    let checkout_head = files
        .read_to_string(&checkout_git_dir(files, path).join("HEAD"))
        .ok()
        .and_then(|head| Head::parse(&head));
    let detached = matches!(checkout_head, Some(Head::Commit(_)));
    if detached {
        let pinned = files.exec(
            path,
            &[
                "git".into(),
                "update-ref".into(),
                CHECKOUT_HEAD_PIN.into(),
                "HEAD".into(),
            ],
        )?;
        if !pinned.success() {
            bail!(
                "pinning Personal's detached HEAD: {}",
                pinned.stderr_utf8().trim()
            );
        }
    }
    fetch_from_guest(peer, vm, keys, path, &PRIMARY_SYNC_REFSPECS)?;
    if detached {
        fetch_from_guest(
            peer,
            vm,
            keys,
            path,
            &[&format!("+{CHECKOUT_HEAD_PIN}:{VM_HEAD}")],
        )?;
    }
    let git = taste_git::GitWorkspace::discover(peer)
        .with_context(|| format!("{} is not a git working tree", peer.display()))?;
    // A folder whose branch has no commit yet is still on that branch: the
    // name HEAD gives it is the one to follow, or its first commit is taken
    // for a branch checked out elsewhere and only the ref moves.
    let unborn = git.unborn_branch();
    let mut sync = PeerSync {
        branch: git.branch_name().or_else(|| unborn.clone()),
        ..PeerSync::default()
    };
    // The branch the checkout is on, when it is on one.
    let checkout_branch = match &checkout_head {
        Some(Head::Branch(branch)) => Some(branch.clone()),
        _ => None,
    };
    // Whether commits moved between the two this pass, or could not: the
    // mirror waits for a pass where the branches agree.
    let mut commits_unsettled = false;
    let clean = git.status().map(|s| s.is_empty()).unwrap_or(false);
    for (name, oid) in git.refs_under(VM_BRANCH_NAMESPACE)? {
        let branch = &name[VM_BRANCH_NAMESPACE.len()..];
        let local = format!("refs/heads/{branch}");
        let mine = git.read_ref(&local)?;
        // The folder's branch, with no commit yet: its first arrives whole —
        // files, index, and branch together — rather than as a moved ref
        // over a folder that then reads as having deleted every file in it.
        if mine.is_none() && unborn.as_deref() == Some(branch) {
            git.adopt_first_commit(branch, oid)
                .with_context(|| format!("bringing {branch}'s first commit into the folder"))?;
            sync.fast_forwarded = true;
            continue;
        }
        if Some(branch) != sync.branch.as_deref() {
            // Not checked out here: the checkout moves it, but only
            // FORWARD. Forced, as it was, the checkout could reset a branch
            // of the user's holding commits it never had — unpushed work on
            // main, gone to the reflog — and a later push from this folder
            // would publish what the agent wrote under the user's keys
            // (review, 2026-09-23). The environments' published branches
            // are the IDE's own, rewritten on purpose by a forced publish,
            // and keep following the checkout.
            match mine {
                None => git.set_ref(&local, oid)?,
                Some(mine) if mine == oid => {}
                Some(_) => {
                    let (ahead, behind) = git.ahead_behind(&local, &name)?;
                    // Personal's own branch, moved here while the folder
                    // was on another — committed to, or rebased by a tool
                    // that restacks several branches — goes to Personal:
                    // forward always, a rewrite only while Personal's copy
                    // is where the two last agreed.
                    let personal = checkout_branch.as_deref() == Some(branch);
                    let agreed = git.mirror_recorded_tip(branch)?;
                    let rewrite = personal && ahead > 0 && behind > 0 && agreed == Some(oid);
                    if personal && ahead > 0 && (behind == 0 || rewrite) {
                        commits_unsettled = true;
                        let expected = rewrite.then(|| oid.to_string());
                        match push_ahead_into_checkout(
                            peer,
                            vm,
                            keys,
                            files,
                            path,
                            branch,
                            Arrival {
                                folder_tree: None,
                                rewrite_from: expected.as_deref(),
                            },
                        ) {
                            Ok(_) => sync.pushed = true,
                            Err(e) => {
                                sync.note = Some(format!(
                                    "this folder's {branch} would not go to the checkout in the \
                                     VM: {e:#}"
                                ))
                            }
                        }
                    } else if (ahead == 0 && behind > 0) || branch.starts_with("agents/") {
                        git.set_ref(&local, oid)?;
                    } else if git.moved_only_by_ide(&local)? {
                        // Rewritten in Personal — an agent's rebase — and
                        // the folder's copy is the one the IDE last put
                        // there, holding nothing of the user's: it follows,
                        // as a branch checked out here would (2026-10-05).
                        git.set_ref(&local, oid)?;
                    } else if ahead > 0 {
                        sync.note = Some(format!(
                            "this folder's {branch} has {ahead} commit(s) the checkout in the VM \
                             does not; it was left as it is"
                        ));
                    }
                }
            }
            continue;
        }
        if mine == Some(oid) {
            continue;
        }
        let (ahead, behind) = git.ahead_behind(&local, &name)?;
        match (ahead, behind) {
            // Behind: the mirror below moves the branch, the index, and the
            // working tree together, when the checkout is on it.
            (0, behind) if behind > 0 && checkout_branch.is_some() => {}
            (0, behind) if behind > 0 => {
                if clean {
                    git.fast_forward_branch(&name)
                        .with_context(|| format!("fast-forwarding {branch}"))?;
                    sync.fast_forwarded = true;
                } else {
                    sync.host_behind = behind;
                    sync.note = Some(format!(
                        "this folder has uncommitted changes, so it was not fast-forwarded to \
                         the {behind} commit(s) the checkout in the VM has"
                    ));
                }
            }
            (ahead, 0) if ahead > 0 => {
                commits_unsettled = true;
                let folder_tree = working_tree_id(peer);
                match push_ahead_into_checkout(
                    peer,
                    vm,
                    keys,
                    files,
                    path,
                    branch,
                    Arrival {
                        folder_tree: folder_tree.as_deref(),
                        rewrite_from: None,
                    },
                ) {
                    Ok(None) => sync.pushed = true,
                    // Taken, but its uncommitted work did not come along:
                    // said as what happened, not as a refusal.
                    Ok(Some(kept)) => {
                        sync.pushed = true;
                        sync.note = Some(format!(
                            "the checkout in the VM took this folder's {ahead} commit(s); {kept}"
                        ));
                    }
                    Err(e) => {
                        sync.host_ahead = ahead;
                        sync.note = Some(format!(
                            "this folder is {ahead} commit(s) ahead of the checkout in the VM, \
                             which would not take them: {e:#}"
                        ));
                    }
                }
            }
            // Diverged, with the folder where the mirror last left it: the
            // branch was rewritten in the checkout — an agent's rebase —
            // and the folder has made nothing of its own since, so the
            // mirror below moves it to the rewrite as it would to a
            // fast-forward (David, 2026-10-02: a finished rebase in the VM
            // sat as "diverged: 9 commit(s) here, 8282 there" while the
            // folder had not moved). The old tip stays in the reflog.
            _ if checkout_branch.is_some()
                && (git.unmoved_since_mirror(branch)? || git.moved_only_by_ide(&local)?) => {}
            // Diverged the other way: the folder rewrote the branch and the
            // checkout has not moved since the two last agreed, so the
            // rewrite goes there — the same rule, from this side.
            (ahead, _)
                if checkout_branch.as_deref() == Some(branch)
                    && git.mirror_recorded_tip(branch)? == Some(oid) =>
            {
                commits_unsettled = true;
                let folder_tree = working_tree_id(peer);
                match push_ahead_into_checkout(
                    peer,
                    vm,
                    keys,
                    files,
                    path,
                    branch,
                    Arrival {
                        folder_tree: folder_tree.as_deref(),
                        rewrite_from: Some(&oid.to_string()),
                    },
                ) {
                    Ok(None) => sync.pushed = true,
                    Ok(Some(kept)) => {
                        sync.pushed = true;
                        sync.note = Some(format!(
                            "the checkout in the VM took this folder's rewritten {branch}; {kept}"
                        ));
                    }
                    Err(e) => {
                        sync.host_ahead = ahead;
                        sync.note = Some(format!(
                            "this folder rewrote {branch}, and the checkout in the VM would not \
                             take it: {e:#}"
                        ));
                    }
                }
            }
            (ahead, behind) => {
                commits_unsettled = true;
                sync.host_ahead = ahead;
                sync.host_behind = behind;
                sync.note = Some(format!(
                    "this folder and the checkout in the VM have diverged on {branch}: {ahead} \
                     commit(s) here, {behind} there"
                ));
            }
        }
    }
    // The branch each side is on, by the same rule as everything else:
    // compared with the branch the two last agreed on, the side that moved
    // is followed by the other. Personal switching is followed by the
    // mirror below; the folder switching — `git switch`, or a tool that
    // rebases a stack and leaves HEAD on its top — is followed here, by
    // Personal (David, 2026-10-02: "If I do something on one side, do it
    // on the other. The only exception is a conflict"). Both having moved
    // is that conflict, and is said rather than settled.
    let follow = match (
        git.checked_out(),
        checkout_head,
        git.mirror_recorded_head()?,
    ) {
        (Some(here), Some(there), Some(agreed)) if here != there && here != agreed => {
            if there == agreed {
                match switch_checkout(peer, vm, keys, files, path, &here, &there) {
                    Ok(()) => {
                        match &here {
                            Head::Branch(branch) => git.record_switch(branch)?,
                            Head::Commit(oid) => git.record_switch_detached(*oid)?,
                        }
                        commits_unsettled = true;
                        sync.pushed = true;
                        Some(here)
                    }
                    Err(e) => {
                        sync.note = Some(format!(
                            "this folder moved to {}, and Personal could not follow from {}: \
                             {e:#}",
                            here.describe(),
                            there.describe()
                        ));
                        Some(there)
                    }
                }
            } else {
                sync.note = Some(format!(
                    "this folder moved to {} and Personal to {} since they last agreed on {}; \
                     move either to where the other is",
                    here.describe(),
                    there.describe(),
                    agreed.describe()
                ));
                Some(there)
            }
        }
        (_, there, _) => there,
    };
    adopt_tags(&git)?;
    adopt_snapshot(&git)?;
    // Submodules the user checked out in the folder reach Personal's
    // checkout as repositories; a failure is said, and the sync goes on.
    if let Err(e) = share_submodules(&git, peer, vm, keys, files, path) {
        tracing::warn!("submodules not given to Personal: {e:#}");
        sync.note
            .get_or_insert_with(|| format!("submodules not given to Personal: {e:#}"));
    }
    // Then each submodule, as a pair of its own, after the parent: the
    // parent's mirror leaves a submodule's contents alone.
    let mirrored_parent = matches!((&follow, commits_unsettled), (Some(_), false));
    if let (Some(head), false) = (follow, commits_unsettled) {
        // The folder the user opened, and nothing wider: a folder inside
        // another repository (a dotfiles repository in the home directory)
        // discovers THAT one, and the mirror would write the checkout's
        // tree over its whole working tree (review, 2026-09-23).
        let same = git.workdir().canonicalize().ok() == peer.canonicalize().ok();
        if !same {
            sync.note = Some(format!(
                "{} is inside the repository at {}; the folder is not mirrored",
                peer.display(),
                git.workdir().display()
            ));
        } else {
            mirror_into_folder(&git, &head, files, path, force, on_send, &mut sync)?;
        }
    }
    if mirrored_parent {
        sync_submodules(&git, peer, vm, keys, files, path, force, on_send, &mut sync);
    }
    Ok(sync)
}

/// Each submodule checked out on both sides, synced as the parent is —
/// snapshotted in the checkout, fetched, and mirrored both ways, its own
/// submodules within it — so a change inside one reaches the folder as
/// surely as a change in the parent does, and the other way (David,
/// 2026-10-05: "A submodule should sync its contents to my local machine
/// just as much as any change in the base git project"). What each one
/// came to is folded into `sync`, its paths under the submodule's; a
/// failure is said and the next submodule goes on.
#[allow(clippy::too_many_arguments)]
fn sync_submodules(
    git: &taste_git::GitWorkspace,
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    force: bool,
    on_send: OnSend,
    sync: &mut PeerSync,
) {
    for (_, sub) in git.submodules() {
        let clone = peer.join(&sub);
        let there = path.join(&sub);
        if !clone.join(".git").exists() || !files.exists(&there.join(".git")) {
            continue;
        }
        // What the folder's clone fetched, given to Personal's copy, as the
        // parent's remote-tracking refs are: nothing in the VM can fetch.
        if let Err(e) = share_submodule_remotes(&clone, vm, keys, &there) {
            tracing::warn!(
                "giving Personal the submodule {}'s fetched refs: {e:#}",
                sub.display()
            );
        }
        let prefixed = |done: usize, of: usize, p: &Path| on_send(done, of, &sub.join(p));
        let inner = snapshot_in_checkout(files, &there).and_then(|()| {
            sync_primary_peer_with(&clone, vm, keys, files, &there, force, &prefixed)
        });
        match inner {
            Ok(inner) => {
                sync.sent += inner.sent;
                sync.received += inner.received;
                sync.conflicts
                    .extend(inner.conflicts.into_iter().map(|p| sub.join(p)));
                if let Some(note) = inner.note {
                    sync.note.get_or_insert_with(|| {
                        format!("in the submodule {}: {note}", sub.display())
                    });
                }
            }
            Err(e) => {
                tracing::warn!("syncing the submodule {}: {e:#}", sub.display());
                sync.note.get_or_insert_with(|| {
                    format!("the submodule {} did not sync: {e:#}", sub.display())
                });
            }
        }
    }
}

/// The git directory of the checkout at `path`: its `.git`, or where a
/// `.git` file sends it — which is how a submodule's is kept, inside the
/// parent's `.git/modules/`.
fn checkout_git_dir(files: &Files, path: &Path) -> PathBuf {
    let dot_git = path.join(".git");
    match files.read_to_string(&dot_git) {
        Ok(text) => match text.trim().strip_prefix("gitdir: ") {
            Some(dir) => path.join(dir),
            None => dot_git,
        },
        Err(_) => dot_git,
    }
}

/// The folder's clone of a submodule's remote-tracking refs — what a
/// fetch there brought — pushed into Personal's copy at `there`, as
/// [`REMOTES_REFSPEC`] gives the parent's; nothing in the VM can fetch,
/// holding none of the user's keys (David, 2026-10-05: "I did a git fetch
/// for the submodule, but the refs aren't available to the container").
/// Pushed only when they have changed since the last push, which a mark in
/// the clone's own git directory remembers, so a sync that moves no ref
/// opens no connection for it.
fn share_submodule_remotes(clone: &Path, vm: &Vm, keys: &Keys, there: &Path) -> Result<()> {
    use std::hash::{Hash, Hasher};
    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(clone)
            .args(args)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let refs = git(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/remotes",
    ])?;
    if refs.is_empty() {
        return Ok(());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (refs.as_str(), there).hash(&mut hasher);
    let print = format!("{:016x}", hasher.finish());
    let mark =
        PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?).join("taste/remotes-shared");
    if std::fs::read_to_string(&mark).ok().as_deref() == Some(print.as_str()) {
        return Ok(());
    }
    push_to_guest(clone, vm, keys, there, &[REMOTES_REFSPEC])?;
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&mark, print);
    Ok(())
}

/// [`share_submodule_remotes`] for every submodule checked out on both
/// sides — what the IDE's own Fetch does after fetching the parent.
pub fn share_submodules_remotes(peer: &Path, vm: &Vm, keys: &Keys, files: &Files, path: &Path) {
    let Some(git) = taste_git::GitWorkspace::discover(peer) else {
        return;
    };
    for (_, sub) in git.submodules() {
        let clone = peer.join(&sub);
        let there = path.join(&sub);
        if !clone.join(".git").exists() || !files.exists(&there.join(".git")) {
            continue;
        }
        if let Err(e) = share_submodule_remotes(&clone, vm, keys, &there) {
            tracing::warn!(
                "giving Personal the submodule {}'s fetched refs: {e:#}",
                sub.display()
            );
        }
    }
}

/// How many of a copy's files are read from the checkout at once.
const COPY_READERS: usize = 4;

/// The ignored files at or under each of `rels` in the checkout at `path`,
/// read through `files`, copied to the same paths under `peer` — the
/// folder the user opened (David, 2026-10-05: "Let me right-click on an
/// ignored file to copy it back to my local machine. Also let me do it
/// with folders"). Part of a sync pass: the mirror never carries an
/// ignored file, and this is the user asking for some (David: "Queued
/// ignore copies should be tracked as part of the sync system").
///
/// Git in the checkout says which they are (`ls-files --others
/// --ignored`), the same question for a file and for a folder: a file
/// that is not ignored is not listed, and a folder gives up only what it
/// ignores. Read [`COPY_READERS`] at a time; each written in place, its
/// executable bit kept — being ignored, nothing else reads it on the way.
/// `on_copied` is told each as it lands. How many were copied, their
/// bytes, and the asked-for paths that held nothing ignored.
pub fn copy_ignored_home(
    files: &Files,
    path: &Path,
    peer: &Path,
    rels: &[PathBuf],
    on_copied: &(dyn Fn(usize, usize, &Path) + Sync),
) -> Result<(usize, u64, Vec<PathBuf>)> {
    use std::os::unix::fs::PermissionsExt;
    let mut wanted: Vec<PathBuf> = Vec::new();
    let mut empty: Vec<PathBuf> = Vec::new();
    for rel in rels {
        let listed = files.exec(
            path,
            &[
                "git".into(),
                "-c".into(),
                "core.quotePath=false".into(),
                "ls-files".into(),
                "--others".into(),
                "--ignored".into(),
                "--exclude-standard".into(),
                "-z".into(),
                "--".into(),
                rel.display().to_string(),
            ],
        )?;
        if !listed.success() {
            bail!(
                "listing what is ignored under {}: {}",
                rel.display(),
                listed.stderr_utf8().trim()
            );
        }
        let found: Vec<PathBuf> = listed
            .stdout_utf8()
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .filter(|p| {
                p.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
            })
            .collect();
        if found.is_empty() {
            empty.push(rel.clone());
        }
        for p in found {
            if !wanted.contains(&p) {
                wanted.push(p);
            }
        }
    }
    let total = wanted.len();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let landed = std::sync::atomic::AtomicUsize::new(0);
    let bytes = std::sync::atomic::AtomicU64::new(0);
    let failed: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..COPY_READERS.min(total) {
            scope.spawn(|| loop {
                if failed.lock().unwrap().is_some() {
                    return;
                }
                let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(rel) = wanted.get(index) else {
                    return;
                };
                let copied = (|| -> Result<u64> {
                    let from = path.join(rel);
                    let content = files
                        .read(&from)
                        .with_context(|| format!("reading {}", rel.display()))?;
                    let to = peer.join(rel);
                    if let Some(parent) = to.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("making {}", parent.display()))?;
                    }
                    std::fs::write(&to, &content)
                        .with_context(|| format!("writing {}", to.display()))?;
                    if files.stat(&from).is_ok_and(|stat| stat.mode & 0o111 != 0) {
                        let _ =
                            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755));
                    }
                    Ok(content.len() as u64)
                })();
                match copied {
                    Ok(size) => {
                        bytes.fetch_add(size, std::sync::atomic::Ordering::SeqCst);
                        let done = landed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        on_copied(done, total, rel);
                    }
                    Err(e) => {
                        failed.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    Ok((landed.into_inner(), bytes.into_inner(), empty))
}

/// Personal's snapshot of the working copy at `path`, taken there.
fn snapshot_in_checkout(files: &Files, path: &Path) -> Result<()> {
    let script = taste_git::snapshot::script(&taste_git::snapshot_ref("primary"))?;
    let out = files.exec(path, &["sh".into(), "-c".into(), script])?;
    if !out.success() {
        bail!(
            "snapshotting {}: {}",
            path.display(),
            out.stderr_utf8().trim()
        );
    }
    Ok(())
}

/// The folder's side of a conflict, inside each submodule: every change
/// the folder's clone made since the two last agreed, sent to the
/// checkout's copy and snapshotted there, nested submodules too — the
/// parent's half is the caller's.
pub fn send_submodules_folder_side(
    git: &taste_git::GitWorkspace,
    peer: &Path,
    files: &Files,
    path: &Path,
) -> Result<()> {
    for (_, sub) in git.submodules() {
        let clone = peer.join(&sub);
        let there = path.join(&sub);
        if !clone.join(".git").exists() || !files.exists(&there.join(".git")) {
            continue;
        }
        let Some(inner) = taste_git::GitWorkspace::discover(&clone) else {
            continue;
        };
        let changes = inner.folder_changes()?;
        for change in &changes {
            send_change(files, &there, change)?;
        }
        inner.record_sent(&changes)?;
        snapshot_in_checkout(files, &there)?;
        send_submodules_folder_side(&inner, &clone, files, &there)?;
    }
    Ok(())
}

/// Personal's snapshot, from where the fetch put it to where the mirror
/// reads it; every other snapshot the checkout holds stays where it landed.
fn adopt_snapshot(git: &taste_git::GitWorkspace) -> Result<()> {
    let name = taste_git::snapshot_ref("primary");
    let fetched = format!("{VM_SNAPSHOT_NAMESPACE}primary");
    if let Some(oid) = git.read_ref(&fetched)? {
        if git.read_ref(&name)? != Some(oid) {
            git.set_ref(&name, oid)?;
        }
    }
    Ok(())
}

/// The checkout's tags the folder does not have, made in the folder; one
/// it has already is left as the folder has it, whatever the checkout says.
fn adopt_tags(git: &taste_git::GitWorkspace) -> Result<()> {
    for (name, oid) in git.refs_under(VM_TAG_NAMESPACE)? {
        let tag = format!("refs/tags/{}", &name[VM_TAG_NAMESPACE.len()..]);
        if git.read_ref(&tag)?.is_none() {
            git.set_ref(&tag, oid)?;
        }
    }
    Ok(())
}

/// The mirror step of [`sync_primary_peer_with`]: the folder onto the
/// checkout's `branch` and latest snapshot, the folder's own changes
/// written into the checkout first.
fn mirror_into_folder(
    git: &taste_git::GitWorkspace,
    head: &taste_git::mirror::Head,
    files: &Files,
    path: &Path,
    force: bool,
    on_send: OnSend,
    sync: &mut PeerSync,
) -> Result<()> {
    use taste_git::mirror::Head;
    let branch = match head {
        Head::Branch(branch) => branch.as_str(),
        // Detached: the folder goes to the commit, which the fetch brought
        // pinned (`CHECKOUT_HEAD_PIN`), with no branch to move.
        Head::Commit(tip) => {
            if git.read_ref(VM_HEAD)? != Some(*tip) {
                return Ok(());
            }
            let Some(snapshot) = git.read_ref(&taste_git::snapshot_ref("primary"))? else {
                return Ok(());
            };
            let folder = git.workdir().to_path_buf();
            let room =
                move |bytes: u64| room_for(&folder, bytes, "bringing the checkout's changes in");
            let outcome = git
                .mirror_detached_within(*tip, snapshot, force, &room)
                .context("mirroring the checkout into this folder")?;
            return settle_mirror(git, outcome, None, files, path, on_send, sync);
        }
    };
    let vm_ref = format!("{VM_BRANCH_NAMESPACE}{branch}");
    let Some(tip) = git.read_ref(&vm_ref)? else {
        return Ok(());
    };
    // The mirror moves the folder's copy of the branch to the checkout's
    // tip; a copy holding commits the checkout lacks is not the mirror's to
    // move, whichever branch the folder has checked out.
    let local = format!("refs/heads/{branch}");
    if let Some(mine) = git.read_ref(&local)? {
        // ...unless the folder made none of them: a branch rewritten in the
        // checkout leaves the folder's old commits "ahead" of it, and they
        // are the checkout's own, from before the rebase.
        if mine != tip && !git.unmoved_since_mirror(branch)? && !git.moved_only_by_ide(&local)? {
            let (ahead, _) = git.ahead_behind(&local, &vm_ref)?;
            if ahead > 0 {
                sync.note = Some(format!(
                    "this folder's {branch} has {ahead} commit(s) the checkout in the VM does \
                     not; the folder follows again once they agree"
                ));
                return Ok(());
            }
        }
    }
    let Some(snapshot) = git.read_ref(&taste_git::snapshot_ref("primary"))? else {
        return Ok(());
    };
    let folder = git.workdir().to_path_buf();
    let room = move |bytes: u64| room_for(&folder, bytes, "bringing the checkout's changes in");
    let outcome = git
        .mirror_from_within(branch, tip, snapshot, force, &room)
        .context("mirroring the checkout into this folder")?;
    settle_mirror(git, outcome, Some(branch), files, path, on_send, sync)
}

/// What a mirror pass came to, said in `sync` — and the folder's own
/// changes it found, sent to the checkout. `branch` is the branch the
/// folder is now on, `None` for a detached HEAD.
fn settle_mirror(
    git: &taste_git::GitWorkspace,
    outcome: taste_git::mirror::Mirror,
    branch: Option<&str>,
    files: &Files,
    path: &Path,
    on_send: OnSend,
    sync: &mut PeerSync,
) -> Result<()> {
    use taste_git::mirror::Mirror;
    match outcome {
        Mirror::Applied { changed, switched } => {
            sync.received = changed;
            sync.switched = switched;
            sync.mirrored = true;
            sync.branch = branch.map(str::to_string);
            sync.note = None;
        }
        Mirror::Unchanged => sync.mirrored = true,
        Mirror::Stale => {}
        Mirror::Paused { reason } => sync.note = Some(reason),
        Mirror::Outgoing { changes } => {
            let mut sent = Vec::new();
            let mut failed = None;
            for (done, change) in changes.iter().enumerate() {
                on_send(done, changes.len(), &change.path);
                match send_change(files, path, change) {
                    Ok(()) => sent.push(change.clone()),
                    Err(e) => {
                        failed = Some(
                            e.context(format!("sending {} to the checkout", change.path.display())),
                        );
                        break;
                    }
                }
            }
            // What did go is recorded as sent even when a later path
            // failed, so the next pass does not mistake it for the
            // checkout's own change.
            if !sent.is_empty() {
                git.record_sent(&sent)?;
            }
            if let Some(e) = failed {
                return Err(e);
            }
            sync.sent = changes.len();
        }
        Mirror::Drift { paths } => {
            sync.note = Some(format!(
                "{} path(s) changed both here and in the checkout in the VM since they last \
                 agreed; the folder was left as it is",
                paths.len()
            ));
            sync.conflicts = paths;
        }
    }
    Ok(())
}

/// One of the folder's changes, written into the checkout at `path` over
/// the files service: the bytes, the executable bit, a link as a link, a
/// deletion as a deletion.
pub fn send_change(files: &Files, path: &Path, change: &taste_git::mirror::Change) -> Result<()> {
    let target = path.join(&change.path);
    match &change.content {
        None => match files.remove(&target, false) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        },
        Some(bytes) if change.link => {
            let link_target = String::from_utf8_lossy(bytes).to_string();
            let out = files.exec(
                path,
                &[
                    "ln".into(),
                    "-sfn".into(),
                    "--".into(),
                    link_target,
                    target.display().to_string(),
                ],
            )?;
            if !out.success() {
                bail!("linking: {}", out.stderr_utf8().trim());
            }
        }
        Some(bytes) => {
            if let Some(parent) = target.parent() {
                files.mkdir_all(parent)?;
            }
            files.write(&target, bytes)?;
            let out = files.exec(
                path,
                &[
                    "chmod".into(),
                    if change.executable { "+x" } else { "-x" }.into(),
                    "--".into(),
                    target.display().to_string(),
                ],
            )?;
            if !out.success() {
                bail!("setting its mode: {}", out.stderr_utf8().trim());
            }
        }
    }
    Ok(())
}

/// The checkout's side of [`push_ahead_into_checkout`], run in the
/// checkout with `$1` the staging ref, `$2` the branch, `$3` the folder's
/// working-tree id (empty when it could not be read), and `$4` — for a
/// branch the folder REWROTE, a rebase or an amend — the tip both sides
/// last agreed on, which the checkout's branch must still be at. Exits 0
/// moved, 4 moved with the checkout's own changes kept in a stash,
/// anything else refused with the reason on stderr.
const SYNC_SCRIPT: &str = r#"set -e
staging="$1"
branch="$2"
folder_tree="$3"
expected="$4"
sent="${5:-}"
cleanup() { git update-ref -d "$staging" 2>/dev/null || true; }
# Every taste-ide-sync stash whose every file, as stashed, is the file as
# some commit since the stash's base has it: work that reached history, so
# the stash only holds a copy of it. Dropped, newest index last so the
# indices below stay put.
prune_superseded() {
  n=$(git stash list | wc -l)
  i=$((n - 1))
  while [ "$i" -ge 0 ]; do
    s="stash@{$i}"
    if git log -1 --format=%gs -g "$s" 2>/dev/null | grep -q 'taste-ide-sync$'; then
      base=$(git rev-parse "$s^1")
      if superseded "$s" "$base"; then git stash drop -q "$s"; fi
    fi
    i=$((i - 1))
  done
}
superseded() {
  s="$1"; base="$2"
  { git diff --name-only "$s^1" "$s"
    git rev-parse -q --verify "$s^3" >/dev/null && git ls-tree -r --name-only "$s^3"
  } | while IFS= read -r p; do
    if git rev-parse -q --verify "$s^3:$p" >/dev/null 2>&1; then want=$(git rev-parse "$s^3:$p")
    else want=$(git rev-parse -q --verify "$s:$p" 2>/dev/null || echo none); fi
    found=0
    for c in $(git rev-list "$base..HEAD" -- "$p"); do
      got=$(git rev-parse -q --verify "$c:$p" 2>/dev/null || echo none)
      if [ "$got" = "$want" ]; then found=1; break; fi
    done
    [ "$found" = 1 ] || exit 1
  done
}
if [ -n "$expected" ]; then
  # A rewrite is taken only from where the two last agreed: a checkout that
  # has moved since holds work of its own, and this would drop it.
  if [ "$(git rev-parse -q --verify "refs/heads/$branch")" != "$expected" ]; then
    cleanup; echo "the checkout's $branch has moved since it last agreed with the folder" >&2; exit 2
  fi
elif ! git merge-base --is-ancestor "refs/heads/$branch" "$staging"; then
  cleanup; echo "the checkout's $branch is not behind the folder's" >&2; exit 2
fi
if [ "$(git symbolic-ref --short -q HEAD)" != "$branch" ]; then
  if [ -n "$expected" ]; then git update-ref "refs/heads/$branch" "$staging" "$expected"
  else git update-ref "refs/heads/$branch" "$staging"; fi
  cleanup; exit 0
fi
if git ls-files -u | grep -q .; then
  if git stash list | grep -q taste-ide-sync && ! git rev-parse -q --verify MERGE_HEAD >/dev/null 2>&1; then
    # The leftovers of a taste-ide-sync stash that did not apply, from
    # before the sync learned to leave the tree clean: the stash holds the
    # changes, so the tree goes back to its commit.
    git reset -q --hard
  else
    cleanup
    echo "the checkout has unresolved conflicts in $(git ls-files -u | awk '{print $4}' | sort -u | tr '\n' ' ')- resolve or discard them in the file tree, and the folder's commits follow" >&2
    exit 5
  fi
fi
# A file the checkout still holds as the folder sent it, where the folder
# has since changed it again, is the folder's stale copy and not Personal's
# work: it goes back to its commit, and the folder's version comes with the
# move or the mirror after it. Carried through the stash below instead, an
# edit the folder had reverted came back out over the revert, which left
# the file as it was before the edit and so took the stash cleanly
# (2026-10-04: bootstrap.sh "modified" in Personal, clean in the folder).
# `$sent` is "sent<TAB>now<TAB>path" a line, objects by id ("none": absent).
if [ -n "$sent" ]; then
  tab=$(printf '\t')
  printf '%s\n' "$sent" | while IFS="$tab" read -r was now p; do
    [ -n "$p" ] && [ "$was" != "$now" ] || continue
    [ -L "$p" ] && continue
    if [ -e "$p" ]; then here=$(git hash-object -- "$p"); else here=none; fi
    [ "$here" = "$was" ] || continue
    if git cat-file -e "HEAD:$p" 2>/dev/null; then git checkout -q HEAD -- "$p"
    else rm -f -- "$p"; fi
  done
fi
# The checkout's files are the folder's: move the branch, keep the files.
if [ -n "$folder_tree" ]; then
  index=$(mktemp)
  GIT_INDEX_FILE="$index" git read-tree HEAD
  GIT_INDEX_FILE="$index" git add -A
  mine=$(GIT_INDEX_FILE="$index" git write-tree)
  rm -f "$index"
  if [ "$mine" = "$folder_tree" ] && { [ -n "$expected" ] || git merge-base --is-ancestor HEAD "$staging"; }; then
    git reset -q "$staging"
    cleanup
    prune_superseded
    exit 0
  fi
fi
stashed=0
if [ -n "$(git status --porcelain)" ]; then
  git stash push --include-untracked -q -m taste-ide-sync && stashed=1
fi
if [ -n "$expected" ]; then
  # The tree is clean here (its changes are in the stash), so this moves
  # the branch and its files and loses nothing.
  git reset -q --hard "$staging"
elif ! git merge --ff-only -q "$staging"; then
  [ "$stashed" = 1 ] && git stash pop -q || true
  cleanup; echo "the checkout would not fast-forward to the folder's $branch" >&2; exit 3
fi
cleanup
if [ "$stashed" = 1 ] && ! git stash pop -q 2>/dev/null; then
  git reset -q --hard
  git clean -fdq
  prune_superseded
  if git stash list | grep -q 'taste-ide-sync$'; then
    echo "its uncommitted changes do not apply over them and are kept in its stash (taste-ide-sync), for the file tree's Unstash" >&2
    exit 4
  fi
  exit 0
fi
prune_superseded
"#;

/// The checkout's side of [`switch_checkout`], run in the checkout with
/// `$1` the staging ref holding the folder's branch, `$2` that branch, and
/// `$3` the branch the two last agreed on, which the checkout must still be
/// on. Personal's own uncommitted changes, or its own commits on the branch
/// it is switching to, are work this would drop: refused, with the reason.
const SWITCH_SCRIPT: &str = r#"set -e
staging="$1"
branch="$2"
from="$3"
cleanup() { git update-ref -d "$staging" 2>/dev/null || true; }
# Where Personal stands, as the folder records it: a branch's name, or the
# commit its HEAD is detached at.
here=$(git symbolic-ref --short -q HEAD || git rev-parse -q --verify HEAD)
if [ "$here" != "$from" ]; then
  cleanup; echo "Personal has moved off $from since it last agreed with the folder" >&2; exit 2
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
  cleanup; echo "Personal has uncommitted changes on $from" >&2; exit 3
fi
# Not a branch: the folder is detached at the commit, and so is Personal.
if [ -z "$branch" ]; then
  if ! git switch -q --detach "$staging" 2>&1; then
    cleanup; exit 5
  fi
  cleanup; exit 0
fi
if git rev-parse -q --verify "refs/heads/$branch" >/dev/null; then
  if ! git merge-base --is-ancestor "refs/heads/$branch" "$staging"; then
    cleanup; echo "Personal's $branch has commits the folder's does not" >&2; exit 4
  fi
fi
git update-ref "refs/heads/$branch" "$staging"
if ! git switch -q "$branch" 2>&1; then
  cleanup; exit 5
fi
cleanup
"#;

/// Move Personal's checkout to `to`, where the folder is, from `from`,
/// where the two last agreed — as `git checkout <commit-ish>` would: onto
/// the folder's branch, its commits first, or detached at the folder's
/// commit. Refused rather than forced over work Personal has of its own
/// ([`SWITCH_SCRIPT`]).
fn switch_checkout(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    to: &taste_git::mirror::Head,
    from: &taste_git::mirror::Head,
) -> Result<()> {
    use taste_git::mirror::Head;
    let (staging, source, branch) = match to {
        Head::Branch(branch) => (
            format!("{PEER_STAGING}/{branch}"),
            format!("refs/heads/{branch}"),
            branch.clone(),
        ),
        Head::Commit(oid) => (
            format!("{PEER_STAGING}/HEAD"),
            oid.to_string(),
            String::new(),
        ),
    };
    push_to_guest(peer, vm, keys, path, &[&format!("+{source}:{staging}")])?;
    let out = files
        .exec(
            path,
            &[
                "sh".into(),
                "-c".into(),
                SWITCH_SCRIPT.to_string(),
                "taste-switch".into(),
                staging,
                branch,
                from.commitish(),
            ],
        )
        .with_context(|| format!("moving Personal to {} in VM {}", to.describe(), vm.domain))?;
    if !out.success() {
        let stderr = out.stderr_utf8();
        let stdout = out.stdout_utf8();
        let reason = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("{}", reason.trim());
    }
    Ok(())
}

/// The tree the folder's working files come to — tracked and untracked,
/// less what is ignored — as git names it, built in a throwaway index so
/// the folder's own is not touched. Content-addressed, so the checkout in
/// the VM computing the same id over the same files means the two trees
/// hold the same work. `None` for a folder git cannot read that way, which
/// costs only the shortcut it feeds.
fn working_tree_id(peer: &Path) -> Option<String> {
    if taste_git::private::find_private(peer).is_some() {
        return None;
    }
    let index = std::env::temp_dir().join(format!(
        "taste-worktree-{}-{:x}.index",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(peer)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
    };
    let tree = git(&["read-tree", "HEAD"])
        .and_then(|_| git(&["add", "-A"]))
        .and_then(|_| git(&["write-tree"]))
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|tree| !tree.is_empty());
    let _ = std::fs::remove_file(&index);
    tree
}

/// Bring the checkout in the VM up to the folder's `branch`, which is
/// ahead of it: the user committed here with another tool.
///
/// **Most of the time nothing needs stashing.** The folder mirrors the
/// checkout, so at launch the checkout's working files are usually the
/// folder's exactly, and the commits the folder is ahead by are commits
/// of that very work — made here, on the host, while the checkout carried
/// it uncommitted. Stashing it around the fast-forward then left a stash
/// that could not apply over its own commits, one per relaunch: a pile of
/// "stashed" files that were all cruft (David, 2026-09-28). So when the
/// checkout's working tree is the folder's (`folder_tree`, the same id
/// computed on both sides), the branch simply moves to the folder's tip
/// with the files left as they are — which leaves exactly the folder's
/// own uncommitted changes showing, the state the mirror means. Only a
/// checkout whose files differ from the folder's takes the stash below.
///
/// A plain push into the checked-out branch is what
/// `receive.denyCurrentBranch=updateInstead` allows only when the
/// checkout's working tree is clean, and a checkout in use is seldom
/// clean — so the first launch after nine host-side commits refused with
/// "would not take them" (2026-09-21). Instead the branch is pushed to a
/// staging ref and the checkout fast-forwards to it ITSELF, its
/// uncommitted work stashed around the move and put back after, the way
/// `git pull --autostash` does. A stash that does not apply over the new
/// commits is kept — and the half-applied tree it would have left is put
/// back to the new commits, clean, so the checkout is not found strewn
/// with conflict markers nobody asked for (2026-09-21: a Conflicts row
/// and a stash both, from one sync); `Ok(Some(..))` says so, for the
/// note. A tree already carrying such leftovers from an earlier sync is
/// cleaned the same way first; a conflict of the user's own — unmerged
/// paths with no such stash, or a merge in progress — is refused by name,
/// with what to do. A branch that is not a fast-forward is left alone, an
/// error.
/// What a branch arriving in the checkout brings with it: the folder's
/// working-tree id, for the move that keeps the checkout's files, and —
/// for a branch the folder rewrote — the tip the two last agreed on.
#[derive(Clone, Copy, Default)]
struct Arrival<'a> {
    folder_tree: Option<&'a str>,
    rewrite_from: Option<&'a str>,
}

/// What the folder has sent the checkout since the two last agreed, as the
/// sync script's `$5`: "sent<TAB>now<TAB>path" a line
/// (`GitWorkspace::sent_against_folder`). A path that would not survive
/// the format — a tab or a newline in it — is left out, and keeps the old
/// behaviour.
fn sent_lines(peer: &Path) -> String {
    let Some(git) = taste_git::GitWorkspace::discover(peer) else {
        return String::new();
    };
    let Ok(sent) = git.sent_against_folder() else {
        return String::new();
    };
    fn id(oid: Option<impl std::fmt::Display>) -> String {
        oid.map_or_else(|| "none".to_string(), |o| o.to_string())
    }
    sent.into_iter()
        .filter_map(|sent| {
            let path = sent.path.to_str()?.to_string();
            (!path.contains(['\t', '\n']))
                .then(|| format!("{}\t{}\t{path}", id(sent.sent), id(sent.now)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_ahead_into_checkout(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    branch: &str,
    arrival: Arrival,
) -> Result<Option<String>> {
    let Arrival {
        folder_tree,
        rewrite_from,
    } = arrival;
    let staging = format!("{PEER_STAGING}/{branch}");
    push_to_guest(
        peer,
        vm,
        keys,
        path,
        &[&format!("+refs/heads/{branch}:{staging}")],
    )?;
    let script = SYNC_SCRIPT.to_string();
    let out = files
        .exec(
            path,
            // The names ride as arguments, never spliced into the script: a
            // branch name may hold a quote, and this runs a shell.
            &[
                "sh".into(),
                "-c".into(),
                script,
                "taste-sync".into(),
                staging.clone(),
                branch.to_string(),
                folder_tree.unwrap_or_default().to_string(),
                rewrite_from.unwrap_or_default().to_string(),
                sent_lines(peer),
            ],
        )
        .with_context(|| format!("fast-forwarding {branch} in VM {}", vm.domain))?;
    match out.status {
        0 => Ok(None),
        4 => Ok(Some(out.stderr_utf8().trim().to_string())),
        _ => bail!("{}", out.stderr_utf8().trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder and its checkout, as `git` sees them, for the sync script.
    struct Pair {
        dir: PathBuf,
    }

    impl Pair {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("taste-sync-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("folder")).unwrap();
            let pair = Self { dir };
            pair.git("folder", &["init", "-q", "-b", "main"]);
            pair.write("folder", "a.txt", "one\n");
            pair.git("folder", &["add", "-A"]);
            pair.commit("folder", "first");
            let folder = pair.dir.join("folder").display().to_string();
            let checkout = pair.dir.join("checkout").display().to_string();
            pair.git(".", &["clone", "-q", &folder, &checkout]);
            pair
        }

        fn git(&self, side: &str, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .current_dir(self.dir.join(side))
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn commit(&self, side: &str, message: &str) {
            self.git(side, &["commit", "-q", "-m", message]);
        }

        fn write(&self, side: &str, file: &str, text: &str) {
            std::fs::write(self.dir.join(side).join(file), text).unwrap();
        }

        /// The folder's tip fetched into the checkout's staging ref, and the
        /// script run there; its exit status.
        fn sync(&self) -> i32 {
            self.sync_rewrite("")
        }

        /// [`Self::sync`] for a branch the folder rewrote, from `agreed`.
        fn sync_rewrite(&self, agreed: &str) -> i32 {
            self.sync_with(agreed, "")
        }

        /// The sync, told what the folder has sent since the two last
        /// agreed (`sent_lines`'s format).
        fn sync_with(&self, agreed: &str, sent: &str) -> i32 {
            let folder = self.dir.join("folder").display().to_string();
            self.git(
                "checkout",
                &["fetch", "-q", &folder, "+main:refs/taste/staging/main"],
            );
            let tree = working_tree_id(&self.dir.join("folder")).unwrap_or_default();
            let out = std::process::Command::new("sh")
                .current_dir(self.dir.join("checkout"))
                .args([
                    "-c",
                    SYNC_SCRIPT,
                    "taste-sync",
                    "refs/taste/staging/main",
                    "main",
                    &tree,
                    agreed,
                    sent,
                ])
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            out.status.code().unwrap_or(-1)
        }

        fn stashes(&self) -> usize {
            self.git("checkout", &["stash", "list"]).lines().count()
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn work_committed_in_the_folder_moves_the_checkout_without_a_stash() {
        let pair = Pair::new("same");
        // The checkout carries the work, mirrored, uncommitted …
        pair.write("checkout", "a.txt", "two\n");
        pair.write("checkout", "new.txt", "fresh\n");
        // … and the folder commits it, with more of the same work on top,
        // also mirrored.
        pair.write("folder", "a.txt", "two\n");
        pair.write("folder", "new.txt", "fresh\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "the work");
        pair.write("folder", "a.txt", "three\n");
        pair.write("checkout", "a.txt", "three\n");
        assert_eq!(pair.sync(), 0);
        assert_eq!(
            pair.stashes(),
            0,
            "the sync stashed work the folder committed"
        );
        assert_eq!(
            pair.git("checkout", &["rev-parse", "HEAD"]),
            pair.git("folder", &["rev-parse", "HEAD"])
        );
        // What shows as uncommitted is the folder's own uncommitted change.
        assert_eq!(pair.git("checkout", &["status", "--porcelain"]), "M a.txt");
    }

    /// An edit the folder sent, committed and then reverted in the folder
    /// before the checkout took either commit, does not come back out of
    /// the sync's stash over the revert (2026-10-04: bootstrap.sh showed as
    /// modified in Personal while the folder was clean).
    #[test]
    fn a_reverted_edit_the_folder_sent_does_not_come_back() {
        let pair = Pair::new("revert");
        let blob = |side: &str, text: &str| {
            std::fs::write(pair.dir.join(side).join(".probe"), text).unwrap();
            let id = pair.git(side, &["hash-object", ".probe"]);
            std::fs::remove_file(pair.dir.join(side).join(".probe")).unwrap();
            id
        };
        // The edit, sent: the checkout holds it uncommitted …
        pair.write("checkout", "a.txt", "two\n");
        // … the folder commits it, then reverts it, and the revert has not
        // been sent yet when the sync runs.
        pair.write("folder", "a.txt", "two\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "the edit");
        pair.write("folder", "a.txt", "one\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "revert the edit");
        let sent = format!(
            "{}\t{}\ta.txt",
            blob("folder", "two\n"),
            blob("folder", "one\n")
        );
        assert_eq!(pair.sync_with("", &sent), 0);
        assert_eq!(
            pair.git("checkout", &["rev-parse", "HEAD"]),
            pair.git("folder", &["rev-parse", "HEAD"])
        );
        assert_eq!(pair.git("checkout", &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(pair.dir.join("checkout/a.txt")).unwrap(),
            "one\n"
        );
    }

    /// Personal's own work — a file the folder never sent — still rides
    /// the stash through the move.
    #[test]
    fn personals_own_change_survives_the_move() {
        let pair = Pair::new("own");
        pair.write("checkout", "mine.txt", "agent's\n");
        pair.write("folder", "a.txt", "two\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "folder work");
        assert_eq!(pair.sync_with("", ""), 0);
        assert_eq!(
            std::fs::read_to_string(pair.dir.join("checkout/mine.txt")).unwrap(),
            "agent's\n"
        );
    }

    /// A rebase or an amend in the folder reaches the checkout while the
    /// checkout is where the two last agreed — and not once it has moved,
    /// which is a conflict the script refuses rather than settles.
    #[test]
    fn a_rewrite_in_the_folder_reaches_a_checkout_that_has_not_moved() {
        let pair = Pair::new("rewrite");
        let agreed = pair.git("checkout", &["rev-parse", "HEAD"]);
        pair.git(
            "folder",
            &["commit", "-q", "--amend", "-m", "first, reworded"],
        );
        assert_eq!(pair.sync(), 2, "a rewrite is not a fast-forward");
        assert_eq!(pair.sync_rewrite(&agreed), 0);
        assert_eq!(
            pair.git("checkout", &["rev-parse", "HEAD"]),
            pair.git("folder", &["rev-parse", "HEAD"])
        );
        assert_eq!(
            pair.git("checkout", &["log", "-1", "--format=%s"]),
            "first, reworded"
        );

        // The checkout commits on its own, and a second rewrite from the
        // old agreement is refused: both sides moved.
        let agreed = pair.git("checkout", &["rev-parse", "HEAD"]);
        pair.write("checkout", "b.txt", "mine\n");
        pair.git("checkout", &["add", "-A"]);
        pair.commit("checkout", "the checkout's own");
        pair.git("folder", &["commit", "-q", "--amend", "-m", "first, again"]);
        assert_eq!(pair.sync_rewrite(&agreed), 2);
        assert_eq!(
            pair.git("checkout", &["log", "-1", "--format=%s"]),
            "the checkout's own"
        );
    }

    /// A branch the folder switched to is the branch Personal switches to,
    /// with the folder's commits on it; Personal's own uncommitted work
    /// refuses the switch.
    #[test]
    fn a_switch_in_the_folder_switches_the_checkout() {
        let pair = Pair::new("switch");
        pair.git("folder", &["switch", "-q", "-c", "topic"]);
        pair.write("folder", "t.txt", "topic\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "on topic");
        let folder = pair.dir.join("folder").display().to_string();
        let switch = |from: &str| {
            pair.git(
                "checkout",
                &["fetch", "-q", &folder, "+topic:refs/taste/staging/topic"],
            );
            std::process::Command::new("sh")
                .current_dir(pair.dir.join("checkout"))
                .args([
                    "-c",
                    SWITCH_SCRIPT,
                    "taste-switch",
                    "refs/taste/staging/topic",
                    "topic",
                    from,
                ])
                .output()
                .unwrap()
                .status
                .code()
                .unwrap_or(-1)
        };
        pair.write("checkout", "a.txt", "personal's edit\n");
        assert_eq!(switch("main"), 3, "Personal's uncommitted work holds it");
        pair.git("checkout", &["checkout", "-q", "--", "a.txt"]);
        assert_eq!(
            switch("elsewhere"),
            2,
            "Personal moved off the agreed branch"
        );
        assert_eq!(switch("main"), 0);
        assert_eq!(
            pair.git("checkout", &["symbolic-ref", "--short", "HEAD"]),
            "topic"
        );
        assert_eq!(
            pair.git("checkout", &["rev-parse", "HEAD"]),
            pair.git("folder", &["rev-parse", "HEAD"])
        );
    }

    /// An environment's checkout, given Personal's branches the way the IDE
    /// gives them, pulls Personal's new commit with a plain `git pull`; a
    /// branch Personal no longer has goes from the remote; and an upstream
    /// set in the environment is kept.
    #[test]
    fn a_plain_pull_in_an_environment_takes_personals_commit() {
        let pair = Pair::new("personal");
        // The environment's checkout, made as `place_in_vm` makes it.
        pair.git(".", &["init", "-q", "-b", "main", "env"]);
        pair.git(
            "env",
            &["config", "receive.denyCurrentBranch", "updateInstead"],
        );
        let env = pair.dir.join("env").display().to_string();
        pair.git(
            "folder",
            &["push", "-q", &env, "+refs/heads/*:refs/heads/*"],
        );
        let share = || {
            // The folder's copy of Personal's branches, as the sync keeps it.
            let tip = pair.git("folder", &["rev-parse", "main"]);
            pair.git("folder", &["update-ref", "refs/taste/vm/main", &tip]);
            let mut args = vec!["push", "-q", "--prune", &env];
            args.extend(PERSONAL_SHARE_REFSPECS);
            pair.git("folder", &args);
            let out = std::process::Command::new("sh")
                .current_dir(pair.dir.join("env"))
                .args(["-c", PERSONAL_REMOTE_SCRIPT])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        pair.git("folder", &["update-ref", "refs/taste/vm/gone", "HEAD"]);
        share();
        assert_eq!(
            pair.git("env", &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
            "personal/main"
        );

        // Personal commits; the IDE shares; the environment pulls.
        pair.write("folder", "b.txt", "personal's\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "committed in Personal");
        pair.git("folder", &["update-ref", "-d", "refs/taste/vm/gone"]);
        share();
        assert!(pair.git("env", &["status", "-sb"]).contains("[behind 1]"));
        pair.git("env", &["pull", "-q", "--ff-only"]);
        assert_eq!(
            pair.git("env", &["log", "-1", "--format=%s"]),
            "committed in Personal"
        );
        assert_eq!(
            pair.git(
                "env",
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/remotes/personal/"
                ]
            ),
            "refs/remotes/personal/main",
            "a branch Personal no longer has is pruned"
        );

        // An upstream the environment chose stays its own.
        pair.git("env", &["config", "branch.main.remote", "elsewhere"]);
        share();
        assert_eq!(
            pair.git("env", &["config", "branch.main.remote"]),
            "elsewhere"
        );
    }

    /// A submodule the user cloned into the folder reaches Personal's
    /// checkout as a repository, on the branch the folder's clone is on,
    /// with nothing fetched over a network: the folder's clone pushed to a
    /// staging repository and cloned from there into place, absorbed as
    /// `git submodule update` would leave it, with git's own relative
    /// paths.
    #[test]
    fn a_submodule_cloned_in_the_folder_is_checked_out_in_personal() {
        let pair = Pair::new("submodule");
        // The submodule's own repository, and the parent recording it.
        pair.git(".", &["init", "-q", "-b", "main", "sub"]);
        pair.write("sub", "lib.txt", "template\n");
        pair.git("sub", &["add", "-A"]);
        pair.commit("sub", "the template");
        let sub = pair.dir.join("sub").display().to_string();
        pair.git(
            "folder",
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub,
                "tpl",
            ],
        );
        pair.commit("folder", "add the template");
        // Personal's checkout has the commit and not the submodule.
        pair.git("checkout", &["pull", "-q"]);
        assert!(!pair.dir.join("checkout/tpl/lib.txt").exists());
        // The handoff: the folder's clone into staging, then the script.
        let staging = pair.dir.join("checkout/.git/taste/submodules/tpl.git");
        pair.git(".", &["init", "-q", "--bare", staging.to_str().unwrap()]);
        pair.git(
            "folder/tpl",
            &[
                "push",
                "-q",
                "--force",
                staging.to_str().unwrap(),
                "+refs/heads/*:refs/heads/*",
            ],
        );
        let run = |mode: &str| {
            std::process::Command::new("sh")
                .current_dir(pair.dir.join("checkout"))
                .args([
                    "-c",
                    SUBMODULE_SCRIPT,
                    "taste-submodule",
                    "tpl",
                    "tpl",
                    staging.to_str().unwrap(),
                    mode,
                    "branch:main",
                ])
                .output()
                .unwrap()
        };
        assert_eq!(run("check").status.code(), Some(1));
        let out = run("update");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(pair.dir.join("checkout/tpl/lib.txt")).unwrap(),
            "template\n"
        );
        let gitfile = std::fs::read_to_string(pair.dir.join("checkout/tpl/.git")).unwrap();
        assert!(gitfile.trim().starts_with("gitdir: ../"), "{gitfile}");
        assert_eq!(
            pair.git("checkout/tpl", &["branch", "--show-current"]),
            "main"
        );
        assert_eq!(run("check").status.code(), Some(0), "now nothing to do");
        // One placed detached, as the old placement left it, meets the
        // folder's clone on its branch — unless it holds work of its own.
        pair.git("checkout/tpl", &["checkout", "-q", "--detach"]);
        pair.write("checkout/tpl", "lib.txt", "Personal's own\n");
        assert_eq!(run("meet").status.code(), Some(4));
        pair.git("checkout/tpl", &["checkout", "-q", "--", "lib.txt"]);
        let out = run("meet");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            pair.git("checkout/tpl", &["branch", "--show-current"]),
            "main"
        );
        // The project's .gitmodules is untouched; the URL is the checkout's.
        assert!(!pair
            .git("checkout", &["diff", "--name-only"])
            .contains(".gitmodules"));
    }

    /// Changes inside a submodule travel as the parent's do: one made in
    /// Personal's copy reaches the folder's clone, one made in the folder's
    /// clone reaches Personal's, and so does a commit — the whole sync, run
    /// against a stand-in ssh that runs git's far end on this machine.
    #[test]
    fn a_submodule_syncs_both_ways_as_the_parent_does() {
        let pair = Pair::new("subsync");
        pair.git(".", &["init", "-q", "-b", "main", "sub"]);
        pair.write("sub", "lib.txt", "template\n");
        pair.git("sub", &["add", "-A"]);
        pair.commit("sub", "the template");
        let sub = pair.dir.join("sub").display().to_string();
        pair.git(
            "folder",
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub,
                "tpl",
            ],
        );
        pair.commit("folder", "add the template");
        pair.git("checkout", &["pull", "-q"]);
        pair.git(
            "checkout",
            &["config", "receive.denyCurrentBranch", "updateInstead"],
        );
        let ssh = pair.dir.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor last; do :; done\nexec sh -c \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let keys = Keys::at(pair.dir.join("keys")).with_ssh_for_tests(&ssh);
        let vm = Vm {
            domain: "test".into(),
            ssh_port: 22,
            workspace_root: pair.dir.join("folder"),
            state: crate::provision::DomainState::Running,
            cloud: None,
        };
        let files = Files::Local;
        let folder = pair.dir.join("folder");
        let checkout = pair.dir.join("checkout");
        let sync = |step: &str| {
            snapshot_in_checkout(&files, &checkout).unwrap();
            sync_primary_peer_with(&folder, &vm, &keys, &files, &checkout, false, &|_, _, _| {})
                .unwrap_or_else(|e| panic!("{step}: {e:#}"))
        };
        let read = |p: &str| std::fs::read_to_string(pair.dir.join(p)).unwrap_or_default();

        let first = sync("pass 1");
        assert_eq!(read("checkout/tpl/lib.txt"), "template\n", "{first:?}");
        assert_eq!(
            pair.git("checkout/tpl", &["branch", "--show-current"]),
            "main"
        );

        // Personal edits inside the submodule; the folder's clone follows.
        pair.write("checkout/tpl", "lib.txt", "changed in Personal\n");
        let pass = sync("pass 2");
        assert_eq!(
            read("folder/tpl/lib.txt"),
            "changed in Personal\n",
            "{pass:?}"
        );

        // The folder's clone gains a file; Personal's copy does too.
        pair.write("folder/tpl", "new.txt", "from the folder\n");
        let pass = sync("pass 3");
        assert_eq!(
            read("checkout/tpl/new.txt"),
            "from the folder\n",
            "{pass:?}"
        );
        let pass = sync("pass 4");
        assert!(pass.conflicts.is_empty(), "{pass:?}");

        // A commit in Personal's submodule reaches the folder's clone.
        pair.git("checkout/tpl", &["add", "-A"]);
        pair.commit("checkout/tpl", "made in Personal");
        sync("pass 5");
        let pass = sync("pass 6");
        assert_eq!(
            pair.git("folder/tpl", &["rev-parse", "HEAD"]),
            pair.git("checkout/tpl", &["rev-parse", "HEAD"]),
            "{pass:?}"
        );
        assert_eq!(read("folder/tpl/new.txt"), "from the folder\n");

        // A fetch in the folder's clone: its remote-tracking refs reach
        // Personal's copy, which cannot fetch for itself.
        let head = pair.git("folder/tpl", &["rev-parse", "HEAD"]);
        pair.git(
            "folder/tpl",
            &["update-ref", "refs/remotes/origin/feature", &head],
        );
        sync("pass 7");
        assert_eq!(
            pair.git(
                "checkout/tpl",
                &["rev-parse", "refs/remotes/origin/feature"]
            ),
            head
        );
    }

    /// What a copy home brings: the ignored files under a folder, to the
    /// same paths, an executable one still executable; one ignored file
    /// alone; and nothing for a file git tracks, which is named back.
    #[test]
    fn the_ignored_files_under_a_path_are_copied_to_the_folder() {
        use std::os::unix::fs::PermissionsExt;
        let pair = Pair::new("copy-home");
        let checkout = pair.dir.join("checkout");
        let folder = pair.dir.join("folder");
        std::fs::write(checkout.join(".gitignore"), "build/\n*.log\n").unwrap();
        std::fs::create_dir_all(checkout.join("build/sub")).unwrap();
        std::fs::write(checkout.join("build/slides.pdf"), "%PDF").unwrap();
        std::fs::write(checkout.join("build/sub/run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            checkout.join("build/sub/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(checkout.join("trace.log"), "log").unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let (count, bytes, empty) = copy_ignored_home(
            &Files::Local,
            &checkout,
            &folder,
            &["build".into(), "trace.log".into(), "a.txt".into()],
            &|done, total, rel| seen.lock().unwrap().push((done, total, rel.to_path_buf())),
        )
        .unwrap();
        assert_eq!((count, bytes), (3, 17));
        assert_eq!(
            empty,
            [PathBuf::from("a.txt")],
            "tracked, so nothing to copy"
        );
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(
            std::fs::read_to_string(folder.join("build/slides.pdf")).unwrap(),
            "%PDF"
        );
        let mode = std::fs::metadata(folder.join("build/sub/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "still executable");
        assert_eq!(
            std::fs::read_to_string(folder.join("trace.log")).unwrap(),
            "log"
        );
    }

    /// A branch rewritten in Personal — an agent's rebase — follows into
    /// the folder when the folder's copy is the one the IDE last put
    /// there; one the user has committed to since stays as it is.
    #[test]
    fn a_branch_rebased_in_personal_follows_where_the_folder_holds_nothing_of_its_own() {
        let pair = Pair::new("rebased");
        pair.git(
            "checkout",
            &["config", "receive.denyCurrentBranch", "updateInstead"],
        );
        let ssh = pair.dir.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor last; do :; done\nexec sh -c \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let keys = Keys::at(pair.dir.join("keys")).with_ssh_for_tests(&ssh);
        let vm = Vm {
            domain: "test".into(),
            ssh_port: 22,
            workspace_root: pair.dir.join("folder"),
            state: crate::provision::DomainState::Running,
            cloud: None,
        };
        let files = Files::Local;
        let folder = pair.dir.join("folder");
        let checkout = pair.dir.join("checkout");
        let sync = || {
            snapshot_in_checkout(&files, &checkout).unwrap();
            sync_primary_peer_with(&folder, &vm, &keys, &files, &checkout, false, &|_, _, _| {})
                .unwrap()
        };
        // Personal makes a branch; the folder is given its copy.
        pair.git("checkout", &["branch", "side"]);
        pair.git("checkout", &["switch", "-q", "side"]);
        pair.write("checkout", "b.txt", "on the side\n");
        pair.git("checkout", &["add", "-A"]);
        pair.commit("checkout", "side work");
        pair.git("checkout", &["switch", "-q", "main"]);
        sync();
        let before = pair.git("checkout", &["rev-parse", "side"]);
        assert_eq!(pair.git("folder", &["rev-parse", "side"]), before);
        // The agent rewrites it there.
        pair.git("checkout", &["switch", "-q", "side"]);
        pair.git(
            "checkout",
            &["commit", "-q", "--amend", "-m", "side work, rebased"],
        );
        pair.git("checkout", &["switch", "-q", "main"]);
        let pass = sync();
        let after = pair.git("checkout", &["rev-parse", "side"]);
        assert_ne!(before, after);
        assert_eq!(
            pair.git("folder", &["rev-parse", "side"]),
            after,
            "the IDE's copy follows the rewrite: {pass:?}"
        );
        // The user commits to the folder's copy; the next rewrite leaves it.
        pair.git("folder", &["switch", "-q", "side"]);
        pair.write("folder", "c.txt", "mine\n");
        pair.git("folder", &["add", "c.txt"]);
        pair.commit("folder", "my own");
        let mine = pair.git("folder", &["rev-parse", "side"]);
        pair.git("folder", &["switch", "-q", "main"]);
        pair.git("checkout", &["switch", "-q", "side"]);
        pair.git(
            "checkout",
            &["commit", "-q", "--amend", "-m", "rewritten again"],
        );
        pair.git("checkout", &["switch", "-q", "main"]);
        sync();
        assert_eq!(
            pair.git("folder", &["rev-parse", "side"]),
            mine,
            "the user's commit stays"
        );
    }

    /// A folder detached at a commit — `git checkout <commit-ish>` — puts
    /// Personal there too, detached; and a switch back onto a branch starts
    /// from that commit, which is where the two then agree.
    #[test]
    fn a_detached_folder_detaches_the_checkout_and_back() {
        let pair = Pair::new("detach");
        let first = pair.git("folder", &["rev-parse", "HEAD"]);
        pair.write("folder", "a.txt", "two\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "second");
        pair.git("checkout", &["pull", "-q"]);
        pair.git("folder", &["checkout", "-q", "--detach", &first]);
        let folder = pair.dir.join("folder").display().to_string();
        let switch = |source: &str, branch: &str, from: &str| {
            pair.git(
                "checkout",
                &[
                    "fetch",
                    "-q",
                    &folder,
                    &format!("+{source}:refs/taste/staging/switch"),
                ],
            );
            std::process::Command::new("sh")
                .current_dir(pair.dir.join("checkout"))
                .args([
                    "-c",
                    SWITCH_SCRIPT,
                    "taste-switch",
                    "refs/taste/staging/switch",
                    branch,
                    from,
                ])
                .output()
                .unwrap()
                .status
                .code()
                .unwrap_or(-1)
        };
        assert_eq!(switch(&first, "", "main"), 0);
        assert_eq!(pair.git("checkout", &["rev-parse", "HEAD"]), first);
        assert!(
            std::process::Command::new("git")
                .current_dir(pair.dir.join("checkout"))
                .args(["symbolic-ref", "-q", "HEAD"])
                .status()
                .unwrap()
                .code()
                == Some(1),
            "Personal is detached, not on a branch"
        );
        assert_eq!(
            pair.git("checkout", &["rev-parse", "refs/heads/main"]),
            pair.git("folder", &["rev-parse", "refs/heads/main"]),
            "detaching leaves Personal's main where it was"
        );

        // Back onto main, from the commit the two agreed on.
        pair.git("folder", &["switch", "-q", "main"]);
        assert_eq!(switch("main", "main", "main"), 2, "Personal is not on main");
        assert_eq!(switch("main", "main", &first), 0);
        assert_eq!(
            pair.git("checkout", &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert_eq!(
            pair.git("checkout", &["rev-parse", "HEAD"]),
            pair.git("folder", &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn a_checkout_whose_work_differs_keeps_it_and_old_copies_are_pruned() {
        let pair = Pair::new("differs");
        // An earlier launch's stash, of work later committed as stashed.
        pair.write("checkout", "a.txt", "two\n");
        pair.git("checkout", &["stash", "push", "-q", "-m", "taste-ide-sync"]);
        pair.write("folder", "a.txt", "two\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "the work");
        // The checkout's own change, which the folder does not have and
        // which does not apply over the folder's next commit.
        pair.write("folder", "a.txt", "folder's\n");
        pair.git("folder", &["add", "-A"]);
        pair.commit("folder", "more");
        pair.write("checkout", "a.txt", "checkout's own\n");
        assert_eq!(pair.sync(), 4);
        // The superseded copy is gone; the checkout's own work is kept.
        assert_eq!(pair.stashes(), 1);
        assert_eq!(
            pair.git("checkout", &["show", "stash@{0}:a.txt"]),
            "checkout's own"
        );
    }

    #[test]
    fn the_guest_url_names_core_the_forward_and_the_absolute_path() {
        let vm = Vm {
            domain: "taste-a".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: crate::provision::DomainState::Running,
            cloud: None,
        };
        assert_eq!(
            guest_url(&vm, Path::new("/var/home/core/taste/799f/i-0001")),
            "ssh://core@127.0.0.1:40022/var/home/core/taste/799f/i-0001"
        );
    }

    /// The primary's refspecs keep the comparison namespace on the peer's
    /// side only: it is neither seeded into the checkout nor fetched back
    /// as an IDE ref, or a branch and its mirror would land on one name.
    #[test]
    fn the_comparison_namespace_never_crosses() {
        assert!(PRIMARY_SEED_REFSPECS.contains(&"^refs/taste/vm/*"));
        assert!(PRIMARY_SYNC_REFSPECS.contains(&"+refs/heads/*:refs/taste/vm/*"));
        // The folder's own: the mirror's baseline is never seeded into the
        // VM, and nothing of the IDE's but Personal's snapshot comes back —
        // not the backlog, not the checkout's tags over the folder's.
        assert!(PRIMARY_SEED_REFSPECS.contains(&"^refs/taste/mirror/*"));
        for spec in PRIMARY_SYNC_REFSPECS {
            let destination = spec.split(':').nth(1).unwrap();
            assert!(destination.starts_with("refs/taste/vm"), "{spec}");
        }
    }

    #[test]
    fn progress_prints_are_read_and_other_lines_are_not() {
        let p = GitProgress::parse("Writing objects:  45% (1364/3031), 40.20 MiB | 12.00 MiB/s")
            .unwrap();
        assert_eq!(p.phase, "Writing objects");
        assert!(!p.remote && !p.done);
        assert_eq!(p.percent, Some(45));
        assert_eq!(p.count, Some((1364, 3031)));
        assert_eq!(p.bytes.as_deref(), Some("40.20 MiB"));

        let p =
            GitProgress::parse("Writing objects: 100% (3031/3031), 90.01 MiB | 30.00 MiB/s, done.")
                .unwrap();
        assert!(p.done);
        assert_eq!(p.bytes.as_deref(), Some("90.01 MiB"));

        let p =
            GitProgress::parse("remote: Resolving deltas: 100% (800/800), done.\x1b[K").unwrap();
        assert!(p.remote && p.done);
        assert_eq!(p.phase, "Resolving deltas");
        assert_eq!(p.bytes, None);

        let p = GitProgress::parse("Enumerating objects: 3031, done.").unwrap();
        assert!(p.done && p.percent.is_none());

        assert_eq!(
            GitProgress::parse("Delta compression using up to 8 threads"),
            None
        );
        assert_eq!(
            GitProgress::parse("Total 3031 (delta 800), reused 0 (delta 0), pack-reused 0"),
            None
        );
        assert_eq!(
            GitProgress::parse("fatal: the remote end hung up unexpectedly"),
            None
        );
        assert_eq!(GitProgress::parse("To ssh://core@127.0.0.1:1/x"), None);
    }

    #[test]
    fn progress_is_worded_for_a_person() {
        let p = GitProgress::parse("Writing objects:  45% (1364/3031), 40.20 MiB | 12.00 MiB/s")
            .unwrap();
        assert_eq!(p.words(), "sending objects, 45% (1364 of 3031), 40.20 MiB");
        let p = GitProgress::parse("remote: Resolving deltas:  30% (240/800)").unwrap();
        assert_eq!(p.words(), "indexing them in the VM, 30% (240 of 800)");
    }

    /// A phase's first print and its last are due at once; the ones
    /// between wait their second.
    #[test]
    fn the_pace_lets_phases_through_and_holds_the_prints_between() {
        let mut pace = ProgressPace::default();
        let at = |pct: u8| {
            GitProgress::parse(&format!("Writing objects: {pct:>3}% ({pct}/100)")).unwrap()
        };
        assert!(pace.due(&at(1)));
        assert!(!pace.due(&at(2)));
        assert!(pace.due(&GitProgress::parse("Writing objects: 100% (100/100), done.").unwrap()));
        assert!(pace.due(&GitProgress::parse("remote: Resolving deltas:   0% (0/9)").unwrap()));
    }

    /// The refspecs are forced and cover branches, tags, and the IDE's own
    /// refs — which is where the snapshots are.
    #[test]
    fn the_peer_tracks_branches_tags_and_the_ides_refs() {
        assert!(PEER_REFSPECS.iter().all(|r| r.starts_with('+')));
        assert!(PEER_REFSPECS.iter().any(|r| r.contains("refs/taste/*")));
        assert!(PEER_REFSPECS.iter().any(|r| r.contains("refs/heads/*")));
    }
}
