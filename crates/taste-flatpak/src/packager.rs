//! The build → install → launch pipeline.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use taste_core::event::FlatpakStateEvent;
use taste_core::{Event, EventBus};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{manifest, FlatpakManifest};

const LOG_RING_CAPACITY: usize = 2000;
const BUILDER_APP: &str = "org.flatpak.Builder";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackagerState {
    /// No pipeline has run yet (or the last one's outcome was cleared).
    Idle,
    Building,
    Launching,
    Succeeded,
    Failed {
        message: String,
    },
}

impl PackagerState {
    fn to_event(&self) -> Option<FlatpakStateEvent> {
        match self {
            PackagerState::Idle => None,
            PackagerState::Building => Some(FlatpakStateEvent::Building),
            PackagerState::Launching => Some(FlatpakStateEvent::Launching),
            PackagerState::Succeeded => Some(FlatpakStateEvent::Succeeded),
            PackagerState::Failed { message } => Some(FlatpakStateEvent::Failed {
                message: message.clone(),
            }),
        }
    }
}

pub struct Packager {
    root: PathBuf,
    events: EventBus,
    manifest: Mutex<Option<FlatpakManifest>>,
    state: Mutex<PackagerState>,
    logs: Mutex<VecDeque<String>>,
    /// Inside the Flatpak-packaged IDE, flatpak itself lives on the host.
    sandboxed: bool,
}

impl Packager {
    pub fn new(root: PathBuf, events: EventBus) -> Arc<Self> {
        let manifest = manifest::discover(&root);
        Arc::new(Self {
            root,
            events,
            manifest: Mutex::new(manifest),
            state: Mutex::new(PackagerState::Idle),
            logs: Mutex::new(VecDeque::new()),
            sandboxed: std::path::Path::new("/.flatpak-info").exists(),
        })
    }

    pub fn manifest(&self) -> Option<FlatpakManifest> {
        self.manifest.lock().unwrap().clone()
    }

    /// Re-scan for a manifest (e.g. after the file tree creates one).
    pub fn rediscover(&self) -> Option<FlatpakManifest> {
        let found = manifest::discover(&self.root);
        *self.manifest.lock().unwrap() = found.clone();
        found
    }

    pub fn state(&self) -> PackagerState {
        self.state.lock().unwrap().clone()
    }

    pub fn logs_tail(&self, n: usize) -> Vec<String> {
        let logs = self.logs.lock().unwrap();
        logs.iter().rev().take(n).rev().cloned().collect()
    }

    fn set_state(&self, state: PackagerState) {
        *self.state.lock().unwrap() = state.clone();
        if let Some(event) = state.to_event() {
            self.events.publish(Event::FlatpakState(event));
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
        self.events.publish(Event::FlatpakLog(line));
    }

    /// flatpak always runs on the host, even when the IDE is sandboxed.
    fn flatpak(&self, args: &[String]) -> tokio::process::Command {
        if self.sandboxed {
            let mut cmd = tokio::process::Command::new("flatpak-spawn");
            cmd.arg("--host").arg("flatpak").args(args);
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("flatpak");
            cmd.args(args);
            cmd
        }
    }

    async fn run_logged(&self, args: Vec<String>) -> Result<()> {
        self.log(format!("$ flatpak {}", args.join(" ")));
        let mut child = self
            .flatpak(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawning flatpak")?;
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
                "flatpak {} failed: {status}",
                args.first().cloned().unwrap_or_default()
            );
        }
        Ok(())
    }

