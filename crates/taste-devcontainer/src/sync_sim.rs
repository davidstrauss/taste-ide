//! The sync, simulated: whole passes of the real folder-and-Personal sync
//! (`peer::sync_primary_peer_with`, both ways) between two repositories on
//! this machine, the VM's end stood in for by an ssh that runs git's far
//! end here, driven by a seeded schedule of what people and agents do —
//! writes, removals, renames of files and directories, a file becoming a
//! directory, the executable bit, commits, and an ignored file — with those
//! edits made at every step of a pass, passes stopped partway, and writes
//! made to fail (docs/SYNC.md → "Making it hold", layer 3).
//!
//! What is checked is the specification, against what the actors did
//! rather than what the sync recorded:
//!
//! - **P1 by a ledger.** Every version an actor writes records what it
//!   replaced on its side. At the end every version ever written must be on
//!   a side, or replaced by one that is, through that chain — so a version
//!   the sync destroyed, which nothing an actor did replaced, is found. A
//!   resolved conflict records the user's choice as that replacement.
//! - **Nothing the actors did not write.** Every file on either side is a
//!   version some actor wrote: no merge markers, no mixed content.
//! - **P4.** Left quiet, with every conflict answered, the two sides hold
//!   the same files and modes, the same commit, and a further pass writes
//!   nothing.
//! - **P7.** The ignored file on each side is what that side's actor last
//!   wrote there.
//!
//! A failure prints its seed and the schedule that led to it;
//! `TASTE_SYNC_SIM_SEED=<seed>` replays that one. The default run is a few
//! seeds; `cargo test -p taste-devcontainer sim_ -- --ignored` runs
//! `TASTE_SYNC_SIM_SEEDS` of them (default 50).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use taste_core::files::Files;

use crate::keys::Keys;
use crate::peer::{sync_primary_peer_with, PeerSync, Resolve, Side};
use crate::provision::{DomainState, Vm};

/// splitmix64: small, fast, and the same sequence from the same seed on
/// every machine, which is what replaying a seed needs.
struct Rng(Cell<u64>);

impl Rng {
    fn next(&self) -> u64 {
        let mut z = self.0.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.0.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Where {
    Folder,
    Checkout,
}

impl Where {
    fn dir(self) -> &'static str {
        match self {
            Where::Folder => "folder",
            Where::Checkout => "checkout",
        }
    }
}

/// A version of a path: its bytes and executable bit, or nothing there.
type Version = Option<(String, bool)>;

/// The paths the actors touch: `a` and `a/q` so a file and a directory
/// meet; `d` and `d2` so a directory can be renamed onto a name with none.
const PATHS: [&str; 8] = ["a", "b", "a/q", "d/x", "d/y", "d/e/z", "d2/x", "c"];
const IGNORED: &str = "build.log";

/// A version as the ledger knows it: its bytes, the mode left out.
fn bytes_of(version: Version) -> Version {
    version.map(|(text, _)| (text, false))
}

/// Marks a pass stopped on purpose; raised with `resume_unwind`, which
/// skips the panic hook, so a deliberate crash prints nothing.
struct Crash;

struct World {
    root: tempfile::TempDir,
    rng: Rng,
    seed: u64,
    vm: Vm,
    keys: Keys,
    /// For each path, each version an actor wrote there and the versions it
    /// replaced.
    replaced: RefCell<HashMap<(PathBuf, Version), HashSet<Version>>>,
    /// Every (path, version) an actor wrote.
    written: RefCell<Vec<(PathBuf, Version)>>,
    /// The ignored file's last content on each side.
    ignored: RefCell<HashMap<Where, String>>,
    /// Which side has committed since the two last stood on one commit:
    /// one side at a time, since both committing is a divergence the
    /// specification leaves to the user.
    committing: Cell<Option<Where>>,
    n: Cell<usize>,
    log: RefCell<Vec<String>>,
    /// A folder directory refuses writes for this pass: a fault for the
    /// sync, so the actors stay out of the folder meanwhile.
    locked: Cell<bool>,
}

impl World {
    fn new(seed: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let ssh = root.path().join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor last; do :; done\nexec sh -c \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let keys = Keys::at(root.path().join("keys")).with_ssh_for_tests(&ssh);
        let vm = Vm {
            domain: "sim".into(),
            ssh_port: 22,
            workspace_root: root.path().join("folder"),
            state: DomainState::Running,
            cloud: None,
        };
        let world = World {
            root,
            rng: Rng(Cell::new(seed)),
            seed,
            vm,
            keys,
            replaced: RefCell::new(HashMap::new()),
            written: RefCell::new(Vec::new()),
            ignored: RefCell::new(HashMap::new()),
            committing: Cell::new(None),
            n: Cell::new(0),
            log: RefCell::new(Vec::new()),
            locked: Cell::new(false),
        };
        std::fs::create_dir_all(world.side(Where::Folder)).unwrap();
        world.git(Where::Folder, &["init", "-q", "-b", "main"]);
        world.record(
            Where::Folder,
            Path::new(".gitignore"),
            Some(("build.log\n".into(), false)),
        );
        std::fs::write(world.side(Where::Folder).join(".gitignore"), "build.log\n").unwrap();
        for path in ["a", "b", "d/x"] {
            world.write(Where::Folder, path);
        }
        world.git(Where::Folder, &["add", "-A"]);
        world.git(Where::Folder, &["commit", "-q", "-m", "start"]);
        let folder = world.side(Where::Folder).display().to_string();
        let checkout = world.side(Where::Checkout).display().to_string();
        let out = std::process::Command::new("git")
            .args(["clone", "-q", &folder, &checkout])
            .output()
            .unwrap();
        assert!(out.status.success());
        for side in [Where::Folder, Where::Checkout] {
            world.git(side, &["config", "user.name", "sim"]);
            world.git(side, &["config", "user.email", "sim@sim"]);
        }
        world.git(
            Where::Checkout,
            &["config", "receive.denyCurrentBranch", "updateInstead"],
        );
        world
    }

