//! The devcontainer lifecycle state machine.
//!
//! ```text
//! NoConfig → ConfigDetected → Building → Starting → Running
//!                  ↑_____________________________↓
//!                      pending changes (config drift)
//! ```
//!
//! The supervisor never restarts the IDE and never touches agent sessions:
//! a reload tears down and recreates only the container, then re-points the
//! shared [`ExecContext`] so *new* terminals and commands land inside it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::event::DevcontainerStateEvent;
use taste_core::{Event, EventBus, ExecContext};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{config::lifecycle_commands, config_hash, DevcontainerConfig};

const LOG_RING_CAPACITY: usize = 2000;

/// One podman resource associated with this workspace's devcontainer, for
/// the environment view (and the read-only MCP mirror).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceInfo {
    pub kind: ResourceKind,
    pub name: String,
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Container,
    Image,
    Volume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorState {
    NoConfig,
    ConfigDetected,
    Building,
    Starting,
    Running { container_id: String },
    Failed { message: String },
    Stopped,
}

impl SupervisorState {
    fn to_event(&self) -> DevcontainerStateEvent {
        match self {
            SupervisorState::NoConfig => DevcontainerStateEvent::NoConfig,
            SupervisorState::ConfigDetected => DevcontainerStateEvent::ConfigDetected,
            SupervisorState::Building => DevcontainerStateEvent::Building,
            SupervisorState::Starting => DevcontainerStateEvent::Starting,
            SupervisorState::Running { container_id } => DevcontainerStateEvent::Running {
                container_id: container_id.clone(),
            },
            SupervisorState::Failed { message } => DevcontainerStateEvent::Failed {
                message: message.clone(),
            },
            SupervisorState::Stopped => DevcontainerStateEvent::Stopped,
        }
    }
}

pub struct Supervisor {
    root: PathBuf,
    events: EventBus,
    exec: ExecContext,
    state: Mutex<SupervisorState>,
    /// Hash of the config the running container was created from.
    running_hash: Mutex<Option<String>>,
    pending: AtomicBool,
    logs: Mutex<VecDeque<String>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Serializes reload/stop/nuke: concurrent lifecycle operations (banner
    /// click + agent MCP reload) would interleave podman commands.
    lifecycle: tokio::sync::Mutex<()>,
    /// True while running inside a Flatpak sandbox (podman lives on the host).
    sandboxed: bool,
    /// True when the IDE itself runs inside a container (self-hosting
    /// bootstrap): the environment is already up, and lifecycle operations
    /// on it must happen from the host IDE instead. No container runtime is
    /// forwarded in — that would put host container creation (arbitrary
    /// mounts, i.e. host root) within reach of the agent and of the repo's
    /// own build.
    inside: bool,
}

fn exists_containerenv() -> bool {
    std::path::Path::new("/run/.containerenv").exists()
        || std::path::Path::new("/.dockerenv").exists()
}

impl Supervisor {
    pub fn new(root: PathBuf, events: EventBus, exec: ExecContext) -> Arc<Self> {
        Self::with_inside(root, events, exec, exists_containerenv())
    }

    /// Test seam: the test suite itself runs in a container, which must not
    /// flip every unit test into self-hosting semantics.
    #[doc(hidden)]
    pub fn new_outside_container_for_tests(
        root: PathBuf,
        events: EventBus,
        exec: ExecContext,
    ) -> Arc<Self> {
        Self::with_inside(root, events, exec, false)
    }

    fn with_inside(root: PathBuf, events: EventBus, exec: ExecContext, inside: bool) -> Arc<Self> {
        Arc::new(Self {
            root,
            events,
            exec,
            state: Mutex::new(SupervisorState::NoConfig),
            running_hash: Mutex::new(None),
            pending: AtomicBool::new(false),
            logs: Mutex::new(VecDeque::new()),
            watcher: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            sandboxed: std::path::Path::new("/.flatpak-info").exists(),
            inside,
        })
    }

    pub fn state(&self) -> SupervisorState {
        self.state.lock().unwrap().clone()
    }

