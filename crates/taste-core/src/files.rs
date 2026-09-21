//! **Files, wherever an environment's checkout is.**
//!
//! An environment's working copy may be on this host or in a VM
//! ([`crate::environment::Checkout`]). Everything the IDE does to files —
//! open one in the editor, list a directory for the tree, save a buffer,
//! run `git status`, search — has to work in both worlds, and the only way
//! that stays true is one API with two implementations rather than two
//! code paths at every call site. [`Files`] is that API.
//!
//! # Synchronous, and off the GTK thread
//!
//! Every method blocks. For a local checkout that is `std::fs`, which is
//! what every call site did already; for a remote one it is a round trip
//! to the VM, which the caller waits on the same way. The rule this
//! imposes is the rule the codebase already has (CLAUDE.md → Performance):
//! **no filesystem IO on the main thread** — a `Files` call belongs in
//! `spawn_blocking`, exactly where its `std::fs` predecessor did. Making
//! the API synchronous keeps taste-core free of a runtime and keeps the
//! conversion mechanical: `std::fs::read(p)` becomes `files.read(p)`.
//!
//! # Paths are the checkout's own
//!
//! Every path handed in is in the checkout's world — a host path for a
//! local checkout, a VM path for a remote one — which is what
//! [`crate::environment::Checkout::path`] gives. The service does not
//! translate; the container that mounts the checkout sees the same path,
//! and so does the agent, so nothing anywhere has to.
//!
//! # `exec` is for the remote world
//!
//! [`Files::exec`] runs a program beside the files: `git`, `rg`, `tar`. A
//! local checkout has libgit2 and the in-process search for that, and
//! those stay the right answer on this host; `exec` exists so the remote
//! implementation can run the same tools where the files are. The local
//! arm is implemented so a caller can be written once and tested locally,
//! not as an invitation to shell out where a library already serves.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Kind {
    pub fn from_name(name: &str) -> Self {
        match name {
            "file" => Kind::File,
            "dir" => Kind::Dir,
            "symlink" => Kind::Symlink,
            _ => Kind::Other,
        }
    }

    fn of(meta: &std::fs::Metadata) -> Self {
        let ft = meta.file_type();
        if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
}

/// What `stat` says about a path. The symlink itself, never its target:
/// a tree that resolved links would show the same file twice and a save
/// through one could land somewhere the tree did not show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    /// Milliseconds since the epoch.
    pub mtime_ms: u64,
    /// Permission bits.
    pub mode: u32,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
}

/// What a program run beside the files produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// The exit status, or -1 for a signal.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    pub fn stdout_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The remote half: a service that has the files and answers for them.
/// Implemented by the keeper in `taste-devcontainer`, which reaches a VM
/// over the transport every other environment fact already rides.
pub trait RemoteFiles: Send + Sync + fmt::Debug {
    /// One phrase naming where the files are, for errors and the log.
    fn describe(&self) -> String;
    fn stat(&self, path: &Path) -> io::Result<Stat>;
    fn list(&self, path: &Path) -> io::Result<Vec<Entry>>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Atomic: written beside the target and renamed into place, with the
    /// parents made.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn mkdir_all(&self, path: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path, recursive: bool) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn exec(&self, cwd: &Path, argv: &[String]) -> io::Result<ExecOutput>;
}

/// How to reach an environment's files.
#[derive(Clone, Debug)]
pub enum Files {
    /// This host's filesystem.
    Local,
    /// A service that has them somewhere else.
    Remote(Arc<dyn RemoteFiles>),
}

impl Files {
    pub fn is_local(&self) -> bool {
        matches!(self, Files::Local)
    }

    /// Where the files are, for errors and the log.
    pub fn describe(&self) -> String {
        match self {
            Files::Local => "this machine".into(),
            Files::Remote(remote) => remote.describe(),
        }
    }

    pub fn stat(&self, path: &Path) -> io::Result<Stat> {
        match self {
            Files::Local => {
                let meta = std::fs::symlink_metadata(path)?;
                Ok(Stat {
                    kind: Kind::of(&meta),
                    size: meta.len(),
                    mtime_ms: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    mode: {
                        use std::os::unix::fs::PermissionsExt;
                        meta.permissions().mode() & 0o7777
                    },
                })
            }
            Files::Remote(remote) => remote.stat(path),
        }
    }

    pub fn exists(&self, path: &Path) -> bool {
        self.stat(path).is_ok()
    }

    pub fn is_dir(&self, path: &Path) -> bool {
        self.stat(path).is_ok_and(|s| s.kind == Kind::Dir)
    }

    pub fn is_file(&self, path: &Path) -> bool {
        self.stat(path).is_ok_and(|s| s.kind == Kind::File)
    }

