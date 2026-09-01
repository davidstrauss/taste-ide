//! Where the IDE serves agents, as a convention both sides can compute.
//!
//! The socket path is shared knowledge: `taste-mcp` binds it, and the
//! devcontainer supervisor has to mount it in, because the agent that
//! connects runs inside the container. Neither crate can depend on the
//! other, so the convention lives here.

use std::path::{Path, PathBuf};

/// Socket path for a workspace, keyed by the supervisor stable
/// per-workspace name. Prefers `$XDG_RUNTIME_DIR` (world-unreadable by
/// construction) but falls back to `/tmp` when that directory is not
/// writable — notably in the self-hosting bootstrap, where only the
/// Wayland socket is mounted at the runtime dir path.
pub fn socket_path(container_name: &str) -> PathBuf {
    runtime_socket(&format!("{container_name}-mcp.sock"))
}

/// Any IDE socket a container has to be able to reach, by file name.
///
/// The directory choice is the shared part and the only subtle part — the
/// MCP socket is no longer the only thing bind-mounted into a container
/// (the auth proxy's socket rides in beside it), and a second copy of this
/// search order would be a second thing to get wrong.
pub fn runtime_socket(file_name: &str) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Inside Flatpak, /run/user/U and /tmp are sandbox-private; the app
    // dir is the one runtime path host-side processes can also see.
    if let (Ok(id), Ok(runtime)) = (
        std::env::var("FLATPAK_ID"),
        std::env::var("XDG_RUNTIME_DIR"),
    ) {
        candidates.push(PathBuf::from(runtime).join("app").join(id));
    }
    candidates.extend(std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from));
    candidates.push(PathBuf::from("/tmp"));
    for dir in &candidates {
        let probe = dir.join(format!(".taste-probe-{file_name}"));
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return dir.join(file_name);
        }
    }
    Path::new("/tmp").join(file_name)
}