    async fn run_captured(&self, args: Vec<String>) -> Result<String> {
        let output = self
            .flatpak(&args)
            .output()
            .await
            .context("running flatpak")?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("flatpak {}: {err}", args.join(" "));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Preflight problems get friendly, actionable errors *before* a long
    /// build fails obscurely.
    async fn preflight(&self, manifest: &FlatpakManifest) -> Result<()> {
        if self
            .run_captured(vec!["info".into(), BUILDER_APP.into()])
            .await
            .is_err()
        {
            bail!(
                "{BUILDER_APP} is not installed. Install it once with:\n  \
                 flatpak install flathub {BUILDER_APP}"
            );
        }
        for path in manifest.referenced_cargo_sources() {
            if !path.exists() {
                bail!(
                    "{} is referenced by the manifest but missing. \
                     Generate it (see README → Flatpak) before building.",
                    path.display()
                );
            }
        }
        Ok(())
    }

    /// The pipeline: preflight → build+install (user installation) →
    /// optionally launch the installed app, detached.
    ///
    /// User-triggered only, by design: agents can read state and logs over
    /// MCP but cannot invoke this (installing to the host is the "user
    /// deploys" trust boundary).
    pub async fn build_install_launch(&self, launch: bool) -> Result<()> {
        let manifest = match self.manifest() {
            Some(m) => m,
            None => {
                // Publish a state: the deploy button re-arms on Failed.
                let message = "no Flatpak manifest found in this workspace".to_string();
                self.log(message.clone());
                self.set_state(PackagerState::Failed {
                    message: message.clone(),
                });
                bail!(message);
            }
        };

        self.set_state(PackagerState::Building);
        if let Err(e) = self.preflight(&manifest).await {
            self.log(format!("preflight: {e:#}"));
            self.set_state(PackagerState::Failed {
                message: e.to_string(),
            });
            return Err(e);
        }

        let manifest_dir = manifest.dir().display().to_string();
        let build_args: Vec<String> = vec![
            "run".into(),
            BUILDER_APP.into(),
            "--force-clean".into(),
            "--user".into(),
            "--install".into(),
            "--install-deps-from=flathub".into(),
            format!("--state-dir={manifest_dir}/.flatpak-builder"),
            format!("{manifest_dir}/build"),
            manifest.path.display().to_string(),
        ];
        if let Err(e) = self.run_logged(build_args).await {
            self.set_state(PackagerState::Failed {
                message: e.to_string(),
            });
            return Err(e);
        }

        if launch {
            self.set_state(PackagerState::Launching);
            // Detached: the app is its own program, not a build step.
            let spawned = self
                .flatpak(&["run".into(), manifest.app_id.clone()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(_child) => self.log(format!("launched {}", manifest.app_id)),
                Err(e) => {
                    let message = format!("launch failed: {e}");
                    self.log(message.clone());
                    self.set_state(PackagerState::Failed { message });
                    bail!("launching {}: {e}", manifest.app_id);
                }
            }
        }

        self.set_state(PackagerState::Succeeded);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_manifest() -> (tempfile::TempDir, Arc<Packager>) {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("build-aux/flatpak");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::write(
            fp.join("org.example.App.json"),
            r#"{"app-id": "org.example.App", "modules": [
                {"name": "app", "sources": ["cargo-sources.json"]}
            ]}"#,
        )
        .unwrap();
        let packager = Packager::new(dir.path().to_path_buf(), EventBus::new());
        (dir, packager)
    }

    #[test]
    fn discovers_on_construction_and_rediscovers() {
        let dir = tempfile::tempdir().unwrap();
        let packager = Packager::new(dir.path().to_path_buf(), EventBus::new());
        assert!(packager.manifest().is_none());

        std::fs::write(
            dir.path().join("org.example.App.json"),
            r#"{"app-id": "org.example.App"}"#,
        )
        .unwrap();
        assert!(packager.rediscover().is_some());
        assert!(packager.manifest().is_some());
    }

    #[tokio::test]
    async fn build_without_manifest_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let packager = Packager::new(dir.path().to_path_buf(), EventBus::new());
        let err = packager.build_install_launch(false).await.unwrap_err();
        assert!(err.to_string().contains("no Flatpak manifest"));
        // A published Failed state (not silent Idle) re-arms the UI button.
        assert!(matches!(packager.state(), PackagerState::Failed { .. }));
    }

    #[tokio::test]
    async fn missing_cargo_sources_gives_actionable_error() {
        let (_dir, packager) = workspace_with_manifest();
        // Preflight fails either on the builder check (no flatpak in the
        // test container) or on cargo-sources; both are Failed states with
        // a message pointing at the fix.
        let err = packager.build_install_launch(false).await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("flatpak install flathub") || message.contains("cargo-sources"),
            "unhelpful error: {message}"
        );
        assert!(matches!(packager.state(), PackagerState::Failed { .. }));
    }

    #[test]
    fn state_events_reach_the_bus() {
        let (_dir, packager) = workspace_with_manifest();
        let rx = packager.events.subscribe();
        packager.set_state(PackagerState::Building);
        match rx.try_recv().unwrap() {
            Event::FlatpakState(FlatpakStateEvent::Building) => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
