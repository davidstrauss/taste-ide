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
            _ if checkout_branch.is_some() && git.unmoved_since_mirror(branch)? => {}
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
    let checkout_branch = match (
        sync.branch.clone(),
        checkout_branch,
        git.mirror_recorded_branch()?,
    ) {
        (Some(here), Some(there), Some(agreed)) if here != there && here != agreed => {
            if there == agreed {
                match switch_checkout(peer, vm, keys, files, path, &here, &there) {
                    Ok(()) => {
                        git.record_switch(&here)?;
                        commits_unsettled = true;
                        sync.pushed = true;
                        Some(here)
                    }
                    Err(e) => {
                        sync.note = Some(format!(
                            "this folder switched to {here}, and Personal could not follow \
                             from {there}: {e:#}"
                        ));
                        Some(there)
                    }
                }
            } else {
                sync.note = Some(format!(
                    "this folder switched to {here} and Personal to {there} since they last \
                     agreed on {agreed}; switch either to the other's branch"
                ));
                Some(there)
            }
        }
        (_, there, _) => there,
    };
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
        // ...unless the folder made none of them: a branch rewritten in the
        // checkout leaves the folder's old commits "ahead" of it, and they
        // are the checkout's own, from before the rebase.
        if mine != tip && !git.unmoved_since_mirror(branch)? {
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
if [ "$(git symbolic-ref --short -q HEAD)" != "$from" ]; then
  cleanup; echo "Personal has moved off $from since it last agreed with the folder" >&2; exit 2
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
  cleanup; echo "Personal has uncommitted changes on $from" >&2; exit 3
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

/// Switch Personal's checkout to `branch`, the folder's, from `from`, the
/// branch the two last agreed on: the folder's branch first, as it stands,
/// then the switch, both refused rather than forced over work Personal has
/// of its own ([`SWITCH_SCRIPT`]).
fn switch_checkout(
    peer: &Path,
    vm: &Vm,
    keys: &Keys,
    files: &Files,
    path: &Path,
    branch: &str,
    from: &str,
) -> Result<()> {
    let staging = format!("{PEER_STAGING}/{branch}");
    push_to_guest(
        peer,
        vm,
        keys,
        path,
        &[&format!("+refs/heads/{branch}:{staging}")],
    )?;
    let out = files
        .exec(
            path,
            &[
                "sh".into(),
                "-c".into(),
                SWITCH_SCRIPT.to_string(),
                "taste-switch".into(),
                staging,
                branch.to_string(),
                from.to_string(),
            ],
        )
        .with_context(|| format!("switching Personal to {branch} in VM {}", vm.domain))?;
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
