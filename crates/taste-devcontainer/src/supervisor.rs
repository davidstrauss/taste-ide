//! One environment's devcontainer lifecycle state machine.
//!
//! ```text
//! NoConfig → ConfigDetected → Building → Starting → Running
//!                  ↑_____________________________↓
//!                      pending changes (config drift)
//! ```
//!
//! A `Supervisor` supervises exactly **one environment** — never "the"
//! devcontainer. Its identity (which environment, whose workspace, which
//! checkout) is injected by the [`crate::EnvironmentRegistry`] rather than
//! derived from a single root, and every state mutex, drift flag, log ring
//! and watcher in here is per-environment by construction. That is why
//! there is no environment id threaded through the methods: an instance
//! *is* the environment.
//!
//! The supervisor never restarts the IDE and never touches agent sessions:
//! a reload tears down and recreates only its container, then re-points
//! that environment's [`ExecContext`] so *new* terminals and commands land
//! inside it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::environment::{self, EnvironmentId, LABEL_CONFIG_HASH, LABEL_ENV, LABEL_WORKSPACE};
use taste_core::event::DevcontainerStateEvent;
use taste_core::{Event, EventBus, ExecContext};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::hash::build_hash;
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

/// Which environment a [`Supervisor`] is, injected at construction.
///
/// Three facts, deliberately separate: the environment's slug, the
/// workspace every podman name is keyed by, and *this* environment's
/// checkout. For the primary environment the last two are the same path;
/// for every other environment the checkout is a clone under
/// `$XDG_STATE_HOME`. Nothing in the supervisor may re-derive one from
/// another — that conflation is exactly what made the single-environment
/// scheme unable to grow a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub id: EnvironmentId,
    pub workspace_root: PathBuf,
    pub root: PathBuf,
}

impl EnvironmentIdentity {
    /// The primary environment: the main checkout itself.
    pub fn primary(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            id: EnvironmentId::primary(),
            root: workspace_root.clone(),
            workspace_root,
        }
    }

    /// A non-primary environment, rooted at its clone.
    pub fn cloned(workspace_root: impl Into<PathBuf>, id: EnvironmentId) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            root: environment::env_repo_root(&workspace_root, &id),
            workspace_root,
            id,
        }
    }
}

pub struct Supervisor {
    env: EnvironmentIdentity,
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
    pub fn new(env: EnvironmentIdentity, events: EventBus, exec: ExecContext) -> Arc<Self> {
        Self::with_inside(env, events, exec, exists_containerenv())
    }

    /// Test seam: the test suite itself runs in a container, which must not
    /// flip every unit test into self-hosting semantics.
    #[doc(hidden)]
    pub fn new_outside_container_for_tests(
        env: EnvironmentIdentity,
        events: EventBus,
        exec: ExecContext,
    ) -> Arc<Self> {
        Self::with_inside(env, events, exec, false)
    }

    fn with_inside(
        env: EnvironmentIdentity,
        events: EventBus,
        exec: ExecContext,
        inside: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            env,
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

    /// Which environment this supervisor is.
    pub fn id(&self) -> &EnvironmentId {
        &self.env.id
    }

    /// This environment's checkout — the main one for the primary, a clone
    /// otherwise. Config discovery, security validation and the workspace
    /// bind all key off this, which is what makes an environment portable
    /// to a clone without a single conditional.
    pub fn root(&self) -> &Path {
        &self.env.root
    }

    /// The workspace this environment belongs to.
    pub fn workspace_root(&self) -> &Path {
        &self.env.workspace_root
    }

    /// This environment's execution target. One per environment; the
    /// workspace holds a handle to the primary's for the call sites that
    /// predate environments.
    pub fn exec(&self) -> &ExecContext {
        &self.exec
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
        self.events.publish(Event::DevcontainerState {
            env: self.env.id.clone(),
            state: state.to_event(),
        });
    }

    fn set_pending(&self, pending: bool) {
        if self.pending.swap(pending, Ordering::SeqCst) != pending {
            self.events.publish(Event::DevcontainerPendingChanges {
                env: self.env.id.clone(),
                pending,
            });
        }
    }

    /// The log ring is per-supervisor, so it is per-environment for free —
    /// no de-interleaving, no shared buffer. The event carries the id so a
    /// subscriber showing one environment's build can drop the rest.
    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_RING_CAPACITY {
            logs.pop_front();
        }
        logs.push_back(line.clone());
        drop(logs);
        self.events.publish(Event::DevcontainerLog {
            env: self.env.id.clone(),
            line,
        });
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
        let config = DevcontainerConfig::discover(&self.env.root)?;
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
                let hash = config_hash(config, &self.ide_mounts(config))?;
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
        let dc_dir = self.env.root.join(".devcontainer");
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
        watcher.watch(&self.env.root, RecursiveMode::NonRecursive)?;
        let dc_dir = self.env.root.join(".devcontainer");
        if dc_dir.is_dir() {
            watcher.watch(&dc_dir, RecursiveMode::Recursive)?;
        }
        *self.watcher.lock().unwrap() = Some(watcher);
        Ok(())
    }

