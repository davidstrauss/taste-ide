//! A devcontainer defined by a Compose file: what the IDE runs, made from
//! what the project wrote.
//!
//! The project's `dockerComposeFile` is read by Compose itself — the pinned
//! `docker compose` (`crate::agentnode`), whose `config --format json` is
//! the canonical model of the files: every relative path resolved, every
//! `env_file` inlined, every profile not asked for gone. This module never
//! parses YAML, and so never disagrees with Compose about what a file
//! says. It takes that model and makes the one the IDE brings up, piped to
//! `docker compose up -f -`:
//!
//! - **The rules are the single container's, for every service** (David,
//!   2026-11-12: "Same rules, every service"). What a service may ask for
//!   is a list, and anything not on it refuses the config by name rather
//!   than passing through unread: no host network, PID, IPC, or UTS
//!   namespace, no devices or capabilities but nesting's, no sysctls, no
//!   external volumes or networks, and binds only inside the checkout.
//!   `privileged` becomes the nesting set, as `"privileged": true` does
//!   ([`crate::security::NESTING_RUN_ARGS`]).
//! - **A bind of a folder above the checkout is narrowed to the checkout**,
//!   at the path the repository has under that target (David, 2026-11-12:
//!   "Narrow to the checkout"). The Codespaces convention mounts `../..`
//!   at `/workspaces`, so the repository is `/workspaces/<its folder's
//!   name>`; in the VM the folder above a checkout holds the workspace's
//!   other environments, and the container sees none of them.
//! - **The dev service is the environment's container**: it is named as a
//!   single container is (`taste-<workspace>-<env>`), carries its labels,
//!   mounts, user mapping, and limits, so exec, the agent's channel, and
//!   every check that finds an environment's container by name work
//!   unchanged. The other services are named after it
//!   (`taste-<workspace>-<env>-<service>`), labeled as the environment's
//!   services rather than as the environment, and given its limits.
//! - **Published ports are dropped.** Each environment of a VM would
//!   publish the same ones and the second would not start; what reaches
//!   the user's machine is `forwardPorts`, published here on the
//!   service whose network the dev service shares, and forwarded as a
//!   single container's are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The pinned Compose: its release, and each architecture's binary's
/// SHA-256 as the release's `checksums.txt` gives it.
pub const VERSION: &str = "5.6.1";
pub const SHA256_X86_64: &str = "47737f4236e6d3ccea7291995203f8f018058783b4c3a9469119811a0b5db8c0";
pub const SHA256_AARCH64: &str = "42cfa75f51ad0ccf290bad37899e0038a8c10a0069e85233ecc1d53e3ff1347d";

/// The label that marks a Compose service as one of an environment's,
/// beside its container: the environment's id. Not `taste.env`, which
/// says a container IS the environment, and is what finds it.
pub const LABEL_SERVICE_OF: &str = "taste.env-service";

/// The label Compose puts on everything it makes for a project — its
/// containers, networks, and volumes — naming the project, which is an
/// environment's own (`Supervisor::owned_volumes`).
pub const LABEL_PROJECT: &str = "com.docker.compose.project";

/// `word` as one word for a POSIX shell: as it is when it can only be
/// one, single-quoted otherwise.
pub fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '=' | ':' | ',')
        })
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// A service key the IDE takes as it is.
const SERVICE_KEYS: &[&str] = &[
    "annotations",
    "attach",
    "build",
    "cap_drop",
    "command",
    "container_name",
    "depends_on",
    "develop",
    "dns",
    "dns_opt",
    "dns_search",
    "domainname",
    "entrypoint",
    "environment",
    "expose",
    "extra_hosts",
    "group_add",
    "healthcheck",
    "hostname",
    "image",
    "init",
    "labels",
    "links",
    "mac_address",
    "network_mode",
    "networks",
    "platform",
    "post_start",
    "pre_stop",
    "pull_policy",
    "read_only",
    "restart",
    "scale",
    "secrets",
    "configs",
    "shm_size",
    "stdin_open",
    "stop_grace_period",
    "stop_signal",
    "tmpfs",
    "tty",
    "ulimits",
    "user",
    "volumes",
    "volumes_from",
    "working_dir",
    // Said, and then replaced by the IDE's own.
    "cap_add",
    "cpu_shares",
    "cpus",
    "deploy",
    "devices",
    "ipc",
    "mem_limit",
    "mem_reservation",
    "memswap_limit",
    "oom_score_adj",
    "ports",
    "privileged",
    "security_opt",
];

/// A build key the IDE takes as it is.
const BUILD_KEYS: &[&str] = &[
    "args",
    "cache_from",
    "context",
    "dockerfile",
    "dockerfile_inline",
    "labels",
    "no_cache",
    "pull",
    "shm_size",
    "target",
];