    pub fn pending_changes(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// Test seam: force the pending-changes flag, so the confirmation gate
    /// keyed on it can be exercised without a running container to drift
    /// against.
    #[doc(hidden)]
    pub fn set_pending_for_tests(&self, pending: bool) {
        self.set_pending(pending);
    }

    /// Hash of the config the running container was built from, if running.
    pub fn running_hash(&self) -> Option<String> {
        self.running_hash.lock().unwrap().clone()
    }

    /// Last `n` lines of build/startup output (for the MCP `devcontainer_logs`
    /// tool and the supervisor console tab's backfill).
    pub fn logs_tail(&self, n: usize) -> Vec<String> {
        let logs = self.logs.lock().unwrap();
        logs.iter().rev().take(n).rev().cloned().collect()
    }

    fn set_state(&self, state: SupervisorState) {
        *self.state.lock().unwrap() = state.clone();
        self.events
            .publish(Event::DevcontainerState(state.to_event()));
    }

    fn set_pending(&self, pending: bool) {
        if self.pending.swap(pending, Ordering::SeqCst) != pending {
            self.events
                .publish(Event::DevcontainerPendingChanges { pending });
        }
    }

    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_RING_CAPACITY {
            logs.pop_front();
        }
        logs.push_back(line.clone());
        drop(logs);
        self.events.publish(Event::DevcontainerLog(line));
    }

    /// Re-evaluate config presence and drift. Called at startup and by the
    /// file watcher on every relevant filesystem event.
    pub fn recheck(&self) -> Result<()> {
        // Self-hosting FALLBACK (no reachable runtime): we ARE the
        // devcontainer, running by definition; drift is managed from the
        // host IDE. With a forwarded podman socket this branch is skipped
        // and the devcontainer is supervised as a real sibling.
        if self.inside {
            if self.state()
                != (SupervisorState::Running {
                    container_id: "self".into(),
                })
            {
                self.set_state(SupervisorState::Running {
                    container_id: "self".into(),
                });
            }
            self.set_pending(false);
            return Ok(());
        }
        let config = DevcontainerConfig::discover(&self.root)?;
        if config.is_some() {
            // A config that appeared after startup must also be watched
            // (the agent's whole job in safe mode is creating it).
            self.watch_devcontainer_dir();
        }
        let current = self.state();
        match (&config, &current) {
            (None, SupervisorState::Running { .. }) => {
                // Config deleted under a running container: that is drift.
                self.set_pending(true);
            }
            (None, _) => {
                self.set_state(SupervisorState::NoConfig);
                self.set_pending(false);
            }
            (Some(config), SupervisorState::Running { .. }) => {
                let hash = config_hash(config)?;
                let drift = self.running_hash().as_deref() != Some(hash.as_str());
                self.set_pending(drift);
            }
            (Some(config), SupervisorState::NoConfig) => {
                // A previous IDE instance may have left the container
                // running: adopt it instead of sitting in safe mode next
                // to a healthy environment.
                if let Some(container_id) = self.adopt_running_container(config) {
                    self.set_state(SupervisorState::Running { container_id });
                } else {
                    self.set_state(SupervisorState::ConfigDetected);
                    self.set_pending(false);
                }
            }
            // A config edit after a failure (or stop) is the fix loop in
            // action: return to ConfigDetected so Start reappears and MCP
            // reports progress instead of a stale failure.
            (Some(_), SupervisorState::Failed { .. }) | (Some(_), SupervisorState::Stopped) => {
                self.set_state(SupervisorState::ConfigDetected);
                self.set_pending(false);
            }
            _ => {}
        }
        Ok(())
    }

    /// Idempotently watch `.devcontainer/` once it exists. notify tolerates
    /// re-watching the same path; errors are non-fatal.
    fn watch_devcontainer_dir(&self) {
        use notify::{RecursiveMode, Watcher};
        let dc_dir = self.root.join(".devcontainer");
        if !dc_dir.is_dir() {
            return;
        }
        if let Some(watcher) = self.watcher.lock().unwrap().as_mut() {
            let _ = watcher.watch(&dc_dir, RecursiveMode::Recursive);
        }
    }