    /// At startup: if this environment's container is already running,
    /// point execution into it and report honest drift from the config
    /// hash it was created with (stored as a container label).
    ///
    /// Reconciliation is by **label**, not by name lookup. Names are ours
    /// to compute and they will keep changing; the labels are the container's
    /// own claim about which workspace and environment it belongs to, so a
    /// container built by a build whose naming we no longer produce is still
    /// recognised — and a container that merely happens to sit at a name we
    /// would have chosen is not adopted.
    fn adopt_running_container(&self, config: &DevcontainerConfig) -> Option<String> {
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
                "ps",
                "--filter",
                &format!("label={LABEL_WORKSPACE}={}", self.workspace_key()),
                "--filter",
                &format!("label={LABEL_ENV}={}", self.env.id),
                "--format",
                &format!(r#"{{{{.Names}}}}|{{{{index .Labels "{LABEL_CONFIG_HASH}"}}}}"#),
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().find(|l| !l.trim().is_empty())?;
        let (name, started_hash) = line.trim().split_once('|')?;
        let (name, started_hash) = (name.to_string(), started_hash.to_string());
        self.exec
            .set_container(name.clone(), config.workspace_folder().to_string());
        *self.running_hash.lock().unwrap() = Some(started_hash.clone());
        let drift = config_hash(config, &self.ide_mounts(config))
            .map(|hash| hash != started_hash)
            .unwrap_or(true);
        self.set_pending(drift);
        self.log(format!("adopted running container {name}"));
        Some(name)
    }

    fn workspace_key(&self) -> String {
        environment::workspace_key(&self.env.workspace_root)
    }

    /// This environment's container name. Derived in one place for the
    /// whole IDE — see [`taste_core::environment`].
    pub fn container_name(&self) -> String {
        environment::env_container_name(&self.env.workspace_root, &self.env.id)
    }

    /// The labels every container and image of this environment carries.
    /// They are what reconciliation and cleanup enumerate by.
    fn resource_labels(&self) -> Vec<String> {
        vec![
            "--label".into(),
            format!("{LABEL_WORKSPACE}={}", self.workspace_key()),
            "--label".into(),
            format!("{LABEL_ENV}={}", self.env.id),
        ]
    }

    /// A repo-declared volume name, namespaced to this environment.
    fn namespaced_volume(&self, declared: &str) -> String {
        environment::env_config_volume(&self.env.workspace_root, &self.env.id, declared)
    }

    /// A repo-declared mount spec with its named volume namespaced.
    fn namespaced_mount(&self, mount: &str) -> String {
        crate::config::rewrite_volume_source(mount, |declared| self.namespaced_volume(declared))
    }

    /// The mounts the IDE adds on its own account, regardless of what the
    /// repo asked for. Separate from `start` so the same list can be
    /// HASHED — see `config_hash`. Change what is mounted and every running
    /// container goes stale by itself, which is the only version of this
    /// that survives someone forgetting.
    fn ide_mounts(&self, config: &DevcontainerConfig) -> Vec<String> {
        let workdir = config.workspace_folder().to_string();
        let mut mounts: Vec<String> = Vec::new();

        // The workspace a SECOND time, at its host path. That is what makes
        // every path an agent exchanges with the IDE mean the same thing on
        // both sides — no translation layer — and it keeps the agent
        // conversation history findable, since the adapter keys history by
        // working directory.
        let host_path = self.env.root.display().to_string();
        if host_path != workdir {
            mounts.push("-v".into());
            mounts.push(format!("{host_path}:{host_path}:Z"));
        }

        // The agent own home. A volume so credentials and history outlive a
        // rebuild; not /home/dev, which is the USER home in here. Per
        // ENVIRONMENT, not per machine: the old single global volume would
        // have put every environment's agent in one home directory.
        mounts.push("-v".into());
        mounts.push(format!(
            "{}:{}",
            environment::env_home_volume(&self.env.workspace_root, &self.env.id),
            taste_core::policy::AGENT_HOME_IN_DEVCONTAINER
        ));

        // The IDE MCP socket. `:z` (shared) is required, not decoration:
        // the socket lives in the host runtime dir and carries its label,
        // which `container_t` cannot touch — the bind appears in
        // /proc/self/mountinfo and every access is denied, so an agent in
        // here would come up with no IDE tools and no error explaining why.
        // Shared rather than private because the IDE and more than one
        // container all speak to it.
        //
        // Reachable by anything in the container, not just the agent: same
        // uid, so no file permission separates them. That is inside the
        // trust line (agent and container on one side, host on the other),
        // not an oversight.
        //
        // One socket per environment: the socket IS the caller's identity,
        // which is how phase 2b routes MCP tools to the right environment
        // without changing the wire.
        let socket = environment::env_socket_path(&self.env.workspace_root, &self.env.id);
        mounts.push("-v".into());
        mounts.push(format!("{}:{}:z", socket.display(), socket.display()));

        mounts
    }

    /// The image tag for a config — keyed by the BUILD hash, so every
    /// environment of this workspace whose config hashes the same shares
    /// one image instead of each holding its own copy.
    fn image_tag(&self, config: &DevcontainerConfig) -> Result<String> {
        Ok(environment::env_image_tag(&build_hash(config)?))
    }

    /// The image tag of the config currently on disk, if it builds one.
    /// `None` means "nothing for us to remove": no config, or a config that
    /// pulls a registry image rather than building.
    fn current_image_tag(&self) -> Option<String> {
        let config = DevcontainerConfig::discover(&self.env.root)
            .ok()
            .flatten()?;
        config.dockerfile_path()?;
        self.image_tag(&config).ok()
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
        let config = match DevcontainerConfig::discover(&self.env.root) {
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
        if let Err(e) = crate::security::validate_security(&config, &self.env.root) {
            self.log(format!("refused: {e:#}"));
            self.set_state(SupervisorState::Failed {
                message: e.to_string(),
            });
            return Err(e);
        }
        let hash = config_hash(&config, &self.ide_mounts(&config))?;
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
            let tag = self.image_tag(&config)?;
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
                // A RUN step cannot reach the host filesystem, but it
                // can still allocate and sit there. Capabilities it never
                // needs, and a memory ceiling it does.
                //
                // No --pids-limit: that is a `podman run` flag, not a
                // `podman build` one, and passing it fails the build —
                // which would strand the IDE in safe mode. Verified
                // against `podman build --help` rather than assumed. A
                // fork bomb in a RUN step is therefore still unbounded;
                // --ulimit may be the substitute, unverified.
                "--cap-drop=all".into(),
                "--memory".into(),
                "8g".into(),
                // The tag is shared between environments with identical
                // config, so the workspace tie an image needs for cleanup
                // rides on a label instead of on its name.
                "--label".into(),
                format!("{LABEL_WORKSPACE}={}", self.workspace_key()),
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
            format!("{LABEL_CONFIG_HASH}={hash}"),
            "--workdir".into(),
            workdir.clone(),
        ];
        args.extend(self.resource_labels());
        // `${localWorkspaceFolder}` means THIS environment's checkout — the
        // clone, for a non-primary environment. The whole config is
        // evaluated against `self.env.root`, which is what makes one config
        // serve N environments without a single conditional.
        let local_workspace_folder = self.env.root.display().to_string();
        match &config.workspace_mount {
            Some(mount) => {
                args.push("--mount".into());
                args.push(self.namespaced_mount(
                    &mount.replace("${localWorkspaceFolder}", &local_workspace_folder),
                ));
            }
            None => {
                args.push("-v".into());
                args.push(format!("{local_workspace_folder}:{workdir}:Z"));
            }
        }
        for mount in &config.mounts {
            if let Some(m) = mount.as_str() {
                args.push("--mount".into());
                args.push(self.namespaced_mount(
                    &m.replace("${localWorkspaceFolder}", &local_workspace_folder),
                ));
            }
        }

        args.extend(self.ide_mounts(&config));
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
    ///
    /// The image is now shared with any environment of this workspace whose
    /// config hashes the same, so the removal is best-effort by design:
    /// podman refuses to delete an image another environment's container
    /// still uses, and that refusal is the right answer. Nuking one
    /// environment must not tear the floor out from under another.
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
        if let Some(tag) = self.current_image_tag() {
            if let Err(e) = self.run_captured(vec!["rmi".into(), tag.clone()]).await {
                self.log(format!(
                    "image {tag} kept: {e} (another environment of this \
                     workspace shares it)"
                ));
            }
        }
        *self.running_hash.lock().unwrap() = None;
        self.exec.set_host();
        self.set_state(SupervisorState::Stopped);
        self.set_pending(false);
        Ok(())
    }

    /// Every podman volume this environment owns: the agent home plus the
    /// namespaced form of each volume the config declares.
    pub fn env_volumes(&self) -> Vec<String> {
        let mut volumes = vec![environment::env_home_volume(
            &self.env.workspace_root,
            &self.env.id,
        )];
        if let Ok(Some(config)) = DevcontainerConfig::discover(&self.env.root) {
            volumes.extend(
                config
                    .named_volumes()
                    .iter()
                    .map(|declared| self.namespaced_volume(declared)),
            );
        }
        volumes
    }

    /// Remove one named volume — but only one this ENVIRONMENT owns.
    /// Anything else is refused: the environment view manages this
    /// environment, not podman at large, and certainly not a sibling
    /// environment's cache.
    pub async fn remove_volume(&self, volume: &str) -> Result<()> {
        if !self.env_volumes().iter().any(|v| v == volume) {
            bail!(
                "volume {volume} does not belong to environment {}",
                self.env.id
            );
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

        // By label, not by name: same reason as adoption — the container's
        // own claim about which environment it is outlives our naming.
        if let Ok(out) = self
            .run_captured(vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("label={LABEL_WORKSPACE}={}", self.workspace_key()),
                "--filter".into(),
                format!("label={LABEL_ENV}={}", self.env.id),
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

        if let Some(tag) = self.current_image_tag() {
            if let Ok(out) = self
                .run_captured(vec![
                    "images".into(),
                    "--filter".into(),
                    format!("reference={tag}"),
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
        }

        for volume in self.env_volumes() {
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
        make_env(root, EnvironmentIdentity::primary(root))
    }

    fn make_env(_root: &std::path::Path, env: EnvironmentIdentity) -> Arc<Supervisor> {
        Supervisor::new_outside_container_for_tests(
            env,
            EventBus::new(),
            ExecContext::host_unsandboxed_for_tests(),
        )
    }

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
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
        *sup.running_hash.lock().unwrap() =
            Some(config_hash(&config, &sup.ide_mounts(&config)).unwrap());
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
        // Not this environment's: refused before podman is ever invoked.
        let err = sup.remove_volume("some-other-volume").await.unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
        // Nor the name the config literally declares — that string is not
        // what podman ends up holding.
        let err = sup.remove_volume("my-cache").await.unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
        // The namespaced form is this environment's, and is offered.
        let owned = sup.namespaced_volume("my-cache");
        assert!(
            sup.env_volumes().contains(&owned),
            "{:?}",
            sup.env_volumes()
        );
    }

    /// The new contract, replacing `container_name_is_stable_per_workspace`:
    /// a name is stable per workspace AND environment, and two environments
    /// of one workspace never collide.
    #[test]
    fn container_names_are_stable_per_workspace_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let primary = || make(root).container_name();
        assert_eq!(primary(), primary(), "stable across supervisors");
        assert!(primary().starts_with("taste-"));

        let review = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        assert_ne!(primary(), review.container_name());
        assert!(review.container_name().ends_with("-review"));

        // Same environment slug, different workspace: still distinct.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            review.container_name(),
            make_env(
                other.path(),
                EnvironmentIdentity::cloned(other.path(), env("review"))
            )
            .container_name()
        );
    }

    /// A non-primary environment is rooted at its clone, and every podman
    /// name it derives keys off the WORKSPACE — otherwise two environments
    /// of one workspace would look like two unrelated workspaces and the
    /// fleet could never be enumerated.
    #[test]
    fn a_cloned_environment_roots_at_its_clone_but_keys_off_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let review = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        assert_ne!(review.root(), root, "the clone, not the main checkout");
        assert!(review.root().ends_with("review/repo"));
        assert_eq!(review.workspace_root(), root);
        assert_eq!(
            review.workspace_key(),
            taste_core::environment::workspace_key(root)
        );
    }

    /// Two environments of one workspace must not silently share the
    /// repo's declared caches, but must share its IMAGE.
    #[test]
    fn environments_split_volumes_and_share_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dc = root.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"build": {"dockerfile": "Containerfile"},
                "mounts": ["source=cargo,target=/c,type=volume"]}"#,
        )
        .unwrap();
        std::fs::write(dc.join("Containerfile"), "FROM scratch\n").unwrap();