/// What the IDE knows that the Compose files do not.
pub struct Context<'a> {
    /// The Compose project: one per environment, so each one's services,
    /// network, and volumes are its own.
    pub project: &'a str,
    /// The config's `service`: the container the IDE works in.
    pub service: &'a str,
    /// The config's `runServices`, when it names some: those and the dev
    /// service are brought up; otherwise every service is.
    pub run_services: &'a [String],
    /// The checkout, where the model's paths are.
    pub checkout: &'a Path,
    /// The folder the user opened, whose trailing names a narrowed bind
    /// puts the repository under.
    pub folder: &'a Path,
    /// The dev service's container name, which is the environment's.
    pub container_name: &'a str,
    /// What a built service's image is called, before `-<service>`.
    pub image_prefix: &'a str,
    /// The dev service's labels, which are the environment's.
    pub dev_labels: BTreeMap<String, String>,
    /// Every other service's labels.
    pub service_labels: BTreeMap<String, String>,
    /// The IDE's own mounts and labels for the dev service, as `podman
    /// run` takes them (`-v`, `--label`).
    pub ide_mounts: &'a [String],
    /// The dev service's user mapping, as `--userns` takes it.
    pub userns: Option<String>,
    /// `forwardPorts`, each with the port it is published on in the VM.
    pub ports: &'a [Forward],
    /// The environment's memory, in MiB, and CPUs: every service's limit.
    pub memory_mib: u64,
    pub cpus: u32,
    /// `--cpu-shares`, and the dev service's reservation or OOM score
    /// (`priority_args`).
    pub cpu_shares: u32,
    pub memory_reservation_mib: Option<u64>,
    pub oom_score_adj: Option<i32>,
    /// The config's `overrideCommand`: the dev service kept up by `sleep
    /// infinity` rather than its own command. Off unless stated, as the
    /// spec has it for Compose.
    pub override_command: bool,
    /// A volume's name for this environment, from the name the files give
    /// it.
    pub volume_name: &'a dyn Fn(&str) -> String,
    /// Whether a path under the checkout may be bound, read, or built
    /// from: no symlink on the way out of it (`security::bind_stays_inside`).
    pub path_check: &'a dyn Fn(&Path) -> Result<(), String>,
}

/// One of `forwardPorts`: the service it names (the dev service's own
/// when none), its port there, and the port it is published on in the VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub service: Option<String>,
    pub target: u16,
    pub published: u16,
}

/// What to bring up.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The model, for `docker compose -f - up`.
    pub model: Value,
    /// The services to start; empty for all of them.
    pub services: Vec<String>,
    /// What was set aside, for the build log.
    pub notes: Vec<String>,
}