    /// Entries of a directory, sorted by name.
    pub fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let mut entries = match self {
            Files::Local => std::fs::read_dir(path)?
                .map(|entry| {
                    let entry = entry?;
                    let kind = entry.file_type().map(|ft| {
                        if ft.is_symlink() {
                            Kind::Symlink
                        } else if ft.is_dir() {
                            Kind::Dir
                        } else if ft.is_file() {
                            Kind::File
                        } else {
                            Kind::Other
                        }
                    })?;
                    Ok(Entry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        kind,
                    })
                })
                .collect::<io::Result<Vec<_>>>()?,
            Files::Remote(remote) => remote.list(path)?,
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Files::Local => std::fs::read(path),
            Files::Remote(remote) => remote.read(path),
        }
    }

    pub fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Write a file whole, making its parents. Atomic on both arms: the
    /// remote service renames into place, and the local arm writes beside
    /// the target and renames too, so a reader never sees a half-written
    /// file and a crash never leaves a truncated one.
    pub fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match self {
            Files::Local => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let part = part_name(path);
                std::fs::write(&part, bytes)?;
                std::fs::rename(&part, path).inspect_err(|_| {
                    let _ = std::fs::remove_file(&part);
                })
            }
            Files::Remote(remote) => remote.write(path, bytes),
        }
    }

    pub fn mkdir_all(&self, path: &Path) -> io::Result<()> {
        match self {
            Files::Local => std::fs::create_dir_all(path),
            Files::Remote(remote) => remote.mkdir_all(path),
        }
    }

    pub fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        match self {
            Files::Local => {
                let meta = std::fs::symlink_metadata(path)?;
                if meta.is_dir() {
                    if recursive {
                        std::fs::remove_dir_all(path)
                    } else {
                        std::fs::remove_dir(path)
                    }
                } else {
                    std::fs::remove_file(path)
                }
            }
            Files::Remote(remote) => remote.remove(path, recursive),
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        match self {
            Files::Local => std::fs::rename(from, to),
            Files::Remote(remote) => remote.rename(from, to),
        }
    }

    /// Run a program beside the files, with `cwd` as its working directory,
    /// and wait for it.
    pub fn exec(&self, cwd: &Path, argv: &[String]) -> io::Result<ExecOutput> {
        match self {
            Files::Local => {
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
                let output = std::process::Command::new(program)
                    .args(args)
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::null())
                    .output()?;
                Ok(ExecOutput {
                    status: output.status.code().unwrap_or(-1),
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            Files::Remote(remote) => remote.exec(cwd, argv),
        }
    }
}

/// The temporary name a write lands under before it is renamed into
/// place. Beside the target, so the rename is within one filesystem.
pub fn part_name(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".taste-part");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local arm is `std::fs` with the shape the remote arm has to
    /// match: symlinks reported as themselves, entries sorted, writes
    /// atomic, exec captured.
    #[test]
    fn the_local_arm_answers_like_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = Files::Local;
        assert!(files.is_local());

        files.write(&root.join("a/b/hello.txt"), b"hi\n").unwrap();
        assert_eq!(
            files.read_to_string(&root.join("a/b/hello.txt")).unwrap(),
            "hi\n"
        );
        assert!(
            !part_name(&root.join("a/b/hello.txt")).exists(),
            "the part was renamed"
        );
        let stat = files.stat(&root.join("a/b/hello.txt")).unwrap();
        assert_eq!(stat.kind, Kind::File);
        assert_eq!(stat.size, 3);
        assert!(stat.mtime_ms > 0);

        std::os::unix::fs::symlink("hello.txt", root.join("a/b/link")).unwrap();
        assert_eq!(
            files.stat(&root.join("a/b/link")).unwrap().kind,
            Kind::Symlink
        );
        files.mkdir_all(&root.join("a/b/zdir")).unwrap();
        let listed = files.list(&root.join("a/b")).unwrap();
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["hello.txt", "link", "zdir"], "sorted by name");
        assert_eq!(listed[2].kind, Kind::Dir);
        assert!(files.is_dir(&root.join("a")));
        assert!(!files.exists(&root.join("nope")));

        files
            .rename(&root.join("a/b/hello.txt"), &root.join("a/moved.txt"))
            .unwrap();
        assert!(files.is_file(&root.join("a/moved.txt")));
        assert!(files.remove(&root.join("a/b"), false).is_err(), "not empty");
        files.remove(&root.join("a/b"), true).unwrap();
        assert!(!files.exists(&root.join("a/b")));

        let out = files
            .exec(
                root,
                &[
                    "sh".into(),
                    "-c".into(),
                    "echo out; echo err >&2; exit 3".into(),
                ],
            )
            .unwrap();
        assert_eq!(out.status, 3);
        assert_eq!(out.stdout_utf8(), "out\n");
        assert_eq!(out.stderr_utf8(), "err\n");
        assert!(!out.success());

        // A missing file is NotFound on this arm, and the remote arm maps
        // its ENOENT to the same kind, so `textfile::load`'s "new file"
        // branch works in both worlds.
        let missing = files.read(&root.join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_kind_round_trips_through_its_name() {
        for (name, kind) in [
            ("file", Kind::File),
            ("dir", Kind::Dir),
            ("symlink", Kind::Symlink),
            ("socket", Kind::Other),
        ] {
            assert_eq!(Kind::from_name(name), kind);
        }
    }
}
