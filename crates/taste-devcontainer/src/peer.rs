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
    let mut args = vec![
        "fetch".to_string(),
        "--quiet".into(),
        "--update-head-ok".into(),
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
    fetch_from_guest(peer, vm, keys, path, &PRIMARY_SYNC_REFSPECS)?;
    let git = taste_git::GitWorkspace::discover(peer)
        .with_context(|| format!("{} is not a git working tree", peer.display()))?;
    let mut sync = PeerSync {
        branch: git.branch_name(),
        ..PeerSync::default()
    };
    // The branch the checkout is on, from its own HEAD: the one the
    // folder follows.
    let checkout_branch = files
        .read_to_string(&path.join(".git/HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        });
    // Whether commits moved between the two this pass, or could not: the
    // mirror waits for a pass where the branches agree.
    let mut commits_unsettled = false;
    let clean = git.status().map(|s| s.is_empty()).unwrap_or(false);
    for (name, oid) in git.refs_under(VM_BRANCH_NAMESPACE)? {
        let branch = &name[VM_BRANCH_NAMESPACE.len()..];
        let local = format!("refs/heads/{branch}");
        let mine = git.read_ref(&local)?;
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
                    if (ahead == 0 && behind > 0) || branch.starts_with("agents/") {
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
                match push_ahead_into_checkout(peer, vm, keys, files, path, branch) {
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
    adopt_tags(&git)?;
    adopt_snapshot(&git)?;
    if let (Some(branch), false) = (checkout_branch, commits_unsettled) {
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
            mirror_into_folder(&git, &branch, files, path, force, on_send, &mut sync)?;
        }
    }
    Ok(sync)
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
    branch: &str,
    files: &Files,
    path: &Path,
    force: bool,
    on_send: OnSend,
    sync: &mut PeerSync,
) -> Result<()> {
    use taste_git::mirror::Mirror;
    let vm_ref = format!("{VM_BRANCH_NAMESPACE}{branch}");
    let Some(tip) = git.read_ref(&vm_ref)? else {
        return Ok(());
    };
    // The mirror moves the folder's copy of the branch to the checkout's
    // tip; a copy holding commits the checkout lacks is not the mirror's to
    // move, whichever branch the folder has checked out.
    let local = format!("refs/heads/{branch}");
    if let Some(mine) = git.read_ref(&local)? {
        if mine != tip {
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
    match git
        .mirror_from_within(branch, tip, snapshot, force, &room)
        .context("mirroring the checkout into this folder")?
    {
        Mirror::Applied { changed, switched } => {
            sync.received = changed;
            sync.switched = switched;
            sync.mirrored = true;
            sync.branch = Some(branch.to_string());
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

/// Bring the checkout in the VM up to the folder's `branch`, which is
/// ahead of it: the user committed here with another tool.
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
fn push_ahead_into_checkout(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    branch: &str,
) -> Result<Option<String>> {
    let staging = format!("{PEER_STAGING}/{branch}");
    push_to_guest(
        peer,
        vm,
        keys,
        path,
        &[&format!("+refs/heads/{branch}:{staging}")],
    )?;
    let script = r#"set -e
staging="$1"
branch="$2"
cleanup() { git update-ref -d "$staging" 2>/dev/null || true; }
if ! git merge-base --is-ancestor "refs/heads/$branch" "$staging"; then
  cleanup; echo "the checkout's $branch is not behind the folder's" >&2; exit 2
fi
if [ "$(git symbolic-ref --short -q HEAD)" != "$branch" ]; then
  git update-ref "refs/heads/$branch" "$staging"; cleanup; exit 0
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
stashed=0
if [ -n "$(git status --porcelain)" ]; then
  git stash push --include-untracked -q -m taste-ide-sync && stashed=1
fi
if ! git merge --ff-only -q "$staging"; then
  [ "$stashed" = 1 ] && git stash pop -q || true
  cleanup; echo "the checkout would not fast-forward to the folder's $branch" >&2; exit 3
fi
cleanup
if [ "$stashed" = 1 ] && ! git stash pop -q 2>/dev/null; then
  git reset -q --hard
  git clean -fdq
  echo "its uncommitted changes do not apply over them and are kept in its stash (taste-ide-sync), for the file tree's Unstash" >&2
  exit 4
fi
"#
    .to_string();
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

    #[test]
    fn the_guest_url_names_core_the_forward_and_the_absolute_path() {
        let vm = Vm {
            domain: "taste-a".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: crate::provision::DomainState::Running,
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
