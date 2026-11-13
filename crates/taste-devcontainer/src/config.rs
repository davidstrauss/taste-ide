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

/// A config file's text as JSON: JSONC's comments and trailing commas
/// dropped.
pub fn jsonc_value(raw: &str) -> Result<serde_json::Value> {
    let mut stripped = String::new();
    json_comments::StripComments::new(raw.as_bytes())
        .read_to_string(&mut stripped)
        .context("stripping JSONC comments")?;
    Ok(serde_json::from_str(&remove_trailing_commas(&stripped))?)
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
    /// The Compose service whose port it is, when `forwardPorts` names one
    /// (`"db:5432"`); `None` for the dev service's own.
    pub service: Option<String>,
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

    /// Where the port is reachable from the host: loopback only, and
    /// named `localhost` rather than `127.0.0.1`, because `localhost` is
    /// the origin dev servers trust by default and the address is the
    /// page's origin. Next.js in development refuses its hot-reload
    /// socket to any other origin (`allowedDevOrigins`), and a page that
    /// never gets the socket never hydrates: the port tab showed the
    /// server's HTML with every menu dead, where the user's own browser,
    /// on `localhost`, worked (2026-10-22). The forward listens on both
    /// loopbacks (`Keys::ssh_tunnel_argv`), and a port is only chosen when
    /// both are free (`Supervisor::publish_ports`), so whichever one
    /// `localhost` resolves to first is this port's.
    pub fn url(&self) -> String {
        let scheme = match self.protocol.as_deref() {
            Some("https") => "https",
            _ => "http",
        };
        format!("{scheme}://localhost:{}", self.host)
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
/// a stated default otherwise. The memory is REAL — `podman run` carries
/// it as `--memory` and `--memory-swap` — so the plan the pool places by
/// and the ceiling the container runs under are one number (David,
/// 2026-09-21: "Surely we know what we plan to grant each devcontainer
/// env"). The CPUs are not a ceiling and not a reservation: a container
/// may use every vCPU its VM has, and on a contended VM the weights
/// decide who goes first (`supervisor::priority_args`), because CPU
/// pressure makes a build slower where memory pressure takes the host
/// down (David, 2026-10-17: "Unlike memory pressure, CPU pressure isn't
/// catastrophic"). `cpus` is kept as what the config said.
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

    /// The CPUs its container sees: one for each GiB of its memory, and
    /// never fewer than two. Uncapped, a container saw its VM's fourteen
    /// vCPUs, cargo ran fourteen `rustc` at once in four GiB, and the
    /// container spent nine tenths of its time stalled on memory — the
    /// agent's startup checks with it, so the agent never came up
    /// (2026-10-17). A build sizes its jobs by `nproc`, which reads this, so
    /// the jobs it starts are ones its memory can hold; a project that wants
    /// more of both asks for more memory in `hostRequirements`.
    pub fn cpu_ceiling(&self) -> u32 {
        u32::try_from(self.memory_mib / 1024)
            .unwrap_or(u32::MAX)
            .max(2)
    }

    /// Whether this grant fits in `free`: its memory, which is the one
    /// commitment. CPUs are shared, so they never make a grant not fit.
    pub fn fits(&self, free: Grant) -> bool {
        self.memory_mib <= free.memory_mib
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

    /// `4.0 GiB`: the memory, the part of a grant that is held.
    pub fn describe(&self) -> String {
        format!("{:.1} GiB", self.memory_mib as f64 / 1024.0)
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
    /// The spec's `privileged`: what it becomes here is
    /// `security::privileged_run_args`, never `--privileged` itself.
    pub privileged: Option<bool>,
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
    /// starts — the spec's `forwardPorts`: a number, or `"host:port"`,
    /// where a host other than `localhost` is a Compose service.
    #[serde(default, deserialize_with = "forward_ports")]
    pub forward_ports: Vec<ForwardPort>,
    /// `hostRequirements`: see [`Grant`].
    pub host_requirements: Option<HostRequirements>,
    /// The spec's `portsAttributes`: labels and protocols, keyed by the
    /// port as a string.
    #[serde(default)]
    pub ports_attributes: std::collections::BTreeMap<String, PortAttributes>,
    /// The spec's `dockerComposeFile`: one Compose file or a list,
    /// relative to the config's directory (`crate::compose`).
    pub docker_compose_file: Option<OneOrMany>,
    /// The Compose service the IDE works in, which a Compose config names.
    pub service: Option<String>,
    /// The Compose services to start with it; all of them when empty.
    #[serde(default)]
    pub run_services: Vec<String>,
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

/// One entry of `forwardPorts`: a port of the dev container, or of the
/// Compose service it names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForwardPort {
    pub service: Option<String>,
    pub port: u16,
}

impl ForwardPort {
    /// An entry as the spec writes it: `3000`, `"3000"`, `"localhost:3000"`,
    /// or `"db:5432"`.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        if let Some(port) = value.as_u64() {
            return u16::try_from(port).ok().map(|port| ForwardPort {
                service: None,
                port,
            });
        }
        let text = value.as_str()?.trim();
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => (Some(host.trim()), port.trim()),
            None => (None, text),
        };
        let port: u16 = port.parse().ok()?;
        let service = match host {
            None | Some("localhost") | Some("127.0.0.1") | Some("") => None,
            Some(host) => Some(host.to_string()),
        };
        Some(ForwardPort { service, port })
    }

    /// As the spec writes it, for a message.
    pub fn spelled(&self) -> String {
        match &self.service {
            Some(service) => format!("{service}:{}", self.port),
            None => self.port.to_string(),
        }
    }
}