    fn side(&self, side: Where) -> PathBuf {
        self.root.path().join(side.dir())
    }

    fn say(&self, line: impl Into<String>) {
        self.log.borrow_mut().push(line.into());
    }

    fn git(&self, side: Where, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(self.side(side))
            .args(args)
            .env("GIT_AUTHOR_NAME", "sim")
            .env("GIT_AUTHOR_EMAIL", "sim@sim")
            .env("GIT_COMMITTER_NAME", "sim")
            .env("GIT_COMMITTER_EMAIL", "sim@sim")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            side.dir(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What `side` holds at `path`: a file's bytes and executable bit, or
    /// nothing (a directory there holds no version of the path).
    fn held(&self, side: Where, path: &Path) -> Version {
        let at = self.side(side).join(path);
        let meta = std::fs::symlink_metadata(&at).ok()?;
        if !meta.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&at).ok()?;
        Some((text, meta.permissions().mode() & 0o111 != 0))
    }

    /// An actor's act at `path` on `side`, recorded before it is done:
    /// `version` replaces what the side holds there now. The ledger knows
    /// versions by their bytes alone: git merges a change of bytes on one
    /// side with a change of mode on the other, cleanly, into a version
    /// neither wrote whole, and the mode is held to agreeing by P4.
    fn record(&self, side: Where, path: &Path, version: Version) {
        let version = bytes_of(version);
        let before = bytes_of(self.held(side, path));
        if before == version {
            return;
        }
        self.replaced
            .borrow_mut()
            .entry((path.to_path_buf(), version.clone()))
            .or_default()
            .insert(before);
        self.written
            .borrow_mut()
            .push((path.to_path_buf(), version));
    }

    /// Every file under `dir` on `side`, relative to the side's root.
    fn files_under(&self, side: Where, dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.side(side).join(dir)];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_name() == ".git" {
                    continue;
                }
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(path),
                    Ok(_) => out.push(path.strip_prefix(self.side(side)).unwrap().to_path_buf()),
                    Err(_) => {}
                }
            }
        }
        out.sort();
        out
    }

    /// Remove `path` on `side` as an actor would: a file, or a directory
    /// and everything in it, each file's removal recorded.
    fn remove(&self, side: Where, path: &Path) {
        let at = self.side(side).join(path);
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            return;
        };
        if meta.is_dir() {
            for file in self.files_under(side, path) {
                self.record(side, &file, None);
            }
            let _ = std::fs::remove_dir_all(&at);
        } else {
            self.record(side, path, None);
            let _ = std::fs::remove_file(&at);
        }
    }

    /// Make room for a file at `path` on `side`: a file where a directory
    /// on the way should be removed, and a directory where the file goes.
    fn clear_for(&self, side: Where, path: &Path) {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if std::fs::symlink_metadata(self.side(side).join(ancestor)).is_ok_and(|m| m.is_file())
            {
                self.remove(side, ancestor);
            }
        }
        if std::fs::symlink_metadata(self.side(side).join(path)).is_ok_and(|m| m.is_dir()) {
            self.remove(side, path);
        }
    }

    /// Write a new version at `path` on `side`, its bytes unique to this
    /// write.
    fn write(&self, side: Where, path: &str) {
        let path = Path::new(path);
        self.clear_for(side, path);
        let n = self.n.get() + 1;
        self.n.set(n);
        let text = format!("v{n} by the {} at {}\n", side.dir(), path.display());
        let exec = self.held(side, path).is_some_and(|(_, x)| x);
        self.record(side, path, Some((text.clone(), exec)));
        let at = self.side(side).join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, text).unwrap();
    }

    /// One thing an actor does on `side`, chosen by the seed.
    fn act(&self, side: Where) {
        let path = *self.rng.pick(&PATHS);
        match self.rng.below(10) {
            0..=3 => {
                self.say(format!("{}: write {path}", side.dir()));
                self.write(side, path);
            }
            4 => {
                self.say(format!("{}: remove {path}", side.dir()));
                self.remove(side, Path::new(path));
            }
            5 => {
                let Some((text, exec)) = self.held(side, Path::new(path)) else {
                    return;
                };
                self.say(format!("{}: chmod {path} to {}", side.dir(), !exec));
                self.record(side, Path::new(path), Some((text, !exec)));
                let at = self.side(side).join(path);
                let mode = if exec { 0o644 } else { 0o755 };
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            6 => {
                let to = *self.rng.pick(&PATHS);
                let (from, to) = (Path::new(path), Path::new(to));
                if from == to || from.starts_with(to) || to.starts_with(from) {
                    return;
                }
                let Some(version) = self.held(side, from) else {
                    return;
                };
                self.say(format!(
                    "{}: rename {} to {}",
                    side.dir(),
                    from.display(),
                    to.display()
                ));
                self.clear_for(side, to);
                self.record(side, to, Some(version));
                self.record(side, from, None);
                let target = self.side(side).join(to);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::rename(self.side(side).join(from), &target).unwrap();
            }
            7 => {
                // A directory renamed whole: one rename, its files moved.
                let (from, to) = if self.rng.chance(50) {
                    ("d", "d2")
                } else {
                    ("d2", "d")
                };
                let (src, dst) = (self.side(side).join(from), self.side(side).join(to));
                if !src.is_dir() || std::fs::symlink_metadata(&dst).is_ok() {
                    return;
                }
                self.say(format!("{}: rename directory {from} to {to}", side.dir()));
                for file in self.files_under(side, Path::new(from)) {
                    let rest = file.strip_prefix(from).unwrap();
                    let version = self.held(side, &file);
                    self.record(side, &Path::new(to).join(rest), version);
                    self.record(side, &file, None);
                }
                std::fs::rename(&src, &dst).unwrap();
            }
            8 => {
                let n = self.n.get() + 1;
                self.n.set(n);
                let text = format!("ignored {n} on the {}\n", side.dir());
                self.say(format!("{}: write {IGNORED}", side.dir()));
                std::fs::write(self.side(side).join(IGNORED), &text).unwrap();
                self.ignored.borrow_mut().insert(side, text);
            }
            _ => {
                if self.committing.get().is_some_and(|s| s != side) {
                    return;
                }
                self.git(side, &["add", "-A"]);
                if self.git(side, &["status", "--porcelain"]).is_empty() {
                    return;
                }
                self.say(format!("{}: commit", side.dir()));
                self.git(side, &["commit", "-q", "-m", "sim"]);
                self.committing.set(Some(side));
            }
        }
    }

    /// A few acts on a side the seed picks.
    fn acts(&self) {
        let side = if self.rng.chance(50) && !self.locked.get() {
            Where::Folder
        } else {
            Where::Checkout
        };
        for _ in 0..1 + self.rng.below(3) {
            self.act(side);
        }
    }

    /// One pass of the real sync. With `chaos`, the actors act at every
    /// step it reports, the pass may be stopped at any of them, and a
    /// directory in the folder may refuse writes for its length. `None`
    /// when it was stopped.
    fn pass(&self, resolve: Option<&Resolve>, chaos: bool) -> Option<PeerSync> {
        let maybe = |at: &str| {
            if !chaos {
                return;
            }
            if self.rng.chance(25) {
                self.say(format!("  during the pass, at {at}:"));
                self.acts();
            }
            if self.rng.chance(4) {
                self.say(format!("  the pass stopped at {at}"));
                std::panic::resume_unwind(Box::new(Crash));
            }
        };
        let say = |line: &str| maybe(line);
        let on_send =
            |_: usize, _: usize, path: &Path| maybe(&format!("sending {}", path.display()));
        let locked = (chaos && self.rng.chance(10))
            .then(|| self.side(Where::Folder).join(self.rng.pick(&["d", "d2"])))
            .filter(|dir| dir.is_dir());
        if let Some(dir) = &locked {
            self.say(format!("  {} refuses writes this pass", dir.display()));
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            self.locked.set(true);
        }
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sync_primary_peer_with(
                &self.side(Where::Folder),
                &self.vm,
                &self.keys,
                &Files::Local,
                &self.side(Where::Checkout),
                resolve,
                &on_send,
                &say,
            )
        }));
        // Wherever the locked directory went: an actor may have renamed it
        // during the pass.
        if locked.is_some() {
            self.locked.set(false);
            for dir in ["d", "d2"] {
                let _ = std::fs::set_permissions(
                    self.side(Where::Folder).join(dir),
                    std::fs::Permissions::from_mode(0o755),
                );
            }
        }
        match ran {
            Ok(Ok(sync)) => {
                self.say(format!(
                    "  pass: sent {} received {} missed {} conflicts {:?}{}",
                    sync.sent,
                    sync.received,
                    sync.missed,
                    sync.conflicts,
                    sync.note
                        .as_deref()
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default()
                ));
                if self.git(Where::Folder, &["rev-parse", "HEAD"])
                    == self.git(Where::Checkout, &["rev-parse", "HEAD"])
                {
                    self.committing.set(None);
                }
                Some(sync)
            }
            Ok(Err(e)) if chaos => {
                self.say(format!("  the pass failed: {e:#}"));
                None
            }
            Ok(Err(e)) => self.fail(&format!("a quiet pass failed: {e:#}")),
            Err(payload) if payload.is::<Crash>() => None,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Passes until one writes nothing; its account.
    fn settle(&self, resolve: Option<&Resolve>) -> PeerSync {
        let mut resolve = resolve;
        for _ in 0..8 {
            let sync = self.pass(resolve, false).unwrap();
            resolve = None;
            if sync.sent == 0 && sync.received == 0 && sync.missed == 0 && !sync.pushed {
                return sync;
            }
        }
        self.fail("still writing after eight quiet passes (P4)");
    }

    fn fail(&self, what: &str) -> ! {
        panic!(
            "seed {}: {what}\nreplay with TASTE_SYNC_SIM_SEED={}\n{}",
            self.seed,
            self.seed,
            self.log.borrow().join("\n")
        );
    }

    /// Every file on `side` and its version, but the repository's own and
    /// the ignored file.
    fn copy(&self, side: Where) -> BTreeMap<PathBuf, Version> {
        self.files_under(side, Path::new(""))
            .into_iter()
            .filter(|p| p != Path::new(IGNORED))
            .map(|p| {
                let v = self.held(side, &p);
                (p, v)
            })
            .collect()
    }

    /// The whole run: setup agreed, a schedule of acts and passes, then
    /// quiet, conflicts answered, and every check.
    fn run(&self, steps: usize) {
        self.settle(None);
        for _ in 0..steps {
            if self.rng.chance(55) {
                self.acts();
            } else {
                self.pass(None, true);
            }
        }
        self.say("quiet:");
        let mut sync = self.settle(None);
        for _ in 0..3 {
            if sync.conflicts.is_empty() {
                break;
            }
            let keep = if self.rng.chance(50) {
                Side::Folder
            } else {
                Side::Checkout
            };
            self.say(format!(
                "the user keeps the {keep:?}'s side of {:?}",
                sync.conflicts
            ));
            // The user's choice is the kept version replacing the other.
            for path in &sync.conflicts {
                let (kept, lost) = match keep {
                    Side::Folder => (
                        self.held(Where::Folder, path),
                        self.held(Where::Checkout, path),
                    ),
                    Side::Checkout => (
                        self.held(Where::Checkout, path),
                        self.held(Where::Folder, path),
                    ),
                };
                self.replaced
                    .borrow_mut()
                    .entry((path.clone(), bytes_of(kept)))
                    .or_default()
                    .insert(bytes_of(lost));
            }
            let resolve = Resolve {
                keep,
                paths: sync.conflicts.iter().cloned().collect(),
            };
            sync = self.settle(Some(&resolve));
        }
        self.check(&sync);
    }

    /// Every version in every commit `side`'s repository holds: what is
    /// committed is not lost, whatever the working copies hold.
    fn committed(&self, side: Where) -> Vec<(PathBuf, Version)> {
        let mut out = Vec::new();
        let mut blobs: HashMap<String, String> = HashMap::new();
        let commits = self.git(side, &["rev-list", "--all"]);
        for commit in commits.lines() {
            let listing = self.git(side, &["ls-tree", "-r", commit]);
            for line in listing.lines() {
                let Some((meta, path)) = line.split_once('\t') else {
                    continue;
                };
                let mut fields = meta.split_whitespace();
                let (mode, kind, id) = (fields.next(), fields.next(), fields.next());
                if kind != Some("blob") {
                    continue;
                }
                let id = id.unwrap_or_default().to_string();
                let text = blobs
                    .entry(id.clone())
                    .or_insert_with(|| {
                        let out = std::process::Command::new("git")
                            .current_dir(self.side(side))
                            .args(["cat-file", "blob", &id])
                            .output()
                            .unwrap();
                        String::from_utf8_lossy(&out.stdout).to_string()
                    })
                    .clone();
                out.push((PathBuf::from(path), Some((text, mode == Some("100755")))));
            }
        }
        out
    }

    /// Every file in every stash on `side`, tracked and untracked, as a
    /// version.
    fn stashed(&self, side: Where) -> Vec<(PathBuf, Version)> {
        let mut out = Vec::new();
        let count = self.git(side, &["stash", "list"]).lines().count();
        for i in 0..count {
            for commit in [format!("stash@{{{i}}}"), format!("stash@{{{i}}}^3")] {
                let Ok(listing) = std::process::Command::new("git")
                    .current_dir(self.side(side))
                    .args(["ls-tree", "-r", &commit])
                    .output()
                else {
                    continue;
                };
                for line in String::from_utf8_lossy(&listing.stdout).lines() {
                    let Some((meta, path)) = line.split_once('\t') else {
                        continue;
                    };
                    let exec = meta.starts_with("100755");
                    let text = self.git(side, &["show", &format!("{commit}:{path}")]);
                    out.push((PathBuf::from(path), Some((format!("{text}\n"), exec))));
                }
            }
        }
        out
    }

    fn check(&self, last: &PeerSync) {
        if !last.conflicts.is_empty() {
            self.fail(&format!(
                "conflicts left after answering: {:?}",
                last.conflicts
            ));
        }
        let folder = self.copy(Where::Folder);
        let checkout = self.copy(Where::Checkout);
        // P4: the same files, the same modes, the same commit.
        if folder != checkout {
            let differ: Vec<_> = folder
                .keys()
                .chain(checkout.keys())
                .filter(|p| folder.get(*p) != checkout.get(*p))
                .collect();
            self.fail(&format!("the two sides differ at {differ:?} (P4)"));
        }
        let heads = (
            self.git(Where::Folder, &["rev-parse", "HEAD"]),
            self.git(Where::Checkout, &["rev-parse", "HEAD"]),
        );
        if heads.0 != heads.1 {
            self.fail(&format!(
                "the two sides are on different commits {heads:?} (P4)"
            ));
        }
        // Nothing the actors did not write.
        let written: HashSet<(PathBuf, Version)> = self.written.borrow().iter().cloned().collect();
        for (path, version) in &folder {
            if !written.contains(&(path.clone(), bytes_of(version.clone()))) {
                self.fail(&format!(
                    "{} holds {version:?}, which no actor wrote",
                    path.display()
                ));
            }
        }
        // P1: every version written is held — in the working copies, in a
        // commit either repository holds, or in a stash the sync kept
        // uncommitted work in when a commit arrived over it — or replaced
        // by one that is.
        let mut stashed = self.stashed(Where::Checkout);
        stashed.extend(self.committed(Where::Folder));
        stashed.extend(self.committed(Where::Checkout));
        let replaced = self.replaced.borrow();
        let reach = |path: &PathBuf, from: &Version| -> HashSet<Version> {
            let mut seen: HashSet<Version> = HashSet::new();
            let mut stack = vec![from.clone()];
            while let Some(v) = stack.pop() {
                if !seen.insert(v.clone()) {
                    continue;
                }
                if let Some(before) = replaced.get(&(path.clone(), v)) {
                    stack.extend(before.iter().cloned());
                }
            }
            seen
        };
        for (path, version) in self.written.borrow().iter() {
            let held: Version = bytes_of(folder.get(path).cloned().flatten());
            let in_stash = stashed
                .iter()
                .filter(|(p, _)| p == path)
                .any(|(_, v)| reach(path, &bytes_of(v.clone())).contains(version));
            if !reach(path, &held).contains(version) && !in_stash {
                self.fail(&format!(
                    "{} lost {version:?}: it is not there, and nothing there replaced it (P1)",
                    path.display()
                ));
            }
        }
        // P7: the ignored file is each side's own.
        for (side, text) in self.ignored.borrow().iter() {
            let now = std::fs::read_to_string(self.side(*side).join(IGNORED)).ok();
            if now.as_deref() != Some(text.as_str()) {
                self.fail(&format!(
                    "the {}'s {IGNORED} is {now:?}, not {text:?} (P7)",
                    side.dir()
                ));
            }
        }
        // P4: a further pass writes nothing.
        let again = self.pass(None, false).unwrap();
        if again.sent + again.received + again.missed != 0 || again.pushed {
            self.fail(&format!(
                "a pass after settling wrote again: {again:?} (P4)"
            ));
        }
    }
}