        let a = make(root);
        let b = make_env(root, EnvironmentIdentity::cloned(root, env("review")));
        let config = DevcontainerConfig::discover(root).unwrap().unwrap();

        assert_eq!(
            a.image_tag(&config).unwrap(),
            b.image_tag(&config).unwrap(),
            "identical config must not mean two copies of one image"
        );
        assert!(a.image_tag(&config).unwrap().starts_with("taste-img-"));

        let (va, vb) = (a.env_volumes(), b.env_volumes());
        assert!(va.iter().all(|v| !vb.contains(v)), "{va:?} vs {vb:?}");
        assert!(va.iter().any(|v| v.ends_with("-cfg-cargo")));
        assert!(va.iter().any(|v| v.ends_with("-home")));
    }

    /// The mount string podman receives carries the namespaced volume, not
    /// the verbatim one the repo wrote — and bind mounts are untouched.
    #[test]
    fn declared_volume_mounts_are_namespaced_at_run_time() {
        let dir = tempfile::tempdir().unwrap();
        let sup = make(dir.path());
        let rewritten = sup.namespaced_mount("source=cargo,target=/c,type=volume");
        assert!(
            rewritten.contains(&sup.namespaced_volume("cargo")),
            "{rewritten}"
        );
        assert!(rewritten.ends_with(",target=/c,type=volume"), "{rewritten}");

        let bind = "type=bind,source=/etc/hosts,target=/etc/hosts";
        assert_eq!(sup.namespaced_mount(bind), bind);
    }

    /// Self-hosting means the IDE's own container IS the environment, and
    /// lifecycle operations belong to a host-side IDE. There is no socket
    /// forwarded in that could make it a sibling — by design.
    #[tokio::test]
    async fn self_hosted_lifecycle_is_refused_not_remoted() {
        let dir = tempfile::tempdir().unwrap();
        let inside = Supervisor::with_inside(
            EnvironmentIdentity::primary(dir.path()),
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
