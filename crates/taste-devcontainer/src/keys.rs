//! **The keys a workspace reaches its VMs with.**
//!
//! Two key pairs, both made by the IDE and both the project's:
//!
//! - the **identity** the IDE logs into the guest with (`id_ed25519`). Its
//!   public half goes into the guest's Ignition; its private half never
//!   leaves the host, and the guest never holds a credential of the user's.
//!   That is the boundary this whole design defends (CLAUDE.md → "The
//!   boundary is the host, not the agent").
//! - the guest's own **host key** (`host_ed25519`), installed into the
//!   guest by Ignition. Generating it here rather than letting sshd mint one
//!   on first boot means the IDE knows the key before the VM exists, so
//!   `known_hosts` can be written ahead of the first connection and a VM
//!   recreated at the same port is not a "REMOTE HOST IDENTIFICATION HAS
//!   CHANGED" mystery. One host key per workspace serves every VM in its
//!   pool; the `[127.0.0.1]:PORT` line is what tells them apart.
//!
//! They live in the workspace's own state directory, beside
//! `anthropic.json` — the provisioner-credential slot docs/ENVIRONMENTS.md
//! → "VM provisioners" names — and so inside the backup's exclusion, for
//! the same reason the Anthropic credential is.
//!
//! `ssh-keygen` does the generating, on the host, through the same wrapper
//! every host program is reached by: the key files must be readable by the
//! host's `ssh` and by podman's ssh client, so they are host paths whatever
//! process wrote them.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// A guest's ssh host key, as the two files sshd wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub private: String,
    pub public: String,
}

/// The key material for one workspace's VMs.
#[derive(Debug, Clone)]
pub struct Keys {
    dir: PathBuf,
    sandboxed: bool,
}

impl Keys {
    /// Under the workspace's state directory: `<state>/guest/`.
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::at(taste_core::state::workspace_state_dir(workspace_root).join("guest"))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            sandboxed: taste_core::podman::sandboxed(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The private identity the IDE logs in with.
    pub fn identity(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }

    /// Its public half, one line, for Ignition.
    pub fn identity_public(&self) -> Result<String> {
        let path = self.identity().with_extension("pub");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(text.trim().to_string())
    }

    fn host_key_path(&self) -> PathBuf {
        self.dir.join("host_ed25519")
    }

    /// The guest's host key, both halves.
    pub fn host_key(&self) -> Result<HostKey> {
        let private_path = self.host_key_path();
        let public_path = private_path.with_extension("pub");
        Ok(HostKey {
            private: std::fs::read_to_string(&private_path)
                .with_context(|| format!("reading {}", private_path.display()))?,
            public: std::fs::read_to_string(&public_path)
                .with_context(|| format!("reading {}", public_path.display()))?
                .trim()
                .to_string(),
        })
    }

    /// The `known_hosts` file naming every VM of this workspace by port.
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    /// Make both key pairs if they are not there. Idempotent; an existing
    /// key is never regenerated, because every VM in the pool was built to
    /// trust it.
    pub async fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        private_dir(&self.dir)?;
        for (path, comment) in [
            (self.identity(), "taste-ide workspace identity"),
            (self.host_key_path(), "taste-ide guest host key"),
        ] {
            if !path.exists() {
                self.keygen(&path, comment).await?;
            }
        }
        Ok(())
    }