fn forward_ports<'de, D>(deserializer: D) -> std::result::Result<Vec<ForwardPort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values: Vec<serde_json::Value> = Deserialize::deserialize(deserializer)?;
    values
        .iter()
        .map(|value| {
            ForwardPort::parse(value).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "forwardPorts: {value} is not a port, \"host:port\", or \"service:port\""
                ))
            })
        })
        .collect()
}

/// A spec value that is one string or a list of them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn items(&self) -> Vec<String> {
        match self {
            OneOrMany::One(one) => vec![one.clone()],
            OneOrMany::Many(many) => many.clone(),
        }
    }
}

/// `path` with its `.` and `..` taken out by name, never through the disk:
/// `None` when a `..` would climb above where it starts.
pub fn lexical_normal(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
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

    /// Whether this config defines its environment with Compose.
    pub fn is_compose(&self) -> bool {
        self.docker_compose_file.is_some()
    }

    /// The Compose files, in the order given, resolved against the config's
    /// directory by name: a list Compose merges in that order.
    pub fn compose_files(&self) -> Vec<PathBuf> {
        self.docker_compose_file
            .as_ref()
            .map(OneOrMany::items)
            .unwrap_or_default()
            .iter()
            .map(|file| {
                let joined = self.config_dir.join(file);
                lexical_normal(&joined).unwrap_or(joined)
            })
            .collect()
    }

    /// Files whose content defines this configuration, for change hashing.
    /// A Compose config's are its Compose files and every file under its
    /// own directory — the Dockerfiles and init scripts its services name
    /// usually sit beside it, and which ones they are is Compose's to say.
    pub fn hash_inputs(&self) -> Vec<PathBuf> {
        let mut v = vec![self.config_path.clone()];
        if self.is_compose() {
            v.extend(self.compose_files());
            let mut beside = Vec::new();
            files_under(&self.config_dir, &mut beside, 0);
            beside.retain(|path| !v.contains(path));
            beside.sort();
            v.extend(beside);
            return v;
        }
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
        ports.sort_by_key(|p| (p.port, p.service.clone()));
        ports.dedup_by_key(|p| p.port);
        ports
            .into_iter()
            .map(|forward| {
                let attributes = self
                    .ports_attributes
                    .get(&forward.spelled())
                    .or_else(|| self.ports_attributes.get(&forward.port.to_string()));
                PortSpec {
                    port: forward.port,
                    host: forward.port,
                    service: forward.service,
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

    /// Where in the container each named volume of `mounts` is mounted,
    /// as the config spells it — what a new volume's ownership is settled
    /// at (`Supervisor::settle_new_volumes`). `workspaceMount` is not
    /// among them: it is the checkout's, and its ownership is the
    /// checkout's.
    pub fn named_volume_targets(&self) -> Vec<String> {
        let mut targets = Vec::new();
        for mount in self.mounts.iter().filter_map(|m| m.as_str()) {
            let mut target: Option<&str> = None;
            let mut is_volume = false;
            for part in mount.split(',') {
                let mut kv = part.splitn(2, '=');
                match (kv.next().map(str::trim), kv.next().map(str::trim)) {
                    (Some("target" | "dst" | "destination"), Some(v)) => target = Some(v),
                    (Some("type"), Some("volume")) => is_volume = true,
                    _ => {}
                }
            }
            if let (true, Some(target)) = (is_volume, target) {
                targets.push(target.to_string());
            }
        }
        targets.sort();
        targets.dedup();
        targets
    }

    pub fn validate(&self) -> Result<()> {
        if self.is_compose() {
            if self.service.as_deref().is_none_or(|s| s.trim().is_empty()) {
                bail!(
                    "{}: a Compose config names the service to work in (\"service\")",
                    self.config_path.display()
                );
            }
            let files = self.compose_files();
            if files.is_empty() {
                bail!(
                    "{}: \"dockerComposeFile\" names no file",
                    self.config_path.display()
                );
            }
            for file in &files {
                if !file.starts_with(&self.root) {
                    bail!(
                        "{}: the Compose file {} is outside the checkout",
                        self.config_path.display(),
                        file.display()
                    );
                }
            }
            // A port is one row, one tab, and one forward on this machine,
            // keyed by its number: the same number from two services would
            // be two of them in one place.
            for (i, a) in self.forward_ports.iter().enumerate() {
                if let Some(b) = self.forward_ports[i + 1..]
                    .iter()
                    .find(|b| b.port == a.port && b.service != a.service)
                {
                    bail!(
                        "{}: forwardPorts names port {} twice ({} and {}); forward one, or \
                         have one service listen on another port",
                        self.config_path.display(),
                        a.port,
                        a.spelled(),
                        b.spelled()
                    );
                }
            }
            return Ok(());
        }
        if let Some(named) = self.forward_ports.iter().find(|p| p.service.is_some()) {
            bail!(
                "{}: forwardPorts names {}, and a service is a Compose config's; this config \
                 is a single container, whose ports are named by number",
                self.config_path.display(),
                named.spelled()
            );
        }
        if self.image.is_none() && self.dockerfile_path().is_none() {
            bail!(
                "{}: needs either \"image\" or a build dockerfile",
                self.config_path.display()
            );
        }
        Ok(())
    }
}

/// Every file under `dir`, a few levels down, for the hash.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_file() => out.push(path),
            Ok(kind) if kind.is_dir() => files_under(&path, out, depth + 1),
            _ => {}
        }
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
        assert_eq!(Grant::DEFAULT.describe(), "4.0 GiB");
        // A job per GiB: the default's four, the baseline's floor of two.
        assert_eq!(Grant::DEFAULT.cpu_ceiling(), 4);
        assert_eq!(Grant::BASELINE.cpu_ceiling(), 2);
        assert_eq!(
            Grant {
                cpus: 2,
                memory_mib: 12 * 1024
            }
            .cpu_ceiling(),
            12
        );
        let free = Grant {
            cpus: 3,
            memory_mib: 5000,
        };
        assert!(Grant::DEFAULT.fits(free));
        assert!(!Grant::DEFAULT.plus(Grant::DEFAULT).fits(free));
        // CPUs are shared: a VM with none left still takes a grant whose
        // memory fits.
        assert!(Grant::DEFAULT.fits(Grant {
            cpus: 0,
            memory_mib: 5000
        }));
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
        assert_eq!(ports[0].url(), "http://localhost:3000");
        assert_eq!(ports[1].port, 5432);
        assert_eq!(ports[1].label, None);
        assert_eq!(ports[1].title(), "5432");
    }

    fn config_of(json: &str) -> (tempfile::TempDir, DevcontainerConfig) {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), json).unwrap();
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        (dir, config)
    }

    /// `forwardPorts` takes the spec's every spelling: a number, a
    /// numeral, `localhost:` one, and a Compose service's `name:port`,
    /// which carries its attributes by either key.
    #[test]
    fn forward_ports_are_read_in_every_spelling() {
        let (_dir, config) = config_of(
            r#"{
                "dockerComposeFile": "compose.yaml", "service": "app",
                "forwardPorts": [3000, "4000", "localhost:5000", "db:5432"],
                "portsAttributes": { "db:5432": { "label": "Database" } }
            }"#,
        );
        let specs = config.ports();
        let summary: Vec<(u16, Option<String>, Option<String>)> = specs
            .into_iter()
            .map(|p| (p.port, p.service, p.label))
            .collect();
        assert_eq!(
            summary,
            vec![
                (3000, None, None),
                (4000, None, None),
                (5000, None, None),
                (5432, Some("db".into()), Some("Database".into())),
            ]
        );
        config.validate().unwrap();
    }

    /// A service's port in a single container's config, or one port number
    /// from two services, is refused by name; a word that is no port makes
    /// the config unreadable, saying which.
    #[test]
    fn a_named_port_needs_compose_and_a_port_is_forwarded_once() {
        let (_dir, single) = config_of(r#"{ "image": "img", "forwardPorts": ["db:5432"] }"#);
        let refused = single.validate().unwrap_err().to_string();
        assert!(refused.contains("db:5432"), "{refused}");
        let (_dir, twice) = config_of(
            r#"{ "dockerComposeFile": "c.yaml", "service": "app",
                 "forwardPorts": ["db:5432", "replica:5432"] }"#,
        );
        let refused = twice.validate().unwrap_err().to_string();
        assert!(refused.contains("port 5432 twice"), "{refused}");
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{ "image": "img", "forwardPorts": ["web"] }"#,
        )
        .unwrap();
        let unread = format!(
            "{:#}",
            DevcontainerConfig::discover(dir.path()).unwrap_err()
        );
        assert!(unread.contains("\"web\""), "{unread}");
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
        // The checkout's own mount is not one whose ownership is settled.
        assert_eq!(config.named_volume_targets(), vec!["/cache", "/cache2"]);
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