    /// Start watching the config locations for drift.
    pub fn start_watching(self: &Arc<Self>) -> Result<()> {
        use notify::{RecursiveMode, Watcher};
        let this = Arc::downgrade(self);
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if res.is_err() {
                    return;
                }
                if let Some(this) = this.upgrade() {
                    let _ = this.recheck();
                }
            })?;
        // Watch the root non-recursively (catches .devcontainer.json and
        // creation/removal of .devcontainer itself) and the .devcontainer
        // directory recursively when present.
        watcher.watch(&self.root, RecursiveMode::NonRecursive)?;
        let dc_dir = self.root.join(".devcontainer");
        if dc_dir.is_dir() {
            watcher.watch(&dc_dir, RecursiveMode::Recursive)?;
        }
        *self.watcher.lock().unwrap() = Some(watcher);
        Ok(())
    }

    /// At startup: if this workspace's container is already running,
    /// point execution into it and report honest drift from the config
    /// hash it was created with (stored as a container label).
    fn adopt_running_container(&self, config: &DevcontainerConfig) -> Option<String> {
        let name = self.container_name();
        let sandboxed = std::path::Path::new("/.flatpak-info").exists();
        let mut command = if sandboxed {
            let mut c = std::process::Command::new("flatpak-spawn");
            c.arg("--host").arg("podman");
            c
        } else {
            std::process::Command::new("podman")
        };
        let output = command
            .args([
                "inspect",
                "--format",
                r#"{{.State.Running}}|{{index .Config.Labels "taste.config-hash"}}"#,
                &name,
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None; // no such container
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let (running, started_hash) = text.trim().split_once('|')?;
        if running != "true" {
            return None;
        }
        self.exec
            .set_container(name.clone(), config.workspace_folder().to_string());
        *self.running_hash.lock().unwrap() = Some(started_hash.to_string());
        let drift = config_hash(config)
            .map(|hash| hash != started_hash)
            .unwrap_or(true);
        self.set_pending(drift);
        self.log(format!("adopted running container {name}"));
        Some(name)
    }

    /// Deterministic container name for this workspace.
    pub fn container_name(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.root.to_string_lossy().as_bytes());
        let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("taste-{short}")
    }

    fn image_tag(&self) -> String {
        format!("{}-image", self.container_name())
    }

    /// Podman always runs on the host, even when the IDE is sandboxed.
    fn podman(&self, args: &[String]) -> tokio::process::Command {
        if self.sandboxed {
            let mut cmd = tokio::process::Command::new("flatpak-spawn");
            cmd.arg("--host").arg("podman").args(args);
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("podman");
            cmd.args(args);
            cmd
        }
    }

    /// Run a podman command, streaming its output into the log ring.
    async fn run_logged(&self, args: Vec<String>) -> Result<()> {
        self.log(format!("$ podman {}", args.join(" ")));
        let mut child = self
            .podman(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawning podman")?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();
        // Branch guards keep a closed stream from spinning the select loop.
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out_lines.next_line(), if !out_done => match line? {
                    Some(l) => self.log(l),
                    None => out_done = true,
                },
                line = err_lines.next_line(), if !err_done => match line? {
                    Some(l) => self.log(l),
                    None => err_done = true,
                },
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!(
                "podman {} failed: {status}",
                args.first().cloned().unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Run a podman command and capture stdout (for ids and inspection).
    async fn run_captured(&self, args: Vec<String>) -> Result<String> {
        let output = self
            .podman(&args)
            .output()
            .await
            .context("running podman")?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("podman {}: {err}", args.join(" "));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Full (re)build-and-start cycle. Idempotent: tears down any previous
    /// container for this workspace first. Editor buffers, git state, and
    /// agent sessions are structurally out of reach of this function — the
    /// design's "never interrupt the AI session" guarantee.
    pub async fn reload(&self) -> Result<()> {
        if self.inside {
            bail!(
                "the IDE is running inside this devcontainer; rebuild it from \
                 the host-side IDE (a container cannot rebuild itself, and no \
                 container runtime is forwarded in here — that would hand the \
                 agent and the repo's own build the host)"
            );
        }
        // One lifecycle operation at a time: a second reload (agent via MCP,
        // second button press) waits instead of interleaving podman calls.
        let _lifecycle = self.lifecycle.lock().await;
        // Every early error must land in a *state* — the banner and MCP
        // read states, not Results.
        let config = match DevcontainerConfig::discover(&self.root) {
            Ok(Some(c)) => c,
            Ok(None) => {
                self.set_state(SupervisorState::NoConfig);
                bail!("no devcontainer configuration found");
            }
            Err(e) => {
                self.log(format!("config error: {e:#}"));
                self.set_state(SupervisorState::Failed {
                    message: e.to_string(),
                });
                return Err(e);
            }
        };
        if let Err(e) = config.validate() {
            self.log(format!("config invalid: {e:#}"));
            self.set_state(SupervisorState::Failed {
                message: e.to_string(),
            });
            return Err(e);
        }
        // The repo is untrusted: refuse configs that reach outside the
        // workspace or weaken isolation. The error surfaces in the banner,
        // the log tab, and MCP — fixable from safe mode.
        if let Err(e) = crate::security::validate_security(&config, &self.root) {
            self.log(format!("refused: {e:#}"));
            self.set_state(SupervisorState::Failed {
                message: e.to_string(),
            });
            return Err(e);
        }
        let hash = config_hash(&config)?;
        let name = self.container_name();

        // Tear down any previous instance (ignore "no such container").
        self.exec.set_host();
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name.clone(),
            ])
            .await;

        // Build or pull the image.
        self.set_state(SupervisorState::Building);
        let image = if let Some(dockerfile) = config.dockerfile_path() {
            let tag = self.image_tag();
            // Build from a STAGED copy, never from the live directory.
            // Validation alone cannot hold here: the config scope is the
            // one thing an agent may write in either mode, so a directory
            // checked at parse can be a symlink by the time podman reads
            // it. Staging closes that window by construction — walk it
            // once, refuse symlinks on the way, build from bytes already
            // ours.
            let staged = stage_build_context(&config.build_context(), &name)?;
            let staged_dockerfile = dockerfile
                .file_name()
                .map(|f| staged.join(f))
                .unwrap_or_else(|| staged.join("Containerfile"));
            let mut args = vec![
                "build".into(),
                "-t".into(),
                tag.clone(),
                "-f".into(),
                staged_dockerfile.display().to_string(),
                // A RUN step cannot reach the host filesystem, but it can
                // still fork, allocate and sit there. Bound all three.
                "--cap-drop=all".into(),
                "--pids-limit".into(),
                "2048".into(),
                "--memory".into(),
                "8g".into(),
            ];
            if let Some(build) = &config.build {
                for (k, v) in &build.args {
                    args.push("--build-arg".into());
                    args.push(format!("{k}={v}"));
                }
            }
            args.push(staged.display().to_string());
            self.run_logged(args).await.inspect_err(|e| {
                self.set_state(SupervisorState::Failed {
                    message: e.to_string(),
                })
            })?;
            tag
        } else {
            let image = config.image.clone().unwrap();
            self.run_logged(vec!["pull".into(), image.clone()])
                .await
                .inspect_err(|e| {
                    self.set_state(SupervisorState::Failed {
                        message: e.to_string(),
                    })
                })?;
            image
        };

        // Start the container.
        self.set_state(SupervisorState::Starting);
        let workdir = config.workspace_folder().to_string();
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("taste.config-hash={hash}"),
            "--workdir".into(),
            workdir.clone(),
        ];
        match &config.workspace_mount {
            Some(mount) => {
                args.push("--mount".into());
                args.push(
                    mount.replace("${localWorkspaceFolder}", &self.root.display().to_string()),
                );
            }
            None => {
                args.push("-v".into());
                args.push(format!("{}:{}:Z", self.root.display(), workdir));
            }
        }
        for mount in &config.mounts {
            if let Some(m) = mount.as_str() {
                args.push("--mount".into());
                args.push(m.replace("${localWorkspaceFolder}", &self.root.display().to_string()));
            }
        }
        for (k, v) in &config.container_env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        // forwardPorts: published on localhost only — services in the
        // container become reachable from the host without exposing them
        // to the network.
        for port in &config.forward_ports {
            args.push("-p".into());
            args.push(format!("127.0.0.1:{port}:{port}"));
        }
        for arg in &config.run_args {
            if crate::security::STRIPPED_FLAGS.contains(&arg.as_str()) {
                self.log(format!(
                    "runArgs {arg} ignored — Docker needs it for systemd, rootless podman does not"
                ));
                continue;
            }
            args.push(arg.clone());
        }
        args.push(image);
        if config.override_command != Some(false) {
            args.push("sleep".into());
            args.push("infinity".into());
        }
        let container_id = self.run_captured(args).await.inspect_err(|e| {
            self.set_state(SupervisorState::Failed {
                message: e.to_string(),
            })
        })?;
        self.log(format!("container started: {container_id}"));

        // Git identity: inherited from the host BEFORE the lifecycle hooks
        // (which may themselves commit). A container that already has one —
        // baked into the image or persisted in a home volume — keeps it.
        self.inherit_git_identity(&name, config.effective_user())
            .await;

        // Lifecycle hooks, in spec order.
        for hook in [
            &config.on_create_command,
            &config.post_create_command,
            &config.post_start_command,
        ]
        .into_iter()
        .flatten()
        {
            for argv in lifecycle_commands(hook) {
                let mut exec_args: Vec<String> =
                    vec!["exec".into(), "--workdir".into(), workdir.clone()];
                if let Some(user) = config.effective_user() {
                    exec_args.push("--user".into());
                    exec_args.push(user.to_string());
                }
                exec_args.push(name.clone());
                exec_args.extend(argv);
                self.run_logged(exec_args).await.inspect_err(|e| {
                    self.set_state(SupervisorState::Failed {
                        message: e.to_string(),
                    })
                })?;
            }
        }

        // Success: record the hash, clear drift, re-point execution.
        *self.running_hash.lock().unwrap() = Some(hash);
        self.set_pending(false);
        self.exec.set_container(name, workdir);
        self.set_state(SupervisorState::Running { container_id });
        Ok(())
    }

    /// Copy the host's `user.name`/`user.email` into the container's
    /// global git config, unless the container already has an identity.
    /// A fresh container otherwise refuses every commit — terminals,
    /// hooks, agents — with "Author identity unknown", an error whose
    /// answer the IDE already knows. Never fatal: a container without
    /// git installed must still start.
    async fn inherit_git_identity(&self, name: &str, user: Option<&str>) {
        let Some(identity) = taste_git::host_identity() else {
            return; // nothing to inherit — the host is equally anonymous
        };
        let exec = |tail: &[&str]| {
            let mut args: Vec<String> = vec!["exec".into()];
            if let Some(user) = user {
                args.push("--user".into());
                args.push(user.to_string());
            }
            args.push(name.to_string());
            args.extend(tail.iter().map(|s| s.to_string()));
            args
        };
        // An existing identity wins: `--get` exits non-zero when unset.
        let existing = self
            .run_captured(exec(&["git", "config", "--global", "--get", "user.email"]))
            .await;
        if existing.map(|out| !out.trim().is_empty()).unwrap_or(false) {
            return;
        }
        for (key, value) in [
            ("user.name", &identity.name),
            ("user.email", &identity.email),
        ] {
            if let Err(e) = self
                .run_captured(exec(&["git", "config", "--global", key, value]))
                .await
            {
                self.log(format!("git identity not inherited ({key}): {e}"));
                return;
            }
        }
        self.log(format!(
            "git identity inherited from host: {} <{}>",
            identity.name, identity.email
        ));
    }

    /// Stop and remove the container; execution falls back to the host.
    pub async fn stop(&self) -> Result<()> {
        if self.inside {
            bail!("cannot stop the container the IDE itself runs in");
        }
        let _lifecycle = self.lifecycle.lock().await;
        let name = self.container_name();
        self.log(format!("stopping {name}"));
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name,
            ])
            .await;
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        Ok(())
    }

    /// Nuke: remove the container *and* its image, so the next start is a
    /// from-scratch rebuild. Named volumes are deliberately untouched —
    /// they are caches with their own removal affordance.
    pub async fn nuke(&self) -> Result<()> {
        if self.inside {
            bail!("cannot nuke the container the IDE itself runs in");
        }
        let _lifecycle = self.lifecycle.lock().await;
        let name = self.container_name();
        self.log(format!("nuking {name}: removing container and image"));
        let _ = self
            .run_captured(vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                name,
            ])
            .await;
        let _ = self
            .run_captured(vec!["rmi".into(), "-f".into(), self.image_tag()])
            .await;
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        Ok(())
    }

    /// Remove one named volume — but only one this workspace's devcontainer
    /// config actually references. Anything else is refused: the
    /// environment view manages this environment, not podman at large.
    pub async fn remove_volume(&self, volume: &str) -> Result<()> {
        let allowed = DevcontainerConfig::discover(&self.root)?
            .map(|config| config.named_volumes())
            .unwrap_or_default();
        if !allowed.iter().any(|v| v == volume) {
            bail!("volume {volume} is not referenced by this workspace's devcontainer config");
        }
        self.log(format!("removing volume {volume}"));
        self.run_captured(vec![
            "volume".into(),
            "rm".into(),
            "-f".into(),
            volume.into(),
        ])
        .await?;
        Ok(())
    }

    /// Everything podman-side associated with this environment: the
    /// container, its image, and the config's named volumes.
    pub async fn list_resources(&self) -> Vec<ResourceInfo> {
        if self.inside {
            return vec![ResourceInfo {
                kind: ResourceKind::Container,
                name: "this container (self-hosted session)".into(),
                id: "self".into(),
                status: "running — manage from the host IDE".into(),
            }];
        }
        let mut resources = Vec::new();
        let name = self.container_name();

        if let Ok(out) = self
            .run_captured(vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("name=^{name}$"),
                "--format".into(),
                "{{.ID}}\t{{.Names}}\t{{.Status}}".into(),
            ])
            .await
        {
            for line in out.lines() {
                let mut fields = line.split('\t');
                if let (Some(id), Some(name), Some(status)) =
                    (fields.next(), fields.next(), fields.next())
                {
                    resources.push(ResourceInfo {
                        kind: ResourceKind::Container,
                        name: name.to_string(),
                        id: id.to_string(),
                        status: status.to_string(),
                    });
                }
            }
        }

        if let Ok(out) = self
            .run_captured(vec![
                "images".into(),
                "--filter".into(),
                format!("reference={}", self.image_tag()),
                "--format".into(),
                "{{.ID}}\t{{.Repository}}\t{{.Size}}".into(),
            ])
            .await
        {
            for line in out.lines() {
                let mut fields = line.split('\t');
                if let (Some(id), Some(repo), Some(size)) =
                    (fields.next(), fields.next(), fields.next())
                {
                    resources.push(ResourceInfo {
                        kind: ResourceKind::Image,
                        name: repo.to_string(),
                        id: id.to_string(),
                        status: size.to_string(),
                    });
                }
            }
        }

        let volumes = DevcontainerConfig::discover(&self.root)
            .ok()
            .flatten()
            .map(|config| config.named_volumes())
            .unwrap_or_default();
        for volume in volumes {
            let exists = self
                .run_captured(vec![
                    "volume".into(),
                    "ls".into(),
                    "-q".into(),
                    "--filter".into(),
                    format!("name={volume}"),
                ])
                .await
                .map(|out| out.lines().any(|l| l == volume))
                .unwrap_or(false);
            resources.push(ResourceInfo {
                kind: ResourceKind::Volume,
                name: volume,
                id: String::new(),
                status: if exists { "present" } else { "absent" }.to_string(),
            });
        }

        resources
    }
}

