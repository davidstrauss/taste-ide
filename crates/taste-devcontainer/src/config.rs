//! Lenient devcontainer.json discovery and parsing.
//!
//! We parse the subset of the spec we drive directly (image / Containerfile
//! builds, mounts, env, users, lifecycle hooks) and keep unknown fields
//! around for diagnostics. JSONC (comments, trailing commas) is accepted.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildSection {
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    #[serde(default)]
    pub args: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevcontainerConfig {
    pub name: Option<String>,
    pub image: Option<String>,
    pub build: Option<BuildSection>,
    /// Legacy top-level form of `build.dockerfile`.
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub run_args: Vec<String>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub workspace_folder: Option<String>,
    pub workspace_mount: Option<String>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub container_env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub remote_env: std::collections::BTreeMap<String, String>,
    /// Ports published to the host (localhost-only) when the container
    /// starts — the spec's `forwardPorts`.
    #[serde(default)]
    pub forward_ports: Vec<u16>,
    pub on_create_command: Option<serde_json::Value>,
    pub post_create_command: Option<serde_json::Value>,
    pub post_start_command: Option<serde_json::Value>,
    pub override_command: Option<bool>,

    /// Directory the config file lives in (not part of the JSON).
    #[serde(skip)]
    pub config_dir: PathBuf,
    /// The config file itself (not part of the JSON).
    #[serde(skip)]
    pub config_path: PathBuf,
}

/// Spec'd discovery locations, in priority order.
pub fn candidate_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        workspace_root.join(".devcontainer/devcontainer.json"),
        workspace_root.join(".devcontainer.json"),
    ];
    // .devcontainer/<subfolder>/devcontainer.json (one level deep, per spec)
    if let Ok(entries) = std::fs::read_dir(workspace_root.join(".devcontainer")) {
        let mut subs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("devcontainer.json"))
            .filter(|p| p.is_file())
            .collect();
        subs.sort();
        v.extend(subs);
    }
    v
}

impl DevcontainerConfig {
    /// Find and parse the workspace's devcontainer config, if present.
    pub fn discover(workspace_root: &Path) -> Result<Option<Self>> {
        for path in candidate_paths(workspace_root) {
            if path.is_file() {
                return Self::load(&path).map(Some);
            }
        }
        Ok(None)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut stripped = String::new();
        json_comments::StripComments::new(raw.as_bytes())
            .read_to_string(&mut stripped)
            .context("stripping JSONC comments")?;
        // Tolerate trailing commas, which the JSONC dialect allows.
        let stripped = remove_trailing_commas(&stripped);
        let mut config: DevcontainerConfig = serde_json::from_str(&stripped)
            .with_context(|| format!("parsing {}", path.display()))?;
        config.config_path = path.to_path_buf();
        config.config_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(config)
    }

    /// The Containerfile/Dockerfile this config builds from, if any,
    /// resolved relative to the config directory.
    pub fn dockerfile_path(&self) -> Option<PathBuf> {
        let name = self
            .build
            .as_ref()
            .and_then(|b| b.dockerfile.clone())
            .or_else(|| self.dockerfile.clone())?;
        Some(self.config_dir.join(name))
    }

    /// Build context directory. **Always** the config directory — the
    /// config does not get to name it.
    ///
    /// devcontainer configuration is machine-independent: it names no host
    /// path, because a path that means something on one machine means
    /// nothing in Codespaces, in CI, or on a colleague laptop. A `context`
    /// key is therefore not a value to validate but a category error, and
    /// `security.rs` refuses it outright.
    ///
    /// Making it a convention rather than a checked input also removes the
    /// swap-after-check window: there is no path from the config for a
    /// repo to point somewhere else between validation and build. The
    /// context is the single host-filesystem input to a build — `RUN`
    /// cannot reach the host and `COPY` cannot leave the context — so
    /// pinning it pins the whole build surface.
    pub fn build_context(&self) -> PathBuf {
        self.config_dir.clone()
    }

    /// Files whose content defines this configuration, for change hashing.
    pub fn hash_inputs(&self) -> Vec<PathBuf> {
        let mut v = vec![self.config_path.clone()];
        if let Some(df) = self.dockerfile_path() {
            v.push(df);
        }
        v
    }