    async fn keygen(&self, path: &Path, comment: &str) -> Result<()> {
        let (program, args) = taste_core::podman::host_argv(
            self.sandboxed,
            "ssh-keygen",
            [
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                comment,
                "-f",
                &path.display().to_string(),
            ],
        );
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .context("running ssh-keygen")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// The `known_hosts` line for a VM at `port`.
    fn known_hosts_line(&self, port: u16) -> Result<String> {
        Ok(format!("[127.0.0.1]:{port} {}", self.host_key()?.public))
    }

    /// Record that the VM at `port` presents this workspace's host key.
    /// Appending twice writes once.
    pub fn record_host(&self, port: u16) -> Result<()> {
        let line = self.known_hosts_line(port)?;
        let path = self.known_hosts();
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.lines().any(|l| l == line) {
            return Ok(());
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Forget the VM at `port` — its line goes, nothing else does.
    pub fn forget_host(&self, port: u16) -> Result<()> {
        let path = self.known_hosts();
        let Ok(existing) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        let prefix = format!("[127.0.0.1]:{port} ");
        let kept: Vec<&str> = existing
            .lines()
            .filter(|l| !l.starts_with(&prefix))
            .collect();
        let mut text = kept.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// `ssh` to the VM at `port`, with this workspace's identity and
    /// `known_hosts` and nothing of the user's: no agent, no default keys,
    /// no `~/.ssh/known_hosts`. The command to run over there follows.
    pub fn ssh_argv<I, S>(&self, port: u16, remote: I) -> (String, Vec<String>)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args: Vec<String> = vec![
            "-i".into(),
            self.identity().display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "IdentityAgent=none".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts().display()),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-p".into(),
            port.to_string(),
            format!("{}@127.0.0.1", crate::provision::GUEST_USER),
        ];
        args.extend(remote.into_iter().map(Into::into));
        taste_core::podman::host_argv(self.sandboxed, "ssh", args)
    }
}

impl Keys {
    /// The `GIT_SSH_COMMAND` for a git talking to this workspace's VMs:
    /// the same options as [`Self::ssh_argv`], without the host and port,
    /// which git supplies from the URL. Shell-quoted, because git runs it
    /// through a shell.
    pub fn git_ssh_command(&self) -> String {
        let quote = |s: String| format!("'{}'", s.replace('\'', "'\\''"));
        [
            "ssh".to_string(),
            "-i".into(),
            quote(self.identity().display().to_string()),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "IdentityAgent=none".into(),
            "-o".into(),
            quote(format!(
                "UserKnownHostsFile={}",
                self.known_hosts().display()
            )),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
        ]
        .join(" ")
    }
}

/// `0700` on the directory that holds private keys.
fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_with_host_key(dir: &Path) -> Keys {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("host_ed25519"), "PRIVATE\n").unwrap();
        std::fs::write(
            dir.join("host_ed25519.pub"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostKey taste-ide guest host key\n",
        )
        .unwrap();
        Keys {
            dir: dir.to_path_buf(),
            sandboxed: false,
        }
    }

    /// The keys are the workspace's, in its state directory, beside the
    /// Anthropic credential — inside the backup's exclusion.
    #[test]
    fn the_keys_live_in_the_workspaces_state_directory() {
        let root = Path::new("/work/some-project");
        let keys = Keys::for_workspace(root);
        let state = taste_core::state::workspace_state_dir(root);
        assert!(keys.dir().starts_with(&state), "{}", keys.dir().display());
        assert!(keys.dir().ends_with("guest"));
        assert!(keys.identity().ends_with("guest/id_ed25519"));
        assert!(keys.known_hosts().ends_with("guest/known_hosts"));
    }

    /// One line per port, written once however often it is recorded, and
    /// forgotten without touching the others.
    #[test]
    fn known_hosts_is_one_line_per_port_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys_with_host_key(dir.path());
        keys.record_host(40001).unwrap();
        keys.record_host(40001).unwrap();
        keys.record_host(40002).unwrap();
        let text = std::fs::read_to_string(keys.known_hosts()).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("[127.0.0.1]:40001 ssh-ed25519 "), "{text}");
        keys.forget_host(40001).unwrap();
        let text = std::fs::read_to_string(keys.known_hosts()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(":40002 "), "{text}");
        // Forgetting a port that was never there is not an error.
        keys.forget_host(1).unwrap();
    }

    /// Nothing of the user's: not their agent, not their default keys, not
    /// their known_hosts. The VM is the project's, and so is the trust.
    #[test]
    fn ssh_uses_only_the_workspaces_own_material() {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys_with_host_key(dir.path());
        let (program, args) = keys.ssh_argv(40001, ["git", "init", "/x"]);
        assert_eq!(program, "ssh");
        let joined = args.join(" ");
        assert!(joined.contains("IdentitiesOnly=yes"), "{joined}");
        assert!(joined.contains("IdentityAgent=none"), "{joined}");
        assert!(
            joined.contains(&format!(
                "UserKnownHostsFile={}",
                keys.known_hosts().display()
            )),
            "{joined}"
        );
        assert!(joined.contains("StrictHostKeyChecking=yes"), "{joined}");
        assert!(
            joined.ends_with("-p 40001 core@127.0.0.1 git init /x"),
            "{joined}"
        );

        // git gets the same trust, quoted for its shell, and no host or
        // port — those come from the URL.
        let command = keys.git_ssh_command();
        assert!(command.starts_with("ssh -i '"), "{command}");
        assert!(command.contains("IdentitiesOnly=yes"), "{command}");
        assert!(command.contains("'UserKnownHostsFile="), "{command}");
        assert!(command.contains("BatchMode=yes"), "{command}");
        assert!(!command.contains("127.0.0.1"), "{command}");
    }
}
