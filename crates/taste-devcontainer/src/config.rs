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

/// One `portsAttributes` entry: what the config says about a forwarded
/// port. The spec keys these by port number (or a range, or a regex over
/// the process command line — only exact numbers are honoured here).
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortAttributes {
    pub label: Option<String>,
    pub on_auto_forward: Option<String>,
    pub protocol: Option<String>,
    pub require_local_port: Option<bool>,
}

/// A forwarded port with what the config says about it — the row the
/// file tree's Ports section shows, and the subject of a port tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    /// The port inside the container, which is what the config names and
    /// what the row, the tab, and the cache are keyed by.
    pub port: u16,
    /// The localhost port it is published on: `port` itself when that was
    /// free at start, another free one when it was not (two environments
    /// of one project forward the same numbers). The supervisor fills it
    /// in; a spec straight from the config says `port`.
    pub host: u16,
    /// `portsAttributes.<port>.label`, when the config gives one.
    pub label: Option<String>,
    /// `portsAttributes.<port>.protocol`: `http` or `https` per the spec,
    /// when the config says.
    pub protocol: Option<String>,
}

impl PortSpec {
    /// How the port is named on screen: `3000 · App`, or just `3000`.
    pub fn title(&self) -> String {
        match &self.label {
            Some(label) if !label.trim().is_empty() => format!("{} · {}", self.port, label.trim()),
            _ => self.port.to_string(),
        }
    }

    /// Where the port is reachable from the host. Published on loopback
    /// only (see the supervisor's `-p` arguments), so this is the one
    /// address that is ever right.
    pub fn url(&self) -> String {
        let scheme = match self.protocol.as_deref() {
            Some("https") => "https",
            _ => "http",
        };
        format!("{scheme}://127.0.0.1:{}", self.host)
    }

    /// Whether the port had to be published on another number.
    pub fn moved(&self) -> bool {
        self.host != self.port
    }
}

/// `hostRequirements`, as the spec spells it: what the configuration says
/// its container needs. `memory` and `storage` are strings like `"8gb"`.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostRequirements {
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub storage: Option<String>,
}

/// What an environment's container is granted, and what its placement
/// costs a VM: the config's `hostRequirements` when it states them, and
/// a stated default otherwise. The grant is REAL — `podman run` carries
/// it as `--cpus`, `--memory`, and `--memory-swap` — so the plan the pool
/// places by and the ceiling the container runs under are one number
/// (David, 2026-09-21: "Surely we know what we plan to grant each
/// devcontainer env").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub cpus: u32,
    pub memory_mib: u64,
}

impl Grant {
    /// A configuration that says nothing: enough for a build, three to a
    /// 12 GiB VM (David, 2026-09-21: "This is a fine default grant").
    pub const DEFAULT: Grant = Grant {
        cpus: 2,
        memory_mib: 4096,
    };
    /// The IDE's baseline environment: a shell, an agent, and git.
    pub const BASELINE: Grant = Grant {
        cpus: 1,
        memory_mib: 2048,
    };

    /// The grant `hostRequirements` asks for, each field falling back to
    /// the default when absent or unreadable.
    pub fn from_requirements(requirements: Option<&HostRequirements>) -> Grant {
        let Some(requirements) = requirements else {
            return Grant::DEFAULT;
        };
        Grant {
            cpus: requirements
                .cpus
                .filter(|cpus| *cpus > 0)
                .unwrap_or(Grant::DEFAULT.cpus),
            memory_mib: requirements
                .memory
                .as_deref()
                .and_then(parse_memory_mib)
                .filter(|mib| *mib > 0)
                .unwrap_or(Grant::DEFAULT.memory_mib),
        }
    }

    /// Whether this grant fits in `free`.
    pub fn fits(&self, free: Grant) -> bool {
        self.cpus <= free.cpus && self.memory_mib <= free.memory_mib
    }

    /// What is left of `self` after `used`, floored at nothing.
    pub fn minus(&self, used: Grant) -> Grant {
        Grant {
            cpus: self.cpus.saturating_sub(used.cpus),
            memory_mib: self.memory_mib.saturating_sub(used.memory_mib),
        }
    }