/// Copy a build context somewhere podman can read it and the repo cannot
/// change it, and hand back the staged path.
///
/// Regular files and directories only. **Symlinks are refused, not
/// followed**: a link is the one entry that can mean something outside the
/// tree being copied, and whether `COPY` would dereference it is podman
/// business we would rather not depend on. Refusing is also the honest
/// error — a devcontainer context that needs a symlink out of itself is
/// not machine-independent and would not survive being sent to Codespaces.
///
/// Staged fresh each build, under the IDE own cache rather than a
/// world-writable temp dir, so no other user can plant the bytes we are
/// about to build.
fn stage_build_context(source: &Path, name: &str) -> Result<PathBuf> {
    let staged = staging_root().join(name);
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)
        .with_context(|| format!("creating build staging dir {}", staged.display()))?;
    copy_context_into(source, &staged)?;
    Ok(staged)
}

fn staging_root() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".cache")
        })
        .join("taste-ide")
        .join("build-context")
}

fn copy_context_into(source: &Path, target: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("reading build context {}", source.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if kind.is_symlink() {
            bail!(
                "{}: symlinks are not allowed in a devcontainer build context — a link \
                 can point outside the repository, and a context that needs one is not \
                 portable",
                from.display()
            );
        } else if kind.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_context_into(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).with_context(|| format!("staging {}", from.display()))?;
        }
        // Anything else (fifo, socket, device) is silently skipped: it
        // cannot contribute to an image build.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(root: &std::path::Path) -> Arc<Supervisor> {
        Supervisor::new_outside_container_for_tests(
            root.to_path_buf(),
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
        )
    }

    fn write_config(root: &std::path::Path) {
        let dc = root.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "registry.example/img:1"}"#,
        )
        .unwrap();
    }

    #[test]
    fn recheck_walks_noconfig_to_configdetected() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        sup.recheck().unwrap();
        assert_eq!(sup.state(), SupervisorState::NoConfig);

        write_config(dir.path());
        sup.recheck().unwrap();
        assert_eq!(sup.state(), SupervisorState::ConfigDetected);
        assert!(!sup.pending_changes());
    }

    /// Staging is what makes the context ours: a directory validated at
    /// parse can be a symlink by the time podman reads it, and the config
    /// scope is the one thing an agent may write in either mode.
    #[test]
    fn staging_copies_the_context_and_refuses_symlinks() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n").unwrap();
        std::fs::create_dir(source.path().join("scripts")).unwrap();
        std::fs::write(source.path().join("scripts/setup.sh"), "echo hi\n").unwrap();

        let staged = stage_build_context(source.path(), "taste-test-ctx").unwrap();
        assert_eq!(
            std::fs::read_to_string(staged.join("Containerfile")).unwrap(),
            "FROM scratch\n"
        );
        assert!(
            staged.join("scripts/setup.sh").is_file(),
            "nested files come too"
        );

        // The escape a lexical check misses, and the reason staging exists.
        std::os::unix::fs::symlink("/etc", source.path().join("escape")).unwrap();
        let error = stage_build_context(source.path(), "taste-test-ctx")
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinks are not allowed"), "{error}");

        let _ = std::fs::remove_dir_all(staging_root().join("taste-test-ctx"));
    }

    /// Restaged every build: yesterday files must not ride along into
    /// today image.
    #[test]
    fn staging_is_fresh_each_time() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n").unwrap();
        let staged = stage_build_context(source.path(), "taste-test-fresh").unwrap();
        std::fs::write(staged.join("leftover"), "stale").unwrap();
        let staged = stage_build_context(source.path(), "taste-test-fresh").unwrap();
        assert!(!staged.join("leftover").exists());
        let _ = std::fs::remove_dir_all(staging_root().join("taste-test-fresh"));
    }

    #[test]
    fn drift_while_running_raises_pending() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let sup = make(dir.path());

        // Simulate a running container recorded at the current hash.
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        *sup.running_hash.lock().unwrap() = Some(config_hash(&config).unwrap());
        sup.set_state(SupervisorState::Running {
            container_id: "x".into(),
        });
        sup.recheck().unwrap();
        assert!(!sup.pending_changes());

        std::fs::write(
            dir.path().join(".devcontainer/devcontainer.json"),
            r#"{"image": "registry.example/img:2"}"#,
        )
        .unwrap();
        sup.recheck().unwrap();
        assert!(sup.pending_changes());
    }

    #[tokio::test]
    async fn remove_volume_refuses_unreferenced_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"image": "img", "mounts": ["source=my-cache,target=/c,type=volume"]}"#,
        )
        .unwrap();
        let sup = make(dir.path());
        // Not in the config: refused before podman is ever invoked.
        let err = sup.remove_volume("some-other-volume").await.unwrap_err();
        assert!(err.to_string().contains("not referenced"));
    }

    #[test]
    fn container_name_is_stable_per_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let a = make(dir.path()).container_name();
        let b = make(dir.path()).container_name();
        assert_eq!(a, b);
        assert!(a.starts_with("taste-"));
    }

    /// Self-hosting means the IDE's own container IS the environment, and
    /// lifecycle operations belong to a host-side IDE. There is no socket
    /// forwarded in that could make it a sibling — by design.
    #[tokio::test]
    async fn self_hosted_lifecycle_is_refused_not_remoted() {
        let dir = tempfile::tempdir().unwrap();
        let inside = Supervisor::with_inside(
            dir.path().to_path_buf(),
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
            true,
        );
        let error = inside.reload().await.unwrap_err().to_string();
        assert!(error.contains("host-side IDE"), "{error}");
        assert!(inside.stop().await.is_err());
        assert!(inside.nuke().await.is_err());
    }
}
