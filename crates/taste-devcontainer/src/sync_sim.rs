//! The sync, simulated: whole passes of the real folder-and-Personal sync
//! (`peer::sync_primary_peer_with`, both ways) between two repositories on
//! this machine, the VM's end stood in for by an ssh that runs git's far
//! end here, driven by a seeded schedule of what people and agents do —
//! writes, removals, renames of files and directories, a file becoming a
//! directory, symbolic links, the executable bit, edits and commits inside
//! a submodule, changes to `.gitignore` and to the `taste-sync` attribute,
//! branch switches, and commits on both sides at once — with those edits
//! made at every step of a pass, passes stopped partway, and writes made
//! to fail (docs/SYNC.md → "Making it hold", layer 3).
//!
//! What is checked is the specification, against what the actors did
//! rather than what the sync recorded:
//!
//! - **P1 by a ledger.** Every version an actor writes records what it
//!   replaced on its side. At the end every version ever written must be
//!   held — in a working copy, in a commit a branch reaches on either side
//!   (or in either side's submodule), or in a stash — or have been
//!   replaced, through that chain, by one that is. A resolved conflict, and
//!   a divergence the user merges, record the user's choice as that
//!   replacement.
//! - **Nothing the actors did not write** is in either working copy.
//! - **P4.** Left quiet, with every conflict answered and every divergence
//!   merged, the two sides hold the same files, modes, and links at every
//!   path in scope, are on the same branch at the same commit, with their
//!   submodules at the same commit, and a further pass writes nothing.
//! - **P7.** The always-ignored file on each side is that side's own.
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

/// A version of a path: its bytes and executable bit — a link's as "link
/// -> <target>" — or nothing there.
type Version = Option<(String, bool)>;

/// The paths the actors write: `a` and `a/q` so a file and a directory
/// meet; `d` and `d2` so a directory can be renamed onto a name with none;
/// `ign/f` under a rule the actors take in and out; and the submodule's.
const PATHS: [&str; 11] = [
    "a",
    "b",
    "a/q",
    "d/x",
    "d/y",
    "d/e/z",
    "d2/x",
    "c",
    "ign/f",
    "tpl/lib.txt",
    "tpl/n.txt",
];
/// Where links go: paths that are never a directory, so an actor's write
/// never travels through one.
const LINKS: [&str; 2] = ["b", "c"];
const IGNORED: &str = "build.log";
const SUB: &str = "tpl";
/// Three, so the two sides can each move to a different one: a position
/// both changed, which the sync says and leaves to the user.
const BRANCHES: [&str; 3] = ["main", "topic", "spike"];