    pub fn plus(&self, other: Grant) -> Grant {
        Grant {
            cpus: self.cpus.saturating_add(other.cpus),
            memory_mib: self.memory_mib.saturating_add(other.memory_mib),
        }
    }

    /// `2 CPU, 4.0 GiB`.
    pub fn describe(&self) -> String {
        format!(
            "{} CPU, {:.1} GiB",
            self.cpus,
            self.memory_mib as f64 / 1024.0
        )
    }
}

/// A memory string the way `hostRequirements` writes one — `"8gb"`,
/// `"512mb"`, `"16 GiB"`, `"2048"` (MiB) — in MiB. Case and a space
/// before the unit do not matter; anything else is `None`.
pub fn parse_memory_mib(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.trim().parse().ok()?;
    let factor = match unit.trim() {
        "" | "mb" | "mib" | "m" => 1.0,
        "gb" | "gib" | "g" => 1024.0,
        "tb" | "tib" | "t" => 1024.0 * 1024.0,
        "kb" | "kib" | "k" => 1.0 / 1024.0,
        _ => return None,
    };
    Some((number * factor).round() as u64)
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
    /// `hostRequirements`: see [`Grant`].
    pub host_requirements: Option<HostRequirements>,
    /// The spec's `portsAttributes`: labels and protocols, keyed by the
    /// port as a string.
    #[serde(default)]
    pub ports_attributes: std::collections::BTreeMap<String, PortAttributes>,
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
    /// The checkout the config was discovered under (not part of the
    /// JSON). What the hash's paths are taken relative to, so a config
    /// hashes the same wherever the checkout — or a mirror of its
    /// `.devcontainer/` — happens to sit on disk.
    #[serde(skip)]
    pub root: PathBuf,
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
    /// What this configuration's container is granted ([`Grant`]).
    pub fn grant(&self) -> Grant {
        Grant::from_requirements(self.host_requirements.as_ref())
    }

    /// Find and parse the workspace's devcontainer config, if present.
    pub fn discover(workspace_root: &Path) -> Result<Option<Self>> {
        for path in candidate_paths(workspace_root) {
            if path.is_file() {
                let mut config = Self::load(&path)?;
                config.root = workspace_root.to_path_buf();
                return Ok(Some(config));
            }
        }
        Ok(None)
    }

    /// Load one file. Its root is taken to be the directory above a
    /// `.devcontainer/` directory, or the config's own directory otherwise
    /// — [`Self::discover`] knows better and says so.
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
        config.root = config
            .config_dir
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == ".devcontainer"))
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config.config_dir.clone());
        Ok(config)
    }

    /// [`Self::hash_inputs`], each relative to [`Self::root`] — the spelling
    /// the hash uses, so two checkouts of one config hash alike.
    pub fn hash_inputs_relative(&self) -> Vec<(PathBuf, PathBuf)> {
        self.hash_inputs()
            .into_iter()
            .map(|path| {
                let relative = path
                    .strip_prefix(&self.root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| path.clone());
                (relative, path)
            })
            .collect()
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
    /// The forwarded ports, each with its attributes, ascending and
    /// deduplicated.
    pub fn ports(&self) -> Vec<PortSpec> {
        let mut ports = self.forward_ports.clone();
        ports.sort_unstable();
        ports.dedup();
        ports
            .into_iter()
            .map(|port| {
                let attributes = self.ports_attributes.get(&port.to_string());
                PortSpec {
                    port,
                    host: port,
                    label: attributes.and_then(|a| a.label.clone()),
                    protocol: attributes.and_then(|a| a.protocol.clone()),
                }
            })
            .collect()
    }

    pub fn workspace_folder(&self) -> &str {
        self.workspace_folder.as_deref().unwrap_or("/workspace")
    }

    /// The user commands run as (remoteUser overrides containerUser).
    pub fn effective_user(&self) -> Option<&str> {
        self.remote_user
            .as_deref()
            .or(self.container_user.as_deref())
    }

    /// Named volumes this config mounts, as the config spells them (from
    /// `mounts` and `workspaceMount`).
    ///
    /// These are *declared* names, not the names podman ends up with: an
    /// environment namespaces them (see
    /// [`taste_core::environment::env_config_volume`]) so two environments
    /// of the same workspace do not silently share the repo's caches. Call
    /// sites that talk to podman must namespace first.
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

/// Rewrite a `--mount` spec's named-volume source, leaving everything else
/// — bind mounts, tmpfs, targets, options, ordering — exactly as written.
///
/// The repo declares volumes with a verbatim string; N environments run the
/// same config, so without this every environment of a workspace would
/// mount the *same* podman volume for the repo's `cargo` cache and two
/// agents would build into one directory. Bind mounts are untouched: their
/// source is a host path, and the security validator has already had its
/// say about which paths are allowed.
pub fn rewrite_volume_source(mount: &str, rename: impl Fn(&str) -> String) -> String {
    let parts: Vec<&str> = mount.split(',').collect();
    let is_volume = parts.iter().any(|part| {
        let mut kv = part.splitn(2, '=');
        matches!(
            (kv.next().map(str::trim), kv.next().map(str::trim)),
            (Some("type"), Some("volume"))
        )
    });
    if !is_volume {
        return mount.to_string();
    }
    parts
        .iter()
        .map(|part| {
            let mut kv = part.splitn(2, '=');
            match (kv.next(), kv.next()) {
                (Some(key), Some(value)) if matches!(key.trim(), "source" | "src") => {
                    format!("{key}={}", rename(value.trim()))
                }
                _ => (*part).to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
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
    /// The grant reads `hostRequirements` the way the spec writes it, and
    /// says the default for a config that says nothing.
    #[test]
    fn the_grant_is_the_configs_host_requirements_or_the_default() {
        use super::{parse_memory_mib, Grant, HostRequirements};
        assert_eq!(parse_memory_mib("8gb"), Some(8192));
        assert_eq!(parse_memory_mib("512mb"), Some(512));
        assert_eq!(parse_memory_mib("16 GiB"), Some(16384));
        assert_eq!(parse_memory_mib("2048"), Some(2048));
        assert_eq!(parse_memory_mib("1.5gb"), Some(1536));
        assert_eq!(parse_memory_mib("lots"), None);
        assert_eq!(Grant::from_requirements(None), Grant::DEFAULT);
        assert_eq!(
            Grant::from_requirements(Some(&HostRequirements {
                cpus: Some(8),
                memory: Some("16gb".into()),
                storage: Some("64gb".into()),
            })),
            Grant {
                cpus: 8,
                memory_mib: 16384
            }
        );
        assert_eq!(
            Grant::from_requirements(Some(&HostRequirements {
                cpus: None,
                memory: Some("nonsense".into()),
                storage: None,
            })),
            Grant::DEFAULT,
            "an unreadable field falls back on its own"
        );
        assert_eq!(Grant::DEFAULT.describe(), "2 CPU, 4.0 GiB");
        let free = Grant {
            cpus: 3,
            memory_mib: 5000,
        };
        assert!(Grant::DEFAULT.fits(free));
        assert!(!Grant::DEFAULT.plus(Grant::DEFAULT).fits(free));
        assert_eq!(
            free.minus(Grant::DEFAULT),
            Grant {
                cpus: 1,
                memory_mib: 904
            }
        );
    }

    use super::*;

    #[test]
    fn ports_carry_their_attributes() {
        let config: DevcontainerConfig = serde_json::from_str(
            r#"{
                "image": "img",
                "forwardPorts": [5432, 3000, 3000],
                "portsAttributes": {
                    "3000": { "label": "App", "onAutoForward": "openBrowser" },
                    "9229": { "label": "Not forwarded" }
                }
            }"#,
        )
        .unwrap();
        let ports = config.ports();
        assert_eq!(
            ports.len(),
            2,
            "deduplicated and only the forwarded ones: {ports:?}"
        );
        assert_eq!(ports[0].port, 3000);
        assert_eq!(ports[0].label.as_deref(), Some("App"));
        assert_eq!(ports[0].title(), "3000 · App");
        assert_eq!(ports[0].url(), "http://127.0.0.1:3000");
        assert_eq!(ports[1].port, 5432);
        assert_eq!(ports[1].label, None);
        assert_eq!(ports[1].title(), "5432");
    }

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