    /// The in-container workspace folder.
    pub fn workspace_folder(&self) -> &str {
        self.workspace_folder.as_deref().unwrap_or("/workspace")
    }

    /// The user commands run as (remoteUser overrides containerUser).
    pub fn effective_user(&self) -> Option<&str> {
        self.remote_user
            .as_deref()
            .or(self.container_user.as_deref())
    }

    /// Named volumes this config mounts (from `mounts` and
    /// `workspaceMount`). These are the volumes the environment view lists
    /// and the only ones volume-removal will touch.
    pub fn named_volumes(&self) -> Vec<String> {
        let mut volumes = Vec::new();
        let mount_strings = self
            .mounts
            .iter()
            .filter_map(|m| m.as_str())
            .chain(self.workspace_mount.as_deref());
        for mount in mount_strings {
            let mut source: Option<&str> = None;
            let mut is_volume = false;
            for part in mount.split(',') {
                let mut kv = part.splitn(2, '=');
                match (kv.next().map(str::trim), kv.next().map(str::trim)) {
                    (Some("source") | Some("src"), Some(v)) => source = Some(v),
                    (Some("type"), Some("volume")) => is_volume = true,
                    _ => {}
                }
            }
            if is_volume {
                if let Some(source) = source {
                    volumes.push(source.to_string());
                }
            }
        }
        volumes.sort();
        volumes.dedup();
        volumes
    }

    pub fn validate(&self) -> Result<()> {
        if self.image.is_none() && self.dockerfile_path().is_none() {
            bail!(
                "{}: needs either \"image\" or a build dockerfile",
                self.config_path.display()
            );
        }
        Ok(())
    }
}

/// A lifecycle command in the spec is a string (shell), array (exec), or
/// object (named parallel commands). Normalize to a list of shell commands.
pub fn lifecycle_commands(value: &serde_json::Value) -> Vec<Vec<String>> {
    match value {
        serde_json::Value::String(s) => vec![vec!["/bin/sh".into(), "-c".into(), s.clone()]],
        serde_json::Value::Array(items) => {
            let argv: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if argv.is_empty() {
                vec![]
            } else {
                vec![argv]
            }
        }
        serde_json::Value::Object(map) => map.values().flat_map(lifecycle_commands).collect(),
        _ => vec![],
    }
}

fn remove_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape = false;
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
                if matches!(next, Some('}') | Some(']')) {
                    // trailing comma: drop it
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{
                // the container
                "name": "demo",
                "build": { "dockerfile": "Containerfile", },
                "runArgs": ["--userns=keep-id",],
            }"#,
        )
        .unwrap();
        std::fs::write(dc.join("Containerfile"), "FROM scratch\n").unwrap();

        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        assert_eq!(config.name.as_deref(), Some("demo"));
        assert_eq!(config.run_args, vec!["--userns=keep-id"]);
        assert!(config.dockerfile_path().unwrap().ends_with("Containerfile"));
        config.validate().unwrap();
    }

    #[test]
    fn missing_config_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(DevcontainerConfig::discover(dir.path()).unwrap().is_none());
    }

    #[test]
    fn named_volumes_come_from_volume_mounts_only() {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{
                "image": "img",
                "workspaceMount": "source=ws-cache,target=/w,type=volume",
                "mounts": [
                    "source=build-cache,target=/cache,type=volume",
                    "source=${localWorkspaceFolder}/data,target=/data,type=bind",
                    "source=build-cache,target=/cache2,type=volume"
                ]
            }"#,
        )
        .unwrap();
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        assert_eq!(config.named_volumes(), vec!["build-cache", "ws-cache"]);
    }

    #[test]
    fn lifecycle_command_forms() {
        let s = serde_json::json!("make setup");
        assert_eq!(
            lifecycle_commands(&s),
            vec![vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "make setup".to_string()
            ]]
        );
        let arr = serde_json::json!(["cargo", "fetch"]);
        assert_eq!(
            lifecycle_commands(&arr),
            vec![vec!["cargo".to_string(), "fetch".to_string()]]
        );
        let obj = serde_json::json!({"a": "echo 1", "b": ["echo", "2"]});
        assert_eq!(lifecycle_commands(&obj).len(), 2);
    }
}