/// The model the IDE brings up, from Compose's own reading of the files,
/// or why it will not be.
pub fn plan(model: &Value, cx: &Context) -> Result<Plan, String> {
    let mut notes = Vec::new();
    let top = model
        .as_object()
        .ok_or("Compose's model is not an object")?;
    let mut out = Map::new();
    out.insert("name".into(), json!(cx.project));
    for (key, value) in top {
        match key.as_str() {
            "name" | "services" => {}
            "networks" => {
                out.insert(key.clone(), networks(value)?);
            }
            "volumes" => {
                out.insert(key.clone(), volumes(value, cx)?);
            }
            "secrets" | "configs" => {
                out.insert(key.clone(), file_sources(key, value, cx)?);
            }
            other if other.starts_with("x-") => {}
            other => {
                return Err(format!(
                    "the Compose files' \"{other}\" is not supported here"
                ))
            }
        }
    }
    let services = top
        .get("services")
        .and_then(Value::as_object)
        .ok_or("the Compose files define no services")?;
    if !services.contains_key(cx.service) {
        return Err(format!(
            "devcontainer.json's \"service\" is \"{}\", and the Compose files define {}",
            cx.service,
            services.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for name in cx.run_services {
        if !services.contains_key(name) {
            return Err(format!(
                "devcontainer.json's \"runServices\" names \"{name}\", which the Compose files \
                 do not define"
            ));
        }
    }
    let mut rewritten = Map::new();
    for (name, service) in services {
        let dev = name == cx.service;
        rewritten.insert(
            name.clone(),
            self::service(name, service, dev, cx, &mut notes)?,
        );
    }
    // forwardPorts, each on whichever service owns the network of the
    // service it names — the dev service's when it names none.
    let mut published: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for forward in cx.ports {
        let named = forward.service.as_deref().unwrap_or(cx.service);
        if !rewritten.contains_key(named) {
            return Err(format!(
                "forwardPorts names {named}:{}, and the Compose files define no service \
                 \"{named}\"",
                forward.target
            ));
        }
        let owner = network_owner(&rewritten, named)?;
        published.entry(owner).or_default().push(json!({
            "target": forward.target,
            "published": forward.published.to_string(),
            "host_ip": "127.0.0.1",
            "protocol": "tcp",
        }));
    }
    for (owner, ports) in published {
        if let Some(service) = rewritten.get_mut(&owner).and_then(Value::as_object_mut) {
            service.insert("ports".into(), Value::Array(ports));
        }
    }
    out.insert("services".into(), Value::Object(rewritten));
    let services = if cx.run_services.is_empty() {
        Vec::new()
    } else {
        let mut chosen: Vec<String> = cx.run_services.to_vec();
        if !chosen.iter().any(|s| s == cx.service) {
            chosen.push(cx.service.to_string());
        }
        chosen
    };
    Ok(Plan {
        model: Value::Object(out),
        services,
        notes,
    })
}

fn service(
    name: &str,
    service: &Value,
    dev: bool,
    cx: &Context,
    notes: &mut Vec<String>,
) -> Result<Value, String> {
    let at = |key: &str| format!("service \"{name}\"'s \"{key}\"");
    let given = service
        .as_object()
        .ok_or_else(|| format!("service \"{name}\" is not an object"))?;
    for key in given.keys() {
        if !SERVICE_KEYS.contains(&key.as_str()) && !key.starts_with("x-") {
            return Err(format!("{} is not supported here", at(key)));
        }
    }
    let mut out: Map<String, Value> = given
        .iter()
        .filter(|(key, _)| !key.starts_with("x-"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Namespaces: a service's own, or another service's; never the VM's.
    if let Some(mode) = given.get("network_mode").and_then(Value::as_str) {
        let ok = mode == "none" || mode == "bridge" || mode.starts_with("service:");
        if !ok {
            return Err(format!(
                "{} is \"{mode}\": a service may share another service's network or have \
                 none, never the VM's",
                at("network_mode")
            ));
        }
    }
    if let Some(ipc) = given.get("ipc").and_then(Value::as_str) {
        if !(ipc == "private" || ipc == "shareable" || ipc == "none" || ipc.starts_with("service:"))
        {
            return Err(format!("{} is \"{ipc}\"", at("ipc")));
        }
    }
    if let Some(from) = given.get("volumes_from").and_then(Value::as_array) {
        for entry in from {
            let entry = entry.as_str().unwrap_or_default();
            if entry.starts_with("container:") {
                return Err(format!(
                    "{} names \"{entry}\": only the project's own services",
                    at("volumes_from")
                ));
            }
        }
    }

    // Privilege: the nesting set, or what of it a service states.
    let privileged = given.get("privileged").and_then(Value::as_bool) == Some(true);
    out.remove("privileged");
    let mut caps = granted(
        given.get("cap_add"),
        crate::security::ALLOWED_CAPS,
        &at("cap_add"),
    )?;
    let mut devices = granted(
        given.get("devices"),
        crate::security::ALLOWED_DEVICES,
        &at("devices"),
    )?;
    let mut opts = granted(
        given.get("security_opt"),
        crate::security::ALLOWED_SECURITY_OPTS,
        &at("security_opt"),
    )?;
    if privileged {
        for flag in crate::security::NESTING_RUN_ARGS {
            let (key, value) = flag.split_once('=').unwrap_or((flag, ""));
            let list = match key {
                "--security-opt" => &mut opts,
                "--device" => &mut devices,
                "--cap-add" => &mut caps,
                _ => continue,
            };
            if !list.iter().any(|v| v == value) {
                list.push(value.to_string());
            }
        }
    }
    for (key, list) in [
        ("cap_add", caps),
        ("devices", devices),
        ("security_opt", opts),
    ] {
        if list.is_empty() {
            out.remove(key);
        } else {
            out.insert(key.into(), json!(list));
        }
    }

    // Published ports: dropped (see the module's header).
    if let Some(ports) = out.remove("ports") {
        if ports.as_array().is_some_and(|p| !p.is_empty()) {
            notes.push(format!(
                "service \"{name}\": its published ports are not published; forwardPorts in \
                 devcontainer.json reaches the user's machine"
            ));
        }
    }

    if let Some(build) = given.get("build") {
        out.insert("build".into(), self::build(name, build, cx)?);
        out.insert("image".into(), json!(format!("{}-{name}", cx.image_prefix)));
    }
    if let Some(volumes) = given.get("volumes").and_then(Value::as_array) {
        let mut kept = Vec::new();
        for volume in volumes {
            kept.push(self::mount(name, volume, cx)?);
        }
        out.insert("volumes".into(), Value::Array(kept));
    }

    // Limits: the environment's, for every service.
    for key in [
        "deploy",
        "mem_limit",
        "memswap_limit",
        "mem_reservation",
        "cpus",
        "cpu_shares",
        "oom_score_adj",
    ] {
        out.remove(key);
    }
    out.insert("mem_limit".into(), json!(format!("{}m", cx.memory_mib)));
    out.insert("memswap_limit".into(), json!(format!("{}m", cx.memory_mib)));
    out.insert("cpus".into(), json!(cx.cpus));
    out.insert("cpu_shares".into(), json!(cx.cpu_shares));

    let mut labels: BTreeMap<String, Value> = given
        .get("labels")
        .and_then(Value::as_object)
        .map(|l| l.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    labels.retain(|key, _| !key.starts_with("taste."));
    if dev {
        out.insert("container_name".into(), json!(cx.container_name));
        for (key, value) in &cx.dev_labels {
            labels.insert(key.clone(), json!(value));
        }
        let (mounts, ide_labels) = ide_mounts(cx.ide_mounts)?;
        for (key, value) in ide_labels {
            labels.insert(key, json!(value));
        }
        let volumes = out
            .entry("volumes")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(volumes) = volumes.as_array_mut() {
            // The IDE's binds win a target the files also bind.
            volumes.retain(|v| {
                let target = v.get("target").and_then(Value::as_str);
                !mounts
                    .iter()
                    .any(|m| m.get("target").and_then(Value::as_str) == target)
            });
            volumes.extend(mounts);
        }
        if let Some(userns) = &cx.userns {
            out.insert("userns_mode".into(), json!(userns));
        }
        if let Some(reservation) = cx.memory_reservation_mib {
            out.insert("mem_reservation".into(), json!(format!("{reservation}m")));
        }
        if let Some(score) = cx.oom_score_adj {
            out.insert("oom_score_adj".into(), json!(score));
        }
        if cx.override_command {
            out.insert("command".into(), json!(["sleep", "infinity"]));
        }
    } else {
        out.insert(
            "container_name".into(),
            json!(format!("{}-{name}", cx.container_name)),
        );
        for (key, value) in &cx.service_labels {
            labels.insert(key.clone(), json!(value));
        }
        out.insert("oom_score_adj".into(), json!(500));
    }
    out.insert("labels".into(), Value::Object(labels.into_iter().collect()));
    Ok(Value::Object(out))
}

/// What a service states of a granted list (`cap_add`, `devices`,
/// `security_opt`), when every entry is on `allowed`.
fn granted(value: Option<&Value>, allowed: &[&str], at: &str) -> Result<Vec<String>, String> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries {
        // A device is "path" or "path:path[:perms]", given or as an object.
        let text = match entry {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        let key = text.split(':').next().unwrap_or_default();
        if !allowed.contains(&text.as_str()) && !allowed.contains(&key) {
            return Err(format!(
                "{at} asks for \"{text}\": only {} may be stated",
                allowed.join(", ")
            ));
        }
        out.push(text);
    }
    Ok(out)
}

fn build(name: &str, build: &Value, cx: &Context) -> Result<Value, String> {
    let at = |key: &str| format!("service \"{name}\"'s build \"{key}\"");
    let given = build
        .as_object()
        .ok_or_else(|| format!("service \"{name}\"'s \"build\" is not an object"))?;
    for key in given.keys() {
        if !BUILD_KEYS.contains(&key.as_str()) && !key.starts_with("x-") {
            return Err(format!("{} is not supported here", at(key)));
        }
    }
    let context = given
        .get("context")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| format!("service \"{name}\"'s build has no context"))?;
    inside(&context, cx).map_err(|e| format!("{}: {e}", at("context")))?;
    if let Some(dockerfile) = given.get("dockerfile").and_then(Value::as_str) {
        let path = context.join(dockerfile);
        inside(&path, cx).map_err(|e| format!("{}: {e}", at("dockerfile")))?;
    }
    Ok(Value::Object(
        given
            .iter()
            .filter(|(k, _)| !k.starts_with("x-"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ))
}

/// One of a service's volumes, as the IDE binds it.
fn mount(name: &str, volume: &Value, cx: &Context) -> Result<Value, String> {
    let at = format!("service \"{name}\"'s volume");
    let given = volume
        .as_object()
        .ok_or_else(|| format!("{at} is not an object"))?;
    let kind = given
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("volume");
    let target = given
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "volume" | "tmpfs" => Ok(volume.clone()),
        "bind" => {
            let source = given
                .get("source")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{at} at {target} binds no source"))?;
            let mut out = given.clone();
            if source != cx.checkout && cx.checkout.starts_with(&source) {
                // Above the checkout: narrowed to it, at the path the
                // repository has under the target.
                let depth = cx
                    .checkout
                    .strip_prefix(&source)
                    .map(|rest| rest.components().count())
                    .unwrap_or(0);
                let names: Vec<_> = cx.folder.components().collect();
                let tail: PathBuf = names[names.len().saturating_sub(depth)..].iter().collect();
                out.insert("source".into(), json!(cx.checkout.display().to_string()));
                out.insert(
                    "target".into(),
                    json!(Path::new(target).join(tail).display().to_string()),
                );
            } else {
                inside(&source, cx).map_err(|e| format!("{at} at {target}: {e}"))?;
            }
            // Shared, so the IDE's own binds of the checkout and this one
            // label it alike.
            let mut bind = given
                .get("bind")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            bind.insert("selinux".into(), json!("z"));
            out.insert("bind".into(), Value::Object(bind));
            Ok(Value::Object(out))
        }
        other => Err(format!(
            "{at} at {target} is a \"{other}\" mount; volumes, tmpfs, and binds inside the \
             checkout are"
        )),
    }
}

/// Whether `path` is the checkout or under it, by its names and then by
/// what is on disk.
fn inside(path: &Path, cx: &Context) -> Result<(), String> {
    let lexical = path.is_absolute()
        && path.starts_with(cx.checkout)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if !lexical {
        return Err(format!(
            "{} is outside the checkout (the repo is untrusted)",
            path.display()
        ));
    }
    (cx.path_check)(path)
}

/// The project's networks: its own, bridged, and named by Compose after
/// the project, whatever name the files give them — a name stated there
/// is the same in every environment, and two environments of a VM would
/// share one network.
fn networks(value: &Value) -> Result<Value, String> {
    let Some(networks) = value.as_object() else {
        return Ok(value.clone());
    };
    let mut out = Map::new();
    for (name, network) in networks {
        let at = format!("network \"{name}\"");
        if network.get("external").and_then(Value::as_bool) == Some(true) {
            return Err(format!("{at} is external: only the project's own networks"));
        }
        if let Some(driver) = network.get("driver").and_then(Value::as_str) {
            if driver != "bridge" {
                return Err(format!(
                    "{at}'s driver is \"{driver}\": only bridge networks, which reach \
                     nothing past the VM"
                ));
            }
        }
        if network.get("driver_opts").is_some() {
            return Err(format!("{at}'s driver_opts are not supported here"));
        }
        let mut network = network.as_object().cloned().unwrap_or_default();
        network.remove("name");
        out.insert(name.clone(), Value::Object(network));
    }
    Ok(Value::Object(out))
}

/// The project's volumes, each named for this environment.
fn volumes(value: &Value, cx: &Context) -> Result<Value, String> {
    let Some(volumes) = value.as_object() else {
        return Ok(value.clone());
    };
    let mut out = Map::new();
    for (name, volume) in volumes {
        let at = format!("volume \"{name}\"");
        if volume.get("external").and_then(Value::as_bool) == Some(true) {
            return Err(format!("{at} is external: only the project's own volumes"));
        }
        if volume.get("driver_opts").is_some() {
            // The local driver's options bind a path of the VM.
            return Err(format!("{at}'s driver_opts are not supported here"));
        }
        if let Some(driver) = volume.get("driver").and_then(Value::as_str) {
            if driver != "local" {
                return Err(format!("{at}'s driver is \"{driver}\""));
            }
        }
        let mut volume = volume.as_object().cloned().unwrap_or_default();
        volume.insert("name".into(), json!((cx.volume_name)(name)));
        out.insert(name.clone(), Value::Object(volume));
    }
    Ok(Value::Object(out))
}

/// The project's secrets or configs: files inside the checkout.
fn file_sources(kind: &str, value: &Value, cx: &Context) -> Result<Value, String> {
    let Some(entries) = value.as_object() else {
        return Ok(value.clone());
    };
    for (name, entry) in entries {
        let at = format!("{kind} \"{name}\"");
        match entry.get("file").and_then(Value::as_str) {
            Some(file) => inside(Path::new(file), cx).map_err(|e| format!("{at}: {e}"))?,
            None if entry.get("content").is_some() => {}
            None => {
                return Err(format!(
                    "{at} is not a file inside the checkout, which is the one source here"
                ))
            }
        }
    }
    Ok(value.clone())
}

/// The service that owns `service`'s network: itself, or the one its
/// `network_mode: service:` names, followed.
fn network_owner(services: &Map<String, Value>, service: &str) -> Result<String, String> {
    let mut current = service.to_string();
    for _ in 0..services.len() + 1 {
        let next = services
            .get(&current)
            .and_then(|s| s.get("network_mode"))
            .and_then(Value::as_str)
            .and_then(|m| m.strip_prefix("service:"))
            .map(str::to_string);
        match next {
            Some(next) => current = next,
            None => return Ok(current),
        }
    }
    Err(format!(
        "service \"{service}\"'s network_mode goes round in a circle"
    ))
}

/// The IDE's mounts and labels for the dev service, from the `podman
/// run` arguments the single container is given (`Supervisor::ide_mounts`):
/// `-v source:target[:options]` and `--label key=value`.
fn ide_mounts(args: &[String]) -> Result<(Vec<Value>, BTreeMap<String, String>), String> {
    let mut mounts = Vec::new();
    let mut labels = BTreeMap::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("the IDE's own {flag} has no value"))?;
        match flag.as_str() {
            "--label" => {
                let (key, value) = value.split_once('=').unwrap_or((value, ""));
                labels.insert(key.to_string(), value.to_string());
            }
            "-v" => {
                let mut parts = value.splitn(3, ':');
                let source = parts.next().unwrap_or_default();
                let target = parts.next().unwrap_or_default();
                let options: Vec<&str> = parts
                    .next()
                    .map(|o| o.split(',').collect())
                    .unwrap_or_default();
                let read_only = options.contains(&"ro");
                if source.starts_with('/') {
                    let selinux = if options.contains(&"Z") { "Z" } else { "z" };
                    mounts.push(json!({
                        "type": "bind",
                        "source": source,
                        "target": target,
                        "read_only": read_only,
                        "bind": { "selinux": selinux },
                    }));
                } else {
                    mounts.push(json!({
                        "type": "volume",
                        "source": source,
                        "target": target,
                        "read_only": read_only,
                    }));
                }
            }
            other => return Err(format!("the IDE's own \"{other}\" has no Compose form")),
        }
    }
    Ok((mounts, labels))
}

/// The named volumes the IDE's own mounts use, which the model must
/// declare as external: they are the environment's, made before the
/// project's (the agent's home, so its history outlives a rebuild).
pub fn ide_volumes(args: &[String]) -> Vec<String> {
    ide_mounts(args)
        .map(|(mounts, _)| {
            mounts
                .iter()
                .filter(|m| m.get("type").and_then(Value::as_str) == Some("volume"))
                .filter_map(|m| m.get("source").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Declare the IDE's own volumes in `plan`'s model as external, under
/// their own names: the model refers to them by name, and Compose would
/// otherwise make a project volume of each.
pub fn declare_ide_volumes(plan: &mut Plan, names: &[String]) {
    let Some(top) = plan.model.as_object_mut() else {
        return;
    };
    let volumes = top
        .entry("volumes")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(volumes) = volumes.as_object_mut() {
        for name in names {
            volumes.insert(name.clone(), json!({ "name": name, "external": true }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// good_job's own Compose files, as the pinned Compose's `config
    /// --format json` read them in a checkout at /vm/primary.
    fn good_job() -> Value {
        json!({
          "name": "x",
          "networks": { "default": { "name": "x_default", "ipam": {} } },
          "services": {
            "app": {
              "command": ["sleep", "infinity"],
              "entrypoint": null,
              "network_mode": "service:db",
              "volumes": [
                { "type": "bind", "source": "/vm", "target": "/workspaces", "bind": {} }
              ],
              "build": { "context": "/vm/primary", "dockerfile": ".devcontainer/Dockerfile" },
              "depends_on": { "db": { "condition": "service_started", "restart": true, "required": true } }
            },
            "db": {
              "restart": "unless-stopped",
              "command": null,
              "entrypoint": null,
              "environment": { "POSTGRES_PASSWORD": "postgres" },
              "image": "postgres:latest",
              "networks": { "default": null },
              "ports": [{ "mode": "ingress", "target": 5432, "published": "5432", "protocol": "tcp" }],
              "volumes": [
                { "type": "volume", "source": "postgres-data", "target": "/var/lib/postgresql/data", "volume": {} },
                { "type": "bind", "source": "/vm/primary/.devcontainer/create-db-user.sql",
                  "target": "/docker-entrypoint-initdb.d/create-db-user.sql", "bind": {} }
              ]
            }
          },
          "volumes": { "postgres-data": { "name": "x_postgres-data" } }
        })
    }

    fn with<T>(edit: impl FnOnce(&mut Context) -> T) -> T {
        with_using(&[], &|_| Ok(()), edit)
    }

    /// The forwarded ports [`with`] gives a plan.
    fn default_ports() -> Vec<Forward> {
        vec![
            Forward {
                service: None,
                target: 3000,
                published: 3000,
            },
            Forward {
                service: None,
                target: 5432,
                published: 15432,
            },
        ]
    }

    /// [`with`], the services to start and the check on disk given.
    fn with_using<T>(
        run_services: &[String],
        path_check: &dyn Fn(&Path) -> Result<(), String>,
        edit: impl FnOnce(&mut Context) -> T,
    ) -> T {
        with_ports(run_services, &default_ports(), path_check, edit)
    }

    /// [`with_using`], the forwarded ports given too.
    fn with_ports<T>(
        run_services: &[String],
        ports: &[Forward],
        path_check: &dyn Fn(&Path) -> Result<(), String>,
        edit: impl FnOnce(&mut Context) -> T,
    ) -> T {
        let ide = vec![
            "-v".to_string(),
            "/vm/primary:/vm/primary:z".to_string(),
            "-v".to_string(),
            "taste-env-w-primary-home:/home/agent".to_string(),
            "--label".to_string(),
            "taste.userns-generation=1".to_string(),
        ];
        let volume_name = |name: &str| format!("taste-env-w-primary-cfg-{name}");
        let mut cx = Context {
            project: "taste-w-primary",
            service: "app",
            run_services,
            checkout: Path::new("/vm/primary"),
            folder: Path::new("/home/u/Projects/good_job"),
            container_name: "taste-w-primary",
            image_prefix: "taste-img-abc",
            dev_labels: BTreeMap::from([("taste.env".into(), "primary".into())]),
            service_labels: BTreeMap::from([(LABEL_SERVICE_OF.into(), "primary".into())]),
            ide_mounts: &ide,
            userns: Some("keep-id:uid=1000,gid=1000".into()),
            ports,
            memory_mib: 4096,
            cpus: 4,
            cpu_shares: 1024,
            memory_reservation_mib: Some(1024),
            oom_score_adj: None,
            override_command: false,
            volume_name: &volume_name,
            path_check,
        };
        edit(&mut cx)
    }

    fn planned(model: &Value) -> Result<Plan, String> {
        with(|cx| plan(model, cx))
    }

    /// The Codespaces shape runs: the parent bind narrowed to the checkout
    /// under the repository's own name, the dev service named and labeled
    /// as the environment's container with the IDE's mounts, the database
    /// as one of its services, its published port dropped, and the
    /// forwarded ports on the database, whose network the app shares.
    #[test]
    fn a_codespaces_compose_runs_as_the_environment() {
        let plan = planned(&good_job()).unwrap();
        let m = &plan.model;
        assert_eq!(m["name"], "taste-w-primary");
        let app = &m["services"]["app"];
        assert_eq!(app["container_name"], "taste-w-primary");
        assert_eq!(app["labels"]["taste.env"], "primary");
        assert_eq!(app["labels"]["taste.userns-generation"], "1");
        assert_eq!(app["userns_mode"], "keep-id:uid=1000,gid=1000");
        assert_eq!(app["image"], "taste-img-abc-app");
        assert_eq!(app["mem_limit"], "4096m");
        let targets: Vec<(&str, &str)> = app["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["source"].as_str().unwrap(), v["target"].as_str().unwrap()))
            .collect();
        assert!(
            targets.contains(&("/vm/primary", "/workspaces/good_job")),
            "{targets:?}"
        );
        assert!(
            targets.contains(&("/vm/primary", "/vm/primary")),
            "{targets:?}"
        );
        assert!(targets.contains(&("taste-env-w-primary-home", "/home/agent")));
        assert!(
            !targets.iter().any(|(s, _)| *s == "/vm"),
            "nothing above the checkout"
        );

        let db = &m["services"]["db"];
        assert_eq!(db["container_name"], "taste-w-primary-db");
        assert_eq!(db["labels"]["taste.env-service"], "primary");
        assert!(
            db["labels"].get("taste.env").is_none(),
            "not the environment"
        );
        let ports: Vec<(&str, u64)> = db["ports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["published"].as_str().unwrap(),
                    p["target"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(ports, vec![("3000", 3000), ("15432", 5432)]);
        assert!(db["ports"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["host_ip"] == "127.0.0.1"));
        assert!(
            plan.notes.iter().any(|n| n.contains("\"db\"")),
            "{:?}",
            plan.notes
        );
        assert_eq!(
            m["volumes"]["postgres-data"]["name"],
            "taste-env-w-primary-cfg-postgres-data"
        );
        assert!(app.get("ports").is_none());
        assert_eq!(plan.services, Vec::<String>::new());
        assert!(
            m["networks"]["default"].get("name").is_none(),
            "named by Compose after the project"
        );
    }

    /// A port forwarded from a service by name (`"cache:6379"`) is
    /// published on that service, or on the one whose network it shares;
    /// a name the files do not define refuses the config.
    #[test]
    fn a_port_named_by_service_is_published_on_it() {
        let mut model = good_job();
        model["services"]["cache"] = json!({ "image": "redis:7" });
        let forwards = [
            Forward {
                service: Some("cache".into()),
                target: 6379,
                published: 16379,
            },
            Forward {
                service: None,
                target: 3000,
                published: 3000,
            },
        ];
        let planned = with_ports(&[], &forwards, &|_| Ok(()), |cx| plan(&model, cx)).unwrap();
        let services = &planned.model["services"];
        assert_eq!(services["cache"]["ports"][0]["target"], 6379);
        assert_eq!(services["cache"]["ports"][0]["published"], "16379");
        // The app shares the database's network, so its port is there.
        assert_eq!(services["db"]["ports"][0]["target"], 3000);
        let missing = [Forward {
            service: Some("search".into()),
            target: 9200,
            published: 9200,
        }];
        let refused = with_ports(&[], &missing, &|_| Ok(()), |cx| plan(&model, cx)).unwrap_err();
        assert!(refused.contains("\"search\""), "{refused}");
    }

    /// Every way out of the VM's own sandbox refuses the config by name.
    #[test]
    fn what_reaches_past_the_project_is_refused() {
        type Edit = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "network_mode",
                Box::new(|m| m["services"]["db"]["network_mode"] = json!("host")),
            ),
            (
                "pid",
                Box::new(|m| m["services"]["db"]["pid"] = json!("host")),
            ),
            (
                "sysctls",
                Box::new(|m| m["services"]["db"]["sysctls"] = json!({"a": "1"})),
            ),
            (
                "cap_add",
                Box::new(|m| m["services"]["db"]["cap_add"] = json!(["NET_ADMIN"])),
            ),
            (
                "devices",
                Box::new(|m| m["services"]["db"]["devices"] = json!(["/dev/kvm"])),
            ),
            (
                "security_opt",
                Box::new(|m| m["services"]["db"]["security_opt"] = json!(["seccomp=unconfined"])),
            ),
            (
                "outside the checkout",
                Box::new(|m| {
                    m["services"]["db"]["volumes"][1]["source"] = json!("/vm/other/secret")
                }),
            ),
            (
                "outside the checkout",
                Box::new(|m| m["services"]["app"]["build"]["context"] = json!("/vm")),
            ),
            (
                "external",
                Box::new(|m| m["volumes"]["postgres-data"]["external"] = json!(true)),
            ),
            (
                "driver_opts",
                Box::new(|m| {
                    m["volumes"]["postgres-data"]["driver_opts"] =
                        json!({"type": "none", "o": "bind", "device": "/"})
                }),
            ),
            (
                "driver",
                Box::new(|m| m["networks"]["default"]["driver"] = json!("macvlan")),
            ),
            (
                "container:",
                Box::new(|m| m["services"]["db"]["volumes_from"] = json!(["container:x"])),
            ),
            (
                "ipc",
                Box::new(|m| m["services"]["db"]["ipc"] = json!("host")),
            ),
            (
                "additional_contexts",
                Box::new(|m| {
                    m["services"]["app"]["build"]["additional_contexts"] = json!({"x": "/"})
                }),
            ),
        ];
        for (named, edit) in cases {
            let mut model = good_job();
            edit(&mut model);
            let refused = planned(&model).unwrap_err();
            assert!(refused.contains(named), "{named}: {refused}");
        }
    }

    /// `privileged` becomes the nesting set, as a single container's does,
    /// and what a service states of that set stays.
    #[test]
    fn privilege_is_the_nesting_set() {
        let mut model = good_job();
        model["services"]["app"]["privileged"] = json!(true);
        model["services"]["db"]["devices"] = json!(["/dev/fuse"]);
        let plan = planned(&model).unwrap();
        let app = &plan.model["services"]["app"];
        assert!(app.get("privileged").is_none());
        assert_eq!(app["cap_add"], json!(["SYS_ADMIN"]));
        assert_eq!(app["devices"], json!(["/dev/fuse", "/dev/net/tun"]));
        assert_eq!(
            app["security_opt"],
            json!(["label=type:container_engine_t", "unmask=ALL"])
        );
        assert_eq!(
            plan.model["services"]["db"]["devices"],
            json!(["/dev/fuse"])
        );
    }

    /// A bind the symlink check refuses refuses the config; one under the
    /// checkout is bound, shared-labeled.
    #[test]
    fn a_bind_inside_the_checkout_is_checked_on_disk() {
        let model = good_job();
        let check = |p: &Path| {
            if p.ends_with("create-db-user.sql") {
                Err(format!("{} is a symlink", p.display()))
            } else {
                Ok(())
            }
        };
        let refused = with_using(&[], &check, |cx| plan(&model, cx)).unwrap_err();
        assert!(refused.contains("symlink"), "{refused}");
        let planned = planned(&model).unwrap();
        assert_eq!(
            planned.model["services"]["db"]["volumes"][1]["bind"]["selinux"],
            "z"
        );
    }

    /// `runServices` starts those and the dev service; a service the files
    /// do not define, as either, refuses the config.
    #[test]
    fn the_services_started_are_the_ones_named() {
        let run = vec!["db".to_string()];
        let started = with_using(&run, &|_| Ok(()), |cx| plan(&good_job(), cx)).unwrap();
        assert_eq!(started.services, vec!["db".to_string(), "app".to_string()]);
        let missing = with(|cx| {
            cx.service = "web";
            plan(&good_job(), cx)
        })
        .unwrap_err();
        assert!(missing.contains("\"web\""), "{missing}");
    }

    /// The command the files give the dev service runs, unless
    /// overrideCommand says to keep it up with sleep.
    #[test]
    fn the_dev_service_keeps_its_own_command_unless_told() {
        let mut model = good_job();
        model["services"]["app"]["command"] = json!(["bin/server"]);
        let own = planned(&model).unwrap();
        assert_eq!(
            own.model["services"]["app"]["command"],
            json!(["bin/server"])
        );
        let kept = with(|cx| {
            cx.override_command = true;
            plan(&model, cx)
        })
        .unwrap();
        assert_eq!(
            kept.model["services"]["app"]["command"],
            json!(["sleep", "infinity"])
        );
    }

    /// The plan for a real model, printed, for bringing it up by hand
    /// against a podman: `TASTE_COMPOSE_MODEL` (Compose's `config --format
    /// json`), `TASTE_COMPOSE_CHECKOUT`, and `TASTE_COMPOSE_FOLDER`.
    #[test]
    #[ignore]
    fn print_a_plan() {
        let model: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("TASTE_COMPOSE_MODEL").unwrap()).unwrap(),
        )
        .unwrap();
        let checkout = PathBuf::from(std::env::var("TASTE_COMPOSE_CHECKOUT").unwrap());
        let folder = PathBuf::from(std::env::var("TASTE_COMPOSE_FOLDER").unwrap());
        let ide = vec![
            "-v".to_string(),
            format!("{0}:{0}:z", checkout.display()),
            "-v".to_string(),
            "taste-env-test-home:/home/agent".to_string(),
            "--label".to_string(),
            "taste.userns-generation=1".to_string(),
        ];
        let volume_name = |name: &str| format!("taste-env-test-primary-cfg-{name}");
        let path_check = |_: &Path| Ok(());
        let mut planned = plan(
            &model,
            &Context {
                project: "taste-test-primary",
                service: "app",
                run_services: &[],
                checkout: &checkout,
                folder: &folder,
                container_name: "taste-test-primary",
                image_prefix: "taste-img-test",
                dev_labels: BTreeMap::from([("taste.env".into(), "primary".into())]),
                service_labels: BTreeMap::from([(LABEL_SERVICE_OF.into(), "primary".into())]),
                ide_mounts: &ide,
                userns: Some("keep-id:uid=1000,gid=1000".into()),
                ports: &[Forward {
                    service: None,
                    target: 8080,
                    published: 18080,
                }],
                memory_mib: 1024,
                cpus: 2,
                cpu_shares: 1024,
                memory_reservation_mib: Some(512),
                oom_score_adj: None,
                override_command: false,
                volume_name: &volume_name,
                path_check: &path_check,
            },
        )
        .unwrap();
        declare_ide_volumes(&mut planned, &ide_volumes(&ide));
        println!("PLAN {}", serde_json::to_string(&planned.model).unwrap());
    }

    /// The IDE's own volumes are declared as external under their names.
    #[test]
    fn the_ides_own_volumes_are_external() {
        let mut plan = planned(&good_job()).unwrap();
        let names = vec!["taste-env-w-primary-home".to_string()];
        declare_ide_volumes(&mut plan, &names);
        assert_eq!(
            plan.model["volumes"]["taste-env-w-primary-home"],
            json!({ "name": "taste-env-w-primary-home", "external": true })
        );
        assert_eq!(
            ide_volumes(&["-v".into(), "vol:/x".into(), "-v".into(), "/a:/b:z".into()]),
            vec!["vol".to_string()]
        );
    }
}
