//! **Where other programs on this machine find what Personal serves.**
//!
//! A port Personal's container forwards reaches this host's loopback, and
//! any local program can connect to it; what a program could not do was
//! find it, or know that 5433 is a project's Postgres moved off a busy
//! 5432 (David, 2026-10-18: "I'd like to make these ports discoverable by
//! other local apps"). Zeroconf was the obvious tool and the wrong one: it
//! announces on the network, and these ports are this machine's alone. So
//! each IDE writes a file, in a directory every program of this user can
//! read and nobody else can:
//!
//! ```text
//! $XDG_RUNTIME_DIR/taste-ide/ports/<workspace-key>.json
//! ```
//!
//! The same directory whichever way the IDE runs: a Flatpak shares it
//! from the host (`--filesystem=xdg-run/taste-ide:create`), so a reader
//! looks in one place for every IDE, native or sandboxed, release or
//! development build.
//!
//! **One file per workspace**, written by the IDE supervising it. Several
//! IDEs may be open at once, and two windows on one folder already settle
//! which supervises (`crate::instance`), so names cannot collide.
//!
//! **Liveness is a lock, not a process id**, since a sandboxed IDE's pids
//! mean nothing outside its sandbox: while an IDE serves a file, it holds
//! an exclusive `flock` on the `<workspace-key>.lock` beside it. A reader
//! that can take a shared lock there (`LOCK_SH | LOCK_NB`) has found a
//! file nobody is serving — an IDE that crashed — and should skip it. The
//! IDE removes both on a clean stop, and the next IDE to start sweeps any
//! a crash left ([`sweep`]). The JSON is replaced atomically, so a reader
//! never sees half of one.
//!
//! The document (`version` 1):
//!
//! ```json
//! {
//!   "version": 1,
//!   "workspace": { "name": "emet-rpg", "root": "/home/u/Projects/emet-rpg" },
//!   "environment": "Personal",
//!   "ports": [
//!     { "port": 3000, "host": "127.0.0.1", "host_port": 3000,
//!       "label": "App", "protocol": "http", "url": "http://127.0.0.1:3000" }
//!   ]
//! }
//! ```
//!
//! `port` is the number inside the container, as the project's config
//! names it; `host_port` is where it answers here, which differs only
//! when the number was taken.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// The schema's version: raised on any change a reader would misread.
pub const VERSION: u32 = 1;

/// The directory the files live in, or `None` without a runtime directory
/// (which a desktop session always has).
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|run| PathBuf::from(run).join("taste-ide").join("ports"))
}

/// One forwarded port, as a reader needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Port {
    /// The number inside the container, as the config names it.
    pub port: u16,
    /// The address it answers on here.
    pub host: String,
    /// The number it answers on here.
    pub host_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Workspace {
    name: String,
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Document {
    version: u32,
    workspace: Workspace,
    environment: &'static str,
    ports: Vec<Port>,
}

/// A workspace's file, served for as long as this is held: dropping it
/// removes the file and lets the lock go.
#[derive(Debug)]
pub struct Advertised {
    json: PathBuf,
    lock_path: PathBuf,
    _lock: File,
}

impl Drop for Advertised {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.json);
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Take `path`'s lock without waiting: exclusive, or shared; whether it
/// was had.
fn try_flock(file: &File, exclusive: bool) -> bool {
    use std::os::fd::AsRawFd;
    let how = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | libc::LOCK_NB;
    // SAFETY: a valid descriptor, and no pointers.
    unsafe { libc::flock(file.as_raw_fd(), how) == 0 }
}

/// Serve `ports` for the workspace at `root`, keyed `key`, in `dir`:
/// updated in place when already served (`current`), started otherwise.
/// An empty list withdraws it.
pub fn advertise_in(
    dir: &Path,
    current: &mut Option<Advertised>,
    key: &str,
    root: &Path,
    ports: Vec<Port>,
) -> Result<()> {
    if ports.is_empty() {
        current.take();
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let json = dir.join(format!("{key}.json"));
    if current.as_ref().map(|a| &a.json) != Some(&json) {
        let lock_path = dir.join(format!("{key}.lock"));
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if !try_flock(&lock, true) {
            anyhow::bail!(
                "another IDE is serving {} — it has this folder open",
                json.display()
            );
        }
        *current = Some(Advertised {
            json: json.clone(),
            lock_path,
            _lock: lock,
        });
    }
    let document = Document {
        version: VERSION,
        workspace: Workspace {
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            root: root.to_path_buf(),
        },
        environment: "Personal",
        ports,
    };
    let part = dir.join(format!(".{key}.json.{}", std::process::id()));
    std::fs::write(&part, serde_json::to_vec_pretty(&document)?)
        .with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, &json).with_context(|| format!("placing {}", json.display()))
}

/// [`advertise_in`] the runtime directory ([`dir`]).
pub fn advertise(
    current: &mut Option<Advertised>,
    key: &str,
    root: &Path,
    ports: Vec<Port>,
) -> Result<()> {
    let Some(dir) = dir() else {
        return Ok(());
    };
    advertise_in(&dir, current, key, root, ports)
}

/// Remove what a crashed IDE left in `dir`: every file whose lock nobody
/// holds.
pub fn sweep_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let lock_path = path.with_extension("lock");
        let held = File::options()
            .write(true)
            .open(&lock_path)
            .map(|lock| !try_flock(&lock, true))
            .unwrap_or(false);
        if !held {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&lock_path);
        }
    }
}

/// [`sweep_in`] the runtime directory.
pub fn sweep() {
    if let Some(dir) = dir() {
        sweep_in(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(n: u16) -> Port {
        Port {
            port: n,
            host: "127.0.0.1".into(),
            host_port: n,
            label: Some("App".into()),
            protocol: Some("http".into()),
            url: format!("http://127.0.0.1:{n}"),
        }
    }

    /// Served while held, and gone when dropped; a second server of the
    /// same workspace is refused; a crashed one's file is swept, and a live
    /// one's is not.
    #[test]
    fn a_workspace_is_served_once_and_swept_when_its_server_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/home/u/Projects/emet-rpg");
        let mut served = None;
        advertise_in(dir.path(), &mut served, "abc", root, vec![port(3000)]).unwrap();
        let json = dir.path().join("abc.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["workspace"]["name"], "emet-rpg");
        assert_eq!(doc["ports"][0]["url"], "http://127.0.0.1:3000");

        // A reader's liveness check: a shared lock is refused while served.
        let reader = File::open(dir.path().join("abc.lock")).unwrap();
        assert!(!try_flock(&reader, false), "live");

        let mut second = None;
        assert!(advertise_in(dir.path(), &mut second, "abc", root, vec![port(3000)]).is_err());

        // Updated in place, then withdrawn by an empty list.
        advertise_in(
            dir.path(),
            &mut served,
            "abc",
            root,
            vec![port(3000), port(5432)],
        )
        .unwrap();
        assert!(std::fs::read_to_string(&json).unwrap().contains("5432"));
        sweep_in(dir.path());
        assert!(json.exists(), "a live server's file is not swept");
        advertise_in(dir.path(), &mut served, "abc", root, Vec::new()).unwrap();
        assert!(!json.exists());

        // A crash: the files stay, nobody holds the lock, the sweep takes them.
        std::fs::write(&json, "{}").unwrap();
        std::fs::write(dir.path().join("abc.lock"), "").unwrap();
        sweep_in(dir.path());
        assert!(!json.exists());
    }
}
