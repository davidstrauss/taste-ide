//! The IDE's own SSH agent, for the Pull and the Push the user presses.
//!
//! Every question those steps run into belongs on the banner's strip
//! (`askpass`), and ssh routes its own there. An agent's are another
//! matter: a key the desktop's agent holds is signed for in the agent's
//! process, with the agent's `SSH_ASKPASS`, so a security key's "confirm
//! user presence" came up as GNOME's own dialog — asking, before it could
//! even say so, to inhibit the desktop's shortcuts — while the IDE's
//! strip stood empty (David, 2026-09-28: "I wanted to collect the
//! passphrase and prompt to touch *only* in the banner").
//!
//! So the IDE runs an agent of its own, one per workspace, started the
//! first time a deliberate step needs it and ended with the IDE: an
//! `ssh-agent -D` whose `SSH_ASKPASS` is the IDE's helper. Before a step,
//! the remote's identity files (`ssh -G`, as `taste_git::presence` reads
//! them) that the agent does not hold yet are added to it, so a key with
//! a passphrase is asked for once — on the strip — and not again until
//! the IDE quits; the step then signs through that agent, and a key that
//! wants a touch says so through the same helper, on the same strip.
//! Pointed at by `SSH_AUTH_SOCK` alone, so the user's own ssh config and
//! `core.sshCommand` stand. The desktop's agent is left alone, and ssh
//! outside the IDE still uses it.
//!
//! On the host, in the user's runtime directory, as the fetch and the push
//! are: nothing an agent or a container runs is given that directory
//! (`taste_acp::sandbox`), so the keys it holds stay on this side of the
//! boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// A running agent and the socket it answers on.
struct Agent {
    socket: PathBuf,
    child: Child,
}

fn agents() -> &'static Mutex<HashMap<PathBuf, Agent>> {
    static AGENTS: OnceLock<Mutex<HashMap<PathBuf, Agent>>> = OnceLock::new();
    AGENTS.get_or_init(Default::default)
}

fn socket_for(workspace_root: &Path) -> PathBuf {
    taste_core::mcp::runtime_socket(&format!(
        "taste-{}-ssh-agent.sock",
        taste_core::environment::workspace_key(workspace_root)
    ))
}

/// The environment a deliberate network step runs with: the askpass
/// helper's, and the IDE's agent when the remote is reached over SSH. Off
/// the GTK thread: it may start a process, and adding a key with a
/// passphrase waits for the user's answer.
pub async fn network_env(
    workspace_root: PathBuf,
    remote_url: Option<String>,
) -> Vec<(String, String)> {
    let run = crate::runtime::runtime().spawn_blocking(move || {
        let mut envs = crate::askpass::env_for(&workspace_root);
        let Some(target) = remote_url
            .as_deref()
            .and_then(taste_git::presence::ssh_target)
        else {
            return envs;
        };
        let Some(socket) = ensure_agent(&workspace_root, &envs) else {
            return envs;
        };
        load_identities(&socket, &target, &envs);
        envs.push(("SSH_AUTH_SOCK".to_string(), socket.display().to_string()));
        envs
    });
    run.await.unwrap_or_default()
}

/// The workspace's agent, started if it is not running.
fn ensure_agent(workspace_root: &Path, askpass_env: &[(String, String)]) -> Option<PathBuf> {
    let mut agents = agents().lock().unwrap();
    if let Some(agent) = agents.get_mut(workspace_root) {
        if matches!(agent.child.try_wait(), Ok(None)) && agent.socket.exists() {
            return Some(agent.socket.clone());
        }
        agents.remove(workspace_root);
    }
    let socket = socket_for(workspace_root);
    // A socket left by an agent of an IDE that did not end cleanly; the
    // new agent refuses to bind over it.
    let _ = std::fs::remove_file(&socket);
    let mut command = Command::new("ssh-agent");
    command
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .envs(askpass_env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Ended with the IDE, however the IDE ends: the agent holds decrypted
    // keys, and an agent that outlived the window would go on holding them.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("the IDE's ssh-agent did not start: {e}");
            return None;
        }
    };
    // `-D` stays in the foreground, so the socket appears a moment after
    // the process does.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !socket.exists() {
        if std::time::Instant::now() > deadline {
            tracing::warn!("the IDE's ssh-agent never opened {}", socket.display());
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    tracing::info!("the IDE's ssh-agent is up on {}", socket.display());
    agents.insert(
        workspace_root.to_path_buf(),
        Agent {
            socket: socket.clone(),
            child,
        },
    );
    Some(socket)
}

/// Add to the agent every identity ssh would offer `target` that it does
/// not hold yet. A key with a passphrase asks for it through the helper;
/// one the user declines is left out, and ssh reads that file itself.
fn load_identities(socket: &Path, target: &str, askpass_env: &[(String, String)]) {
    let config = Command::new("ssh")
        .args(["-G", target])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let files = taste_git::presence::identity_files(&config, home.as_deref());
    let held = Command::new("ssh-add")
        .arg("-L")
        .env("SSH_AUTH_SOCK", socket)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    for file in files {
        if !file.is_file() || file.extension().is_some_and(|ext| ext == "pub") {
            continue;
        }
        if public_key(&file).is_some_and(|key| held_by(&held, &key)) {
            continue;
        }
        let added = Command::new("ssh-add")
            .arg(&file)
            .envs(askpass_env.iter().cloned())
            .env("SSH_AUTH_SOCK", socket)
            .stdin(Stdio::null())
            .output();
        match added {
            Ok(out) if out.status.success() => {
                tracing::info!("the IDE's ssh-agent holds {}", file.display())
            }
            Ok(out) => tracing::info!(
                "{} was not added to the IDE's ssh-agent: {}",
                file.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!("ssh-add did not run: {e}"),
        }
    }
}

/// The key a private key file's `.pub` names: its type and its base64,
/// the two fields an agent's `ssh-add -L` listing also carries.
fn public_key(private: &Path) -> Option<String> {
    let mut public = private.as_os_str().to_owned();
    public.push(".pub");
    let text = std::fs::read_to_string(PathBuf::from(public)).ok()?;
    key_fields(text.lines().next()?)
}

fn key_fields(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    Some(format!("{} {}", fields.next()?, fields.next()?))
}

/// Whether an `ssh-add -L` listing holds `key` (type and base64).
fn held_by(listing: &str, key: &str) -> bool {
    listing
        .lines()
        .filter_map(key_fields)
        .any(|held| held == key)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_key_is_held_when_its_type_and_body_are_listed() {
        let listing = "sk-ssh-ed25519@openssh.com AAAAGnNr ssh:\n\
                       ssh-ed25519 AAAAC3Nz user@host\n";
        assert!(super::held_by(listing, "ssh-ed25519 AAAAC3Nz"));
        assert!(super::held_by(
            listing,
            "sk-ssh-ed25519@openssh.com AAAAGnNr"
        ));
        assert!(!super::held_by(listing, "ssh-ed25519 AAAAother"));
        assert!(!super::held_by(
            "The agent has no identities.\n",
            "ssh-ed25519 AAAAC3Nz"
        ));
    }
}