fn run_seed(seed: u64, steps: usize) {
    let ran = std::panic::catch_unwind(|| World::new(seed).run(steps));
    if let Err(payload) = ran {
        eprintln!("(seed {seed}; replay with TASTE_SYNC_SIM_SEED={seed})");
        std::panic::resume_unwind(payload);
    }
}

/// A few seeds, every test run: enough to keep the harness honest.
#[test]
fn a_few_seeded_schedules_hold_the_specification() {
    if let Ok(seed) = std::env::var("TASTE_SYNC_SIM_SEED") {
        run_seed(seed.parse().unwrap(), 14);
        return;
    }
    for seed in 1..=3 {
        run_seed(seed, 10);
    }
}

/// Many seeds, on request: `cargo test -p taste-devcontainer sim_ --
/// --ignored`, `TASTE_SYNC_SIM_SEEDS` of them (default 50) from
/// `TASTE_SYNC_SIM_FROM` (default 1000).
#[test]
#[ignore]
fn sim_many_seeded_schedules_hold_the_specification() {
    let count: u64 = std::env::var("TASTE_SYNC_SIM_SEEDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(50);
    let from: u64 = std::env::var("TASTE_SYNC_SIM_FROM")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1000);
    for seed in from..from + count {
        run_seed(seed, 14);
    }
}