/// A version as the ledger knows it: its bytes, the mode left out — git
/// merges a change of bytes on one side with a change of mode on the
/// other, cleanly, into a version neither wrote whole, and the mode is
/// held to agreeing by P4.
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
    /// The always-ignored file's last content on each side.
    ignored: RefCell<HashMap<Where, String>>,
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
            n: Cell::new(0),
            log: RefCell::new(Vec::new()),
            locked: Cell::new(false),
        };
        // The submodule's upstream.
        let upstream = world.root.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        world.git_at(&upstream, &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.join("lib.txt"), "the template\n").unwrap();
        world.git_at(&upstream, &["add", "-A"]);
        world.git_at(&upstream, &["commit", "-q", "-m", "template"]);
        // The folder: a few files, the rules, and the submodule.
        std::fs::create_dir_all(world.side(Where::Folder)).unwrap();
        world.git(Where::Folder, &["init", "-q", "-b", "main"]);
        world.write_rules(Where::Folder, false, true, false);
        for path in ["a", "b", "d/x"] {
            world.write(Where::Folder, path);
        }
        world.git(
            Where::Folder,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &upstream.display().to_string(),
                SUB,
            ],
        );
        world.record_over(
            Path::new("tpl/lib.txt"),
            Some(("the template\n".into(), false)),
            None,
        );
        let modules = world.held(Where::Folder, Path::new(".gitmodules"));
        world.record_over(Path::new(".gitmodules"), modules, None);
        world.git(Where::Folder, &["add", "-A"]);
        world.git(Where::Folder, &["commit", "-q", "-m", "start"]);
        // Personal: a clone, its submodule given it by the first pass.
        let folder = world.side(Where::Folder).display().to_string();
        let checkout = world.side(Where::Checkout).display().to_string();
        world.git_at(world.root.path(), &["clone", "-q", &folder, &checkout]);
        for side in [Where::Folder, Where::Checkout] {
            world.git(side, &["config", "user.name", "sim"]);
            world.git(side, &["config", "user.email", "sim@sim"]);
            world.git(side, &["config", "protocol.file.allow", "always"]);
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

    /// git in `dir`; `None` when it fails, which an actor's git may.
    fn try_git_at(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "sim")
            .env("GIT_AUTHOR_EMAIL", "sim@sim")
            .env("GIT_COMMITTER_NAME", "sim")
            .env("GIT_COMMITTER_EMAIL", "sim@sim")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        match self.try_git_at(dir, args) {
            Some(out) => out,
            None => self.fail(&format!("git {args:?} in {} failed", dir.display())),
        }
    }

    fn git(&self, side: Where, args: &[&str]) -> String {
        self.git_at(&self.side(side), args)
    }

    fn try_git(&self, side: Where, args: &[&str]) -> Option<String> {
        self.try_git_at(&self.side(side), args)
    }

    /// What `side` holds at `path`: a file's bytes and executable bit, a
    /// link's target, or nothing (a directory there holds no version of
    /// the path).
    fn held(&self, side: Where, path: &Path) -> Version {
        let at = self.side(side).join(path);
        let meta = std::fs::symlink_metadata(&at).ok()?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&at).ok()?;
            return Some((format!("link -> {}", target.display()), false));
        }
        if !meta.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&at).ok()?;
        Some((text, meta.permissions().mode() & 0o111 != 0))
    }

    /// An actor's act at `path` on `side`, recorded before it is done:
    /// `version` replaces what the side holds there now.
    fn record(&self, side: Where, path: &Path, version: Version) {
        let before = bytes_of(self.held(side, path));
        self.record_over(path, version, before);
    }

    /// `version` at `path` replacing `before`, whoever made it so.
    fn record_over(&self, path: &Path, version: Version, before: Version) {
        let version = bytes_of(version);
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

    /// Every file and link under `dir` on `side`, relative to the side's
    /// root, the repositories' own directories left out.
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

    /// Remove `path` on `side` as an actor would: a file or a link, or a
    /// directory and everything in it, each removal recorded.
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

    /// Make room for a file at `path` on `side`: a file or a link where a
    /// directory on the way should be removed, and a directory or a link
    /// where the file goes.
    fn clear_for(&self, side: Where, path: &Path) {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if std::fs::symlink_metadata(self.side(side).join(ancestor)).is_ok_and(|m| !m.is_dir())
            {
                self.remove(side, ancestor);
            }
        }
        let at = self.side(side).join(path);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => self.remove(side, path),
            // Removed without a record: the write that follows records
            // its replacing the link.
            Ok(m) if m.file_type().is_symlink() => {
                let _ = std::fs::remove_file(&at);
            }
            _ => {}
        }
    }

    fn fresh(&self) -> usize {
        let n = self.n.get() + 1;
        self.n.set(n);
        n
    }

    /// Write a new version at `path` on `side`, its bytes unique to this
    /// write.
    fn write(&self, side: Where, path: &str) {
        let path = Path::new(path);
        let before = bytes_of(self.held(side, path));
        let exec = self
            .held(side, path)
            .is_some_and(|(t, x)| x && !t.starts_with("link -> "));
        self.clear_for(side, path);
        let text = format!(
            "v{} by the {} at {}\n",
            self.fresh(),
            side.dir(),
            path.display()
        );
        self.record_over(path, Some((text.clone(), exec)), before);
        let at = self.side(side).join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, text).unwrap();
        if exec {
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Make `path` on `side` a link to a target unique to this write,
    /// which nothing need hold.
    fn link(&self, side: Where, path: &str) {
        let path = Path::new(path);
        let before = bytes_of(self.held(side, path));
        self.clear_for(side, path);
        let at = self.side(side).join(path);
        if std::fs::symlink_metadata(&at).is_ok() {
            let _ = std::fs::remove_file(&at);
        }
        let target = format!("target-{}", self.fresh());
        self.record_over(path, Some((format!("link -> {target}"), false)), before);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &at).unwrap();
    }

    /// The rules, written on `side`: `build.log` always ignored, `c` and
    /// `ign/` ignored when asked, and `ign/` synced anyway when asked —
    /// each write unique, so the ledger tells them apart.
    fn write_rules(&self, side: Where, ignore_c: bool, ignore_ign: bool, sync_ign: bool) {
        let n = self.fresh();
        let mut ignore = format!("# rules v{n}\n{IGNORED}\n");
        if ignore_c {
            ignore.push_str("c\n");
        }
        if ignore_ign {
            ignore.push_str("/ign/\n");
        }
        let attributes = if sync_ign {
            format!("# attributes v{n}\n/ign/** taste-sync\n")
        } else {
            format!("# attributes v{n}\n")
        };
        for (file, text) in [(".gitignore", ignore), (".gitattributes", attributes)] {
            let path = Path::new(file);
            self.record(side, path, Some((text.clone(), false)));
            let at = self.side(side).join(path);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
                self.remove(side, path);
            }
            std::fs::write(&at, text).unwrap();
        }
    }

    /// The branch `side` is on, or `None` for a detached HEAD.
    fn branch(&self, side: Where) -> Option<String> {
        self.try_git(side, &["symbolic-ref", "--short", "-q", "HEAD"])
    }

    /// Commit everything on `side`, its submodule's pointer included.
    fn commit(&self, side: Where, why: &str) -> bool {
        self.git(side, &["add", "-A"]);
        if self.git(side, &["status", "--porcelain"]).is_empty() {
            return false;
        }
        self.try_git(side, &["commit", "-q", "-m", why]).is_some()
    }

    /// One thing an actor does on `side`, chosen by the seed.
    fn act(&self, side: Where) {
        let path = *self.rng.pick(&PATHS);
        match self.rng.below(16) {
            0..=4 => {
                self.say(format!("{}: write {path}", side.dir()));
                self.write(side, path);
            }
            5 => {
                self.say(format!("{}: remove {path}", side.dir()));
                self.remove(side, Path::new(path));
            }
            6 => {
                let Some((text, exec)) = self.held(side, Path::new(path)) else {
                    return;
                };
                if text.starts_with("link -> ") {
                    return;
                }
                self.say(format!("{}: chmod {path} to {}", side.dir(), !exec));
                self.record(side, Path::new(path), Some((text, !exec)));
                let at = self.side(side).join(path);
                let mode = if exec { 0o644 } else { 0o755 };
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            7 => {
                let to = *self.rng.pick(&PATHS);
                let (from, to) = (Path::new(path), Path::new(to));
                if from == to || from.starts_with(to) || to.starts_with(from) {
                    return;
                }
                let Some(version) = self.held(side, from) else {
                    return;
                };
                // A link only where links go.
                if version.0.starts_with("link -> ") && !LINKS.iter().any(|l| Path::new(l) == to) {
                    return;
                }
                self.say(format!(
                    "{}: rename {} to {}",
                    side.dir(),
                    from.display(),
                    to.display()
                ));
                let before = bytes_of(self.held(side, to));
                self.clear_for(side, to);
                self.record_over(to, Some(version), before);
                self.record(side, from, None);
                let target = self.side(side).join(to);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::rename(self.side(side).join(from), &target).unwrap();
            }
            8 => {
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
            9 => {
                let path = *self.rng.pick(&LINKS);
                self.say(format!("{}: link {path}", side.dir()));
                self.link(side, path);
            }
            10 => {
                let (c, ign, sync) = (
                    self.rng.chance(40),
                    self.rng.chance(60),
                    self.rng.chance(40),
                );
                self.say(format!(
                    "{}: rules: ignore c {c}, ignore ign/ {ign}, sync ign/ {sync}",
                    side.dir()
                ));
                self.write_rules(side, c, ign, sync);
            }
            11 => {
                let text = format!("ignored {} on the {}\n", self.fresh(), side.dir());
                self.say(format!("{}: write {IGNORED}", side.dir()));
                std::fs::write(self.side(side).join(IGNORED), &text).unwrap();
                self.ignored.borrow_mut().insert(side, text);
            }
            12 => {
                let sub = self.side(side).join(SUB);
                if !sub.join(".git").exists() {
                    return;
                }
                self.try_git_at(&sub, &["add", "-A"]);
                if self
                    .try_git_at(&sub, &["status", "--porcelain"])
                    .is_none_or(|s| s.is_empty())
                {
                    return;
                }
                self.say(format!("{}: commit in {SUB}", side.dir()));
                self.try_git_at(&sub, &["commit", "-q", "-m", "sim"]);
            }
            13 => {
                let to = *self.rng.pick(&BRANCHES);
                if self.branch(side).as_deref() == Some(to) {
                    return;
                }
                let exists = self
                    .try_git(
                        side,
                        &["rev-parse", "-q", "--verify", &format!("refs/heads/{to}")],
                    )
                    .is_some();
                let switched = if exists {
                    self.try_git(side, &["switch", "-q", to]).is_some()
                } else {
                    self.try_git(side, &["switch", "-q", "-c", to]).is_some()
                };
                if switched {
                    self.say(format!("{}: switch to {to}", side.dir()));
                }
            }
            _ => {
                if self.commit(side, "sim") {
                    self.say(format!("{}: commit", side.dir()));
                }
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
        let say = |line: &str| {
            // The position protocol's own steps, always in the schedule.
            if line.starts_with("moving Personal to") || line.starts_with("sending this folder") {
                self.say(format!("  {line}"));
            }
            maybe(line)
        };
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
        // `TASTE_SYNC_SIM_KEEP=1` leaves the repositories where they are,
        // for a look at what the schedule left.
        if std::env::var_os("TASTE_SYNC_SIM_KEEP").is_some() {
            let kept = self.root.path().to_path_buf();
            std::mem::forget(tempfile::TempDir::new());
            eprintln!("kept {}", kept.display());
            std::process::exit(101);
        }
        panic!(
            "seed {}: {what}\nreplay with TASTE_SYNC_SIM_SEED={}\n{}",
            self.seed,
            self.seed,
            self.log.borrow().join("\n")
        );
    }

    /// Where the two sides stand: each one's branch and commit.
    fn positions(&self) -> ((Option<String>, String), (Option<String>, String)) {
        (
            (
                self.branch(Where::Folder),
                self.git(Where::Folder, &["rev-parse", "HEAD"]),
            ),
            (
                self.branch(Where::Checkout),
                self.git(Where::Checkout, &["rev-parse", "HEAD"]),
            ),
        )
    }

    /// The user brings the folder to where Personal is, as a person would
    /// with the sync's note in front of them — in the submodule first, a
    /// pair of its own, then in the parent: their own work committed,
    /// Personal's branch checked out, and Personal's commits merged,
    /// keeping their own side where the two clash. Whatever the merge
    /// leaves in the folder is the user's choice, and recorded as such.
    fn user_merges(&self) {
        let before: BTreeMap<PathBuf, Version> = self.copy(Where::Folder);
        let folder = self.side(Where::Folder);
        let checkout = self.side(Where::Checkout);
        for (here, there) in [
            (folder.join(SUB), checkout.join(SUB)),
            (folder.clone(), checkout.clone()),
        ] {
            if !here.join(".git").exists() || !there.join(".git").exists() {
                continue;
            }
            let Some(branch) = self.try_git_at(&there, &["symbolic-ref", "--short", "-q", "HEAD"])
            else {
                continue;
            };
            let at = |p: &Path| self.try_git_at(p, &["rev-parse", "HEAD"]);
            let mine_branch = self.try_git_at(&here, &["symbolic-ref", "--short", "-q", "HEAD"]);
            if mine_branch.as_deref() == Some(branch.as_str()) && at(&here) == at(&there) {
                continue;
            }
            self.say(format!(
                "the user brings {} to Personal's {branch}",
                here.strip_prefix(self.root.path()).unwrap().display()
            ));
            self.try_git_at(&here, &["add", "-A"]);
            self.try_git_at(&here, &["commit", "-q", "-m", "the user's own"]);
            let theirs = format!("refs/taste/vm/{branch}");
            let has_theirs = self
                .try_git_at(&here, &["rev-parse", "-q", "--verify", &theirs])
                .is_some();
            if mine_branch.as_deref() != Some(branch.as_str()) {
                let mine = format!("refs/heads/{branch}");
                if self
                    .try_git_at(&here, &["rev-parse", "-q", "--verify", &mine])
                    .is_some()
                {
                    self.try_git_at(&here, &["switch", "-q", &branch]);
                } else if has_theirs {
                    self.try_git_at(&here, &["switch", "-q", "-c", &branch, &theirs]);
                }
            }
            if has_theirs
                && self
                    .try_git_at(&here, &["merge", "-q", "--no-edit", "-X", "ours", &theirs])
                    .is_none()
            {
                self.try_git_at(&here, &["add", "-A"]);
                self.try_git_at(&here, &["commit", "-q", "--no-edit"]);
            }
        }
        let after = self.copy(Where::Folder);
        for path in before.keys().chain(after.keys()) {
            let (was, now) = (
                before.get(path).cloned().flatten(),
                after.get(path).cloned().flatten(),
            );
            self.record_over(path, now, bytes_of(was));
        }
    }

    /// Personal's own files that its note says are in the way of a commit
    /// it would not take — an untracked or ignored file where the folder's
    /// commit has one — moved aside by the user, kept beside under
    /// `<name>.kept`: the sync says this and leaves it, never moving the
    /// checkout's files itself.
    fn user_moves_aside(&self, note: &str) {
        let Some(list) = note.split("would be overwritten by merge:").nth(1) else {
            return;
        };
        for line in list.lines() {
            let Some(name) = line.strip_prefix('\t') else {
                continue;
            };
            let path = Path::new(name.trim());
            let version = self.held(Where::Checkout, path);
            if version.is_none() {
                continue;
            }
            let aside = PathBuf::from(format!("{}.kept", path.display()));
            self.say(format!(
                "the user moves Personal's {} aside",
                path.display()
            ));
            self.record_over(&aside, version.clone(), None);
            self.record_over(path, None, bytes_of(version));
            let root = self.side(Where::Checkout);
            let _ = std::fs::rename(root.join(path), root.join(&aside));
        }
    }

    /// Where each side's submodule stands.
    fn sub_positions(&self) -> (Option<String>, Option<String>) {
        (
            self.try_git_at(&self.side(Where::Folder).join(SUB), &["rev-parse", "HEAD"]),
            self.try_git_at(
                &self.side(Where::Checkout).join(SUB),
                &["rev-parse", "HEAD"],
            ),
        )
    }

    /// Every file and link on `side` and its version, but the always-
    /// ignored file.
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

    /// Whether `path` is in the sync's scope by the folder's rules: not
    /// ignored, or synced anyway. The submodule's files are its own pair's,
    /// which ignores nothing.
    fn in_scope(&self, path: &Path) -> bool {
        if path.starts_with(SUB) {
            return true;
        }
        let at = self.side(Where::Folder);
        let p = path.display().to_string();
        let ignored = std::process::Command::new("git")
            .current_dir(&at)
            .args(["check-ignore", "-q", "--no-index", "--", &p])
            .status()
            .is_ok_and(|s| s.success());
        if !ignored {
            return true;
        }
        self.try_git(Where::Folder, &["check-attr", "taste-sync", "--", &p])
            .is_some_and(|out| out.ends_with(": set"))
    }

    /// The whole run: setup agreed, a schedule of acts and passes, then
    /// quiet, every conflict answered and every divergence merged, and
    /// every check.
    fn run(&self, steps: usize) {
        self.settle(None);
        for _ in 0..steps {
            match self.rng.below(100) {
                0..=54 => self.acts(),
                55..=59 => self.user_merges(),
                _ => {
                    self.pass(None, true);
                }
            }
        }
        self.say("quiet:");
        let mut sync = self.settle(None);
        for _ in 0..6 {
            if !sync.conflicts.is_empty() {
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
                    self.record_over(path, kept, bytes_of(lost));
                }
                let resolve = Resolve {
                    keep,
                    paths: sync.conflicts.iter().cloned().collect(),
                };
                sync = self.settle(Some(&resolve));
                continue;
            }
            let (here, there) = self.positions();
            let (sub_here, sub_there) = self.sub_positions();
            if here == there && sub_here == sub_there {
                break;
            }
            if let Some(note) = &sync.note {
                self.user_moves_aside(note);
            }
            self.user_merges();
            sync = self.settle(None);
        }
        self.check(&sync);
    }

    /// Every version in every commit a branch reaches in `dir`, its paths
    /// under `prefix`: what is committed is not lost, whatever the working
    /// copies hold. Only what a branch reaches: the sync's own snapshots
    /// and archive are commits too, and a version that only they hold is
    /// one the user cannot find.
    fn committed(&self, dir: &Path, prefix: &Path) -> Vec<(PathBuf, Version)> {
        let mut out = Vec::new();
        let mut blobs: HashMap<String, String> = HashMap::new();
        let Some(commits) = self.try_git_at(dir, &["rev-list", "--branches"]) else {
            return out;
        };
        for commit in commits.lines() {
            let listing = self.git_at(dir, &["ls-tree", "-r", commit]);
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
                            .current_dir(dir)
                            .args(["cat-file", "blob", &id])
                            .output()
                            .unwrap();
                        String::from_utf8_lossy(&out.stdout).to_string()
                    })
                    .clone();
                let text = if mode == Some("120000") {
                    format!("link -> {text}")
                } else {
                    text
                };
                out.push((prefix.join(path), Some((text, mode == Some("100755")))));
            }
        }
        out
    }

    /// Every file in every stash in `dir`, tracked and untracked, its
    /// paths under `prefix`.
    fn stashed(&self, dir: &Path, prefix: &Path) -> Vec<(PathBuf, Version)> {
        let mut out = Vec::new();
        let count = self
            .try_git_at(dir, &["stash", "list"])
            .map_or(0, |s| s.lines().count());
        for i in 0..count {
            for commit in [format!("stash@{{{i}}}"), format!("stash@{{{i}}}^3")] {
                let Some(listing) = self.try_git_at(dir, &["ls-tree", "-r", &commit]) else {
                    continue;
                };
                for line in listing.lines() {
                    let Some((meta, path)) = line.split_once('\t') else {
                        continue;
                    };
                    let exec = meta.starts_with("100755");
                    let link = meta.starts_with("120000");
                    let Some(text) = self.try_git_at(dir, &["show", &format!("{commit}:{path}")])
                    else {
                        continue;
                    };
                    let text = if link {
                        format!("link -> {text}")
                    } else {
                        format!("{text}\n")
                    };
                    out.push((prefix.join(path), Some((text, exec))));
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
        let (here, there) = self.positions();
        if here != there {
            self.fail(&format!(
                "the two sides stand apart after the user merged: {here:?} {there:?} (P4)"
            ));
        }
        let sub_heads = self.sub_positions();
        if sub_heads.0 != sub_heads.1 {
            self.fail(&format!("the submodule stands apart: {sub_heads:?} (P4)"));
        }
        let folder = self.copy(Where::Folder);
        let checkout = self.copy(Where::Checkout);
        // P4: the same files, modes, and links at every path in scope.
        let differ: Vec<&PathBuf> = folder
            .keys()
            .chain(checkout.keys())
            .filter(|p| folder.get(*p) != checkout.get(*p))
            .filter(|p| self.in_scope(p))
            .collect();
        if !differ.is_empty() {
            self.fail(&format!("the two sides differ at {differ:?} (P4)"));
        }
        // Nothing the actors did not write. By bytes, wherever they are:
        // each write's bytes are its own, and git's merges carry a change
        // along a rename — a modification of `d/x` stashed over a commit
        // that renamed `d` comes back at `d2/x`.
        let written_bytes: HashSet<String> = self
            .written
            .borrow()
            .iter()
            .filter_map(|(_, v)| v.as_ref().map(|(text, _)| text.clone()))
            .collect();
        for (path, version) in folder.iter().chain(&checkout) {
            if !version
                .as_ref()
                .is_some_and(|(text, _)| written_bytes.contains(text))
            {
                self.fail(&format!(
                    "{} holds {version:?}, which no actor wrote",
                    path.display()
                ));
            }
        }
        // P1: every version written is held — in a working copy, in a
        // commit a branch reaches, or in a stash — or replaced by one that
        // is.
        let mut kept: Vec<(PathBuf, Version)> = Vec::new();
        for side in [Where::Folder, Where::Checkout] {
            let root = self.side(side);
            kept.extend(self.committed(&root, Path::new("")));
            kept.extend(self.committed(&root.join(SUB), Path::new(SUB)));
            kept.extend(self.stashed(&root, Path::new("")));
            kept.extend(self.stashed(&root.join(SUB), Path::new(SUB)));
        }
        kept.extend(folder.iter().map(|(p, v)| (p.clone(), v.clone())));
        kept.extend(checkout.iter().map(|(p, v)| (p.clone(), v.clone())));
        // A path a working copy does not hold holds "absent" there, which a
        // removal wrote; so a removal that came back undone is found (P5).
        for (path, _) in self.written.borrow().iter() {
            for copy in [&folder, &checkout] {
                if !copy.contains_key(path) {
                    kept.push((path.clone(), None));
                }
            }
        }
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
        let mut live: HashMap<PathBuf, HashSet<Version>> = HashMap::new();
        for (path, version) in &kept {
            let entry = live.entry(path.clone()).or_default();
            entry.extend(reach(path, &bytes_of(version.clone())));
        }
        // And a version's bytes held at any path are the version held, for
        // the same reason.
        let held_bytes: HashSet<String> = kept
            .iter()
            .filter_map(|(_, v)| v.as_ref().map(|(text, _)| text.clone()))
            .collect();
        let mut live_bytes: HashSet<String> = HashSet::new();
        for set in live.values() {
            live_bytes.extend(set.iter().flatten().map(|(text, _)| text.clone()));
        }
        for (path, version) in self.written.borrow().iter() {
            let anywhere = version
                .as_ref()
                .is_some_and(|(text, _)| held_bytes.contains(text) || live_bytes.contains(text));
            if !anywhere && !live.get(path).is_some_and(|l| l.contains(version)) {
                self.fail(&format!(
                    "{} lost {version:?}: it is held nowhere, and nothing held replaced it (P1)",
                    path.display()
                ));
            }
        }
        // P7: the always-ignored file is each side's own.
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

/// What a run exercised, by the lines its schedule logged: so a long run
/// can say that the cases it claims to cover came up, and how often.
const COVERAGE: [(&str, &str); 13] = [
    ("links made", ": link "),
    ("rules changed", ": rules:"),
    ("branches switched", ": switch to "),
    ("submodule commits", ": commit in tpl"),
    ("commits", ": commit"),
    ("directories renamed", ": rename directory"),
    ("passes stopped", "the pass stopped"),
    ("divergences", "have diverged"),
    ("Personal followed a switch", "moving Personal to"),
    ("both moved", "since they last agreed on"),
    ("conflicts in the submodule", "conflicts [\"tpl/"),
    ("user merges", "the user brings "),
    ("files moved aside", "aside"),
];

static TALLY: std::sync::Mutex<[usize; COVERAGE.len()]> =
    std::sync::Mutex::new([0; COVERAGE.len()]);

fn run_seed(seed: u64, steps: usize) {
    let ran = std::panic::catch_unwind(|| {
        let world = World::new(seed);
        world.run(steps);
        let mut tally = TALLY.lock().unwrap();
        for line in world.log.borrow().iter() {
            for (i, (_, needle)) in COVERAGE.iter().enumerate() {
                if line.contains(needle) {
                    tally[i] += 1;
                }
            }
        }
    });
    if let Err(payload) = ran {
        eprintln!("(seed {seed}; replay with TASTE_SYNC_SIM_SEED={seed})");
        std::panic::resume_unwind(payload);
    }
}

/// A few seeds, every test run: enough to keep the harness honest.
#[test]
fn a_few_seeded_schedules_hold_the_specification() {
    if let Ok(seed) = std::env::var("TASTE_SYNC_SIM_SEED") {
        run_seed(seed.parse().unwrap(), 16);
        return;
    }
    for seed in 1..=3 {
        run_seed(seed, 12);
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
        run_seed(seed, 16);
    }
    let tally = TALLY.lock().unwrap();
    eprintln!("{count} seeds from {from}; what they exercised:");
    for ((what, _), n) in COVERAGE.iter().zip(tally.iter()) {
        eprintln!("  {what}: {n}");
    }
}
