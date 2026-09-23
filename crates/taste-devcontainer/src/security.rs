//! Security validation of devcontainer configs.
//!
//! The project repo is untrusted: a cloned repository's devcontainer.json
//! could otherwise mount the home directory, disable isolation, or grant
//! itself devices. The supervisor refuses to build/start a container whose
//! config asks for anything outside this allowlist — the error lands in the
//! banner, the log, and MCP, where safe mode exists precisely so the config
//! can be fixed.
//!
//! Rootless podman already denies real root; this validator's job is to
//! keep repo-controlled flags from reaching the user's data (`-v /home/...`)
//! or weakening the container boundary (`--privileged`, `--security-opt`).
//!
//! One weakening is granted on purpose: **nesting** — a container engine
//! inside the container, for a project whose own build or tests run podman
//! (see [`NESTING_RUN_ARGS`]).

use std::path::Path;

use anyhow::{bail, Result};

use crate::DevcontainerConfig;

/// runArgs entries allowed as exact strings or `prefix=`-style flags.
const ALLOWED_FLAG_PREFIXES: &[&str] = &[
    "--userns=keep-id",
    // systemd-as-PID1 service management inside the container; hardens
    // nothing away from the host (still rootless podman).
    "--systemd=always",
    "--systemd=true",
    "--env=",
    "--shm-size=",
    "--memory=",
    "--cpus=",
    "--hostname=",
    "--init",
    "--label=",
];

/// runArgs flags that consume the *next* entry as their value.
const ALLOWED_FLAGS_WITH_VALUE: &[&str] = &["-e", "--env", "--shm-size", "--hostname", "--label"];

/// What a container needs to run podman inside itself — `podman build`
/// and `podman run` both, rootless, as the image's user — and nothing more.
///
/// Measured, not assumed (2026-09-23, Fedora CoreOS 44 guest, podman 5.8,
/// `quay.io/podman/stable` run with the IDE's own `--userns=keep-id`):
/// with the default flags a nested BUILD works and a nested RUN fails,
/// `mount devpts to dev/pts: Permission denied`, which is SELinux's
/// `container_t`. `label=type:container_engine_t` is container-selinux's
/// domain for exactly this — still confined, still MCS-separated — and it
/// gets as far as the inner container masking `/proc/acpi`, which the
/// outer container's own masked `/proc` forbids; `unmask=ALL` is that last
/// step. Then storage: where the image's container storage is a volume,
/// native overlay works, but on the container's own overlay root — the
/// ordinary case, and the one an agent's image met — podman falls back to
/// fuse-overlayfs, which fails with no `/dev/fuse` ("cannot mount: No such
/// file or directory") and works with it. The first measurement missed
/// this because `quay.io/podman/stable` declares its storage as volumes.
/// Then networking: a nested container's network is pasta's, which needs
/// `/dev/net/tun` — missed by a probe that ran with `--network=none`, and
/// found by an agent. Then the hostname: a nested container sets its own,
/// and the default seccomp profile allows `sethostname` only to a process
/// holding `CAP_SYS_ADMIN` in the outer container — "crun: sethostname:
/// Operation not permitted" without it, SELinux not involved (it failed
/// with `label=disable` too). `--cap-add=SYS_ADMIN` is the grant (David,
/// 2026-09-23, over a profile of the IDE's own that allowed the one call,
/// and over every project setting `--uts=host`): a capability in the
/// container's own user namespace, which also lets seccomp pass the rest
/// of that capability's calls — reach into the VM's kernel, not the
/// host's. Measured: with it, a nested `podman run` with no flags names
/// its host and reaches the network. No `--privileged`. `label=disable`
/// works for SELinux's part too, and is accepted for configs that already
/// say it, but is not what the IDE asks for.
///
/// Why this is not the boundary giving way: that boundary is the HOST
/// (ENVIRONMENTS → "Isolation: the standard, and what meets it"), and the
/// container's substrate is a VM of the workspace's pool. What nesting
/// widens is the container's reach into its own VM, whose kernel is not
/// the user's.
pub const NESTING_RUN_ARGS: &[&str] = &[
    "--security-opt=label=type:container_engine_t",
    "--security-opt=unmask=ALL",
    "--device=/dev/fuse",
    "--device=/dev/net/tun",
    "--cap-add=SYS_ADMIN",
];

/// The `--security-opt` values a config may state: the nesting set, and
/// the two other spellings podman-in-podman guides give it.
const ALLOWED_SECURITY_OPTS: &[&str] = &[
    "label=type:container_engine_t",
    "unmask=ALL",
    "label=disable",
    "label=nested",
];

/// The `--device` values a config may state, both nesting's own:
/// `/dev/fuse` for fuse-overlayfs (storage on an overlay root), and
/// `/dev/net/tun` for pasta, which gives a nested container its network
/// ("Failed to open() /dev/net/tun" without it). The guest has both, and
/// neither reaches anything outside the VM.
const ALLOWED_DEVICES: &[&str] = &["/dev/fuse", "/dev/net/tun"];

/// The `--cap-add` values a config may state: nesting's one, for
/// `sethostname` in a nested container (see [`NESTING_RUN_ARGS`]).
const ALLOWED_CAPS: &[&str] = &["SYS_ADMIN", "CAP_SYS_ADMIN"];

/// Flags accepted for cross-ecosystem compatibility but never passed to
/// podman as they are. Docker needs `--privileged` for systemd-in-container
/// and for docker-in-docker; rootless podman needs it for neither
/// (`--systemd` handles the first, [`NESTING_RUN_ARGS`] the second), so a
/// devcontainer.json shared with VS Code / Codespaces keeps working here —
/// and the flag becomes the nesting set, which is the one thing a config
/// asking for it could still want (`privileged_run_args`).
pub const STRIPPED_FLAGS: &[&str] = &["--privileged"];

/// Whether the config asks for a container engine inside the container:
/// privileged, or stating nesting's SELinux domain itself. What decides
/// that the started container is probed for it.
pub fn asks_for_nesting(config: &DevcontainerConfig) -> bool {
    !privileged_run_args(config).is_empty()
        || config.run_args.iter().any(|arg| {
            arg.contains("label=type:container_engine_t") || arg.contains("label=disable")
        })
}

/// What a config's own request for privilege becomes: the nesting set,
/// once, when `runArgs` carries `--privileged` or the spec's top-level
/// `"privileged": true` is set, and nothing otherwise. The supervisor
/// appends it where the stripped flag would have gone.
pub fn privileged_run_args(config: &DevcontainerConfig) -> Vec<String> {
    let asked = config.privileged == Some(true)
        || config
            .run_args
            .iter()
            .any(|arg| STRIPPED_FLAGS.contains(&arg.as_str()));
    if !asked {
        return Vec::new();
    }
    let stated = |flag: &str| {
        let (name, value) = flag.split_once('=').unwrap_or((flag, ""));
        config.run_args.iter().enumerate().any(|(i, arg)| {
            arg == flag
                || (arg == name && config.run_args.get(i + 1).map(String::as_str) == Some(value))
        })
    };
    NESTING_RUN_ARGS
        .iter()
        .filter(|flag| !stated(flag))
        .map(|flag| flag.to_string())
        .collect()
}

/// The host directories the config's bind mounts read from, inside the
/// workspace: `${localWorkspaceFolder}` expanded, volumes and everything
/// outside the workspace left out. What `Supervisor::reload` creates
/// before `podman run`, because podman does not — it refuses a missing
/// source with "statfs …: no such file or directory", and a project that
/// binds its vendor folder has none until its first install (2026-09-16).
pub fn bind_sources(config: &DevcontainerConfig, workspace_root: &Path) -> Vec<std::path::PathBuf> {
    let mut sources = Vec::new();
    let mounts = config
        .workspace_mount
        .iter()
        .map(String::as_str)
        .chain(config.mounts.iter().filter_map(|m| m.as_str()));
    for mount in mounts {
        let mut source: Option<&str> = None;
        let mut mount_type: Option<&str> = None;
        for part in mount.split(',') {
            let mut kv = part.splitn(2, '=');
            match (kv.next().map(str::trim), kv.next().map(str::trim)) {
                (Some("source") | Some("src"), Some(v)) => source = Some(v),
                (Some("type"), Some(v)) => mount_type = Some(v),
                _ => {}
            }
        }
        if mount_type != Some("bind") {
            continue;
        }
        let Some(source) = source else { continue };
        let expanded = source.replace(
            "${localWorkspaceFolder}",
            &workspace_root.display().to_string(),
        );
        let path = std::path::PathBuf::from(expanded);
        if path.is_absolute() && path.starts_with(workspace_root) && path != workspace_root {
            sources.push(path);
        }
    }
    sources
}

pub fn validate_security(config: &DevcontainerConfig, workspace_root: &Path) -> Result<()> {
    validate_security_via(&taste_core::files::Files::Local, config, workspace_root)
}

/// [`validate_security`] for a checkout reached through `files` — the
/// VM's service when the checkout is over there. The bind-source
/// resolution has to happen where the files are: resolving a VM path on
/// this host walked up to `/var/home`, which exists here, and refused the
/// project's own `${localWorkspaceFolder}` as "outside the workspace".
pub fn validate_security_via(
    files: &taste_core::files::Files,
    config: &DevcontainerConfig,
    workspace_root: &Path,
) -> Result<()> {
    validate_run_args(&config.run_args)?;
    validate_build(config)?;
    for port in &config.forward_ports {
        if *port < 1024 {
            bail!(
                "devcontainer.json forwardPorts: {port} is privileged;                  only ports ≥ 1024 are published (the repo is untrusted)"
            );
        }
    }
    if config.forward_ports.len() > 32 {
        bail!("devcontainer.json forwardPorts: more than 32 ports");
    }
    if let Some(mount) = &config.workspace_mount {
        validate_mount(files, mount, workspace_root)?;
    }
    for mount in &config.mounts {
        if let Some(mount) = mount.as_str() {
            validate_mount(files, mount, workspace_root)?;
        } else {
            bail!(
                "devcontainer.json: object-form mounts are not supported yet; use the string form"
            );
        }
    }
    Ok(())
}

/// The build section, held to the format own rule: **devcontainer
/// configuration is machine-independent.** It names no host path, so
/// neither of these is a value to be checked — one is refused outright and
/// the other may only be a filename.
///
/// Phrased as portability rather than as suspicion on purpose. An author
/// whose config we reject learns something true about their config; a
/// hostile one gets no special-cased error to probe.
fn validate_build(config: &DevcontainerConfig) -> Result<()> {
    if let Some(build) = &config.build {
        if build.context.is_some() {
            bail!(
                "devcontainer.json build.context: not supported. The build context is \
                 always the .devcontainer directory, so the configuration stays \
                 machine-independent — it works unchanged here, in VS Code, and in \
                 Codespaces. Put what the image needs beside the Containerfile."
            );
        }
    }
    let dockerfile = config
        .build
        .as_ref()
        .and_then(|b| b.dockerfile.clone())
        .or_else(|| config.dockerfile.clone());
    if let Some(name) = dockerfile {
        let looks_like_a_path = name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || Path::new(&name).is_absolute();
        if looks_like_a_path {
            bail!(
                "devcontainer.json dockerfile \"{name}\": must be a plain file name next to \
                 devcontainer.json, not a path. Paths make the configuration \
                 machine-dependent."
            );
        }
    }
    Ok(())
}

fn validate_run_args(run_args: &[String]) -> Result<()> {
    let mut iter = run_args.iter().peekable();
    while let Some(arg) = iter.next() {
        if STRIPPED_FLAGS.contains(&arg.as_str()) {
            continue;
        }
        if ALLOWED_FLAGS_WITH_VALUE.contains(&arg.as_str()) {
            if iter.next().is_none() {
                bail!("devcontainer.json runArgs: {arg} is missing its value");
            }
            continue;
        }
        // Nesting's own flags, in either spelling, and only its values.
        let valued = [
            ("--security-opt", ALLOWED_SECURITY_OPTS),
            ("--device", ALLOWED_DEVICES),
            ("--cap-add", ALLOWED_CAPS),
        ];
        if let Some((flag, allowed)) = valued
            .iter()
            .find(|(flag, _)| arg == flag || arg.starts_with(&format!("{flag}=")))
        {
            let value = match arg.strip_prefix(&format!("{flag}=")) {
                Some(value) => value.to_string(),
                None => match iter.next() {
                    Some(value) => value.clone(),
                    None => bail!("devcontainer.json runArgs: {arg} is missing its value"),
                },
            };
            if !allowed.contains(&value.as_str()) {
                bail!(
                    "devcontainer.json runArgs: \"{flag} {value}\" is not allowed \
                     (the repo is untrusted; {flag} takes only {}, which is what running \
                     podman inside the container needs)",
                    allowed.join(", ")
                );
            }
            continue;
        }
        let allowed = ALLOWED_FLAG_PREFIXES.iter().any(|prefix| {
            if let Some(bare) = prefix.strip_suffix('=') {
                arg == bare || arg.starts_with(prefix)
            } else {
                arg == prefix || arg.starts_with(&format!("{prefix}:"))
            }
        });
        if !allowed {
            bail!(
                "devcontainer.json runArgs: \"{arg}\" is not allowed \
                 (the repo is untrusted; only resource limits, env, \
                 --userns=keep-id, --hostname, --init, labels, and the \
                 flags podman-in-podman needs pass)"
            );
        }
    }
    Ok(())
}

/// A mount string (`source=…,target=…,type=…`) may bind only paths inside
/// the workspace, or use named volumes.
fn validate_mount(
    files: &taste_core::files::Files,
    mount: &str,
    workspace_root: &Path,
) -> Result<()> {
    let mut source: Option<String> = None;
    let mut mount_type: Option<String> = None;
    for part in mount.split(',') {
        let mut kv = part.splitn(2, '=');
        match (kv.next().map(str::trim), kv.next().map(str::trim)) {
            (Some("source") | Some("src"), Some(v)) => source = Some(v.to_string()),
            (Some("type"), Some(v)) => mount_type = Some(v.to_string()),
            _ => {}
        }
    }
    let mount_type = mount_type.unwrap_or_else(|| "volume".into());
    match mount_type.as_str() {
        "volume" => Ok(()),
        "bind" => {
            let Some(source) = source else {
                bail!("devcontainer.json mount \"{mount}\": bind mount without a source");
            };
            let expanded = source.replace(
                "${localWorkspaceFolder}",
                &workspace_root.display().to_string(),
            );
            let path = Path::new(&expanded);
            if !path.is_absolute() || !path.starts_with(workspace_root) || expanded.contains("..") {
                bail!(
                    "devcontainer.json mount \"{mount}\": bind sources must stay \
                     inside the workspace (the repo is untrusted)"
                );
            }
            // Lexical containment is not enough: the repo can commit a
            // symlink pointing anywhere. Resolve and re-check. A source
            // that does not exist yet is fine — the IDE makes the directory
            // before `podman run` (`bind_sources`; podman itself refuses
            // with "statfs …: no such file or directory"), and a config
            // that binds `${localWorkspaceFolder}/vendor` before anything
            // has installed into it is the ordinary first day of a project
            // (refusing it as "bind source does not exist" left a fresh
            // devcontainer.json passed over with nothing on screen saying
            // so, 2026-09-16) — so what is resolved is its nearest existing
            // ancestor, which is where a symlink could sit.
            if !files.is_local() {
                // Where the files are, component by component: a symlink
                // anywhere under the root on the way to the source is the
                // repo pointing the bind elsewhere, and is refused by
                // name; a component that does not exist ends the walk,
                // since nothing below it can be a link yet.
                let mut probe = workspace_root.to_path_buf();
                for component in path
                    .strip_prefix(workspace_root)
                    .unwrap_or(path)
                    .components()
                {
                    probe.push(component);
                    match files.stat(&probe) {
                        Ok(stat) if stat.kind == taste_core::files::Kind::Symlink => bail!(
                            "devcontainer.json mount \"{mount}\": {} is a symlink, so the \
                             source could resolve outside the workspace (the repo is untrusted)",
                            probe.display()
                        ),
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                        Err(e) => bail!(
                            "devcontainer.json mount \"{mount}\": bind source cannot be \
                             resolved: {e}"
                        ),
                    }
                }
                return Ok(());
            }
            let canonical_root = workspace_root
                .canonicalize()
                .unwrap_or_else(|_| workspace_root.to_path_buf());
            let mut probe = path.to_path_buf();
            while !probe.exists() {
                match probe.parent() {
                    Some(parent) => probe = parent.to_path_buf(),
                    None => bail!(
                        "devcontainer.json mount \"{mount}\": bind source has no existing \
                         ancestor"
                    ),
                }
            }
            match probe.canonicalize() {
                Ok(resolved) if resolved.starts_with(&canonical_root) => Ok(()),
                Ok(resolved) => bail!(
                    "devcontainer.json mount \"{mount}\": source resolves to {} — \
                     outside the workspace (the repo is untrusted)",
                    resolved.display()
                ),
                Err(e) => {
                    bail!(
                        "devcontainer.json mount \"{mount}\": bind source cannot be resolved: {e}"
                    )
                }
            }
        }
        "tmpfs" => Ok(()),
        other => bail!("devcontainer.json mount \"{mount}\": unsupported type \"{other}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_sources_are_the_in_workspace_bind_mounts_expanded() {
        let root = Path::new("/w/proj");
        let config: DevcontainerConfig = serde_json::from_str(
            r#"{"image": "img", "mounts": [
                "source=${localWorkspaceFolder}/vendor,target=/var/www/vendor,type=bind,consistency=cached",
                "source=cache,target=/cache,type=volume",
                "source=/etc,target=/host-etc,type=bind"
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            bind_sources(&config, root),
            vec![std::path::PathBuf::from("/w/proj/vendor")]
        );
    }

    fn config_with(json: &str) -> (tempfile::TempDir, DevcontainerConfig) {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), json).unwrap();
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        (dir, config)
    }

    #[test]
    fn benign_config_passes() {
        let (dir, config) = config_with(
            r#"{
                "image": "img",
                "runArgs": ["--userns=keep-id:uid=1000,gid=1000", "-e", "FOO=1", "--init"],
                "mounts": ["source=my-cache,target=/cache,type=volume"]
            }"#,
        );
        validate_security(&config, dir.path()).unwrap();
    }

    #[test]
    fn boundary_weakening_flags_are_rejected() {
        // (--privileged is absent: it is tolerated-and-stripped for
        // VS Code/Codespaces compatibility, see below.)
        for bad in [
            "--security-opt=seccomp=unconfined",
            "--security-opt=apparmor=unconfined",
            "--cap-add=ALL",
            "--device=/dev/kvm",
            "--device=/dev/sda",
            "--pid=host",
            "--network=host",
            "-v",
        ] {
            let (dir, config) =
                config_with(&format!(r#"{{"image": "img", "runArgs": ["{bad}"]}}"#));
            assert!(
                validate_security(&config, dir.path()).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn forward_ports_validated() {
        let (dir, config) = config_with(r#"{"image": "img", "forwardPorts": [8080, 3000]}"#);
        validate_security(&config, dir.path()).unwrap();
        let (dir, config) = config_with(r#"{"image": "img", "forwardPorts": [80]}"#);
        assert!(validate_security(&config, dir.path()).is_err());
    }

    #[test]
    fn privileged_is_tolerated_for_compat_but_stripped() {
        // A VS Code/Codespaces-style systemd config validates fine; the
        // supervisor drops the flag before podman ever sees it.
        let (dir, config) = config_with(
            r#"{"image": "img", "runArgs": ["--privileged"], "overrideCommand": false}"#,
        );
        validate_security(&config, dir.path()).unwrap();
        assert!(STRIPPED_FLAGS.contains(&"--privileged"));
    }

    #[test]
    fn nesting_flags_pass_in_either_spelling_and_only_theirs() {
        let (dir, config) = config_with(
            r#"{"image": "img", "runArgs": [
                "--security-opt=label=type:container_engine_t",
                "--security-opt", "unmask=ALL",
                "--security-opt=label=disable",
                "--device", "/dev/fuse"
            ]}"#,
        );
        validate_security(&config, dir.path()).unwrap();
        let (dir, config) = config_with(r#"{"image": "img", "runArgs": ["--cap-add=SYS_ADMIN"]}"#);
        validate_security(&config, dir.path()).unwrap();
        for bad in [
            r#"["--cap-add", "NET_ADMIN"]"#,
            r#"["--cap-add=ALL"]"#,
            r#"["--security-opt", "seccomp=unconfined"]"#,
            r#"["--device", "/dev/kvm"]"#,
            r#"["--security-opt"]"#,
        ] {
            let (dir, config) = config_with(&format!(r#"{{"image": "img", "runArgs": {bad}}}"#));
            assert!(validate_security(&config, dir.path()).is_err(), "{bad}");
        }
    }

    #[test]
    fn privileged_becomes_the_nesting_set_once() {
        let (_dir, plain) = config_with(r#"{"image": "img"}"#);
        assert!(privileged_run_args(&plain).is_empty());
        for asked in [
            r#"{"image": "img", "runArgs": ["--privileged"]}"#,
            r#"{"image": "img", "privileged": true}"#,
        ] {
            let (_dir, config) = config_with(asked);
            assert_eq!(privileged_run_args(&config), NESTING_RUN_ARGS, "{asked}");
        }
        // A flag the config already states is not stated twice.
        let (_dir, config) = config_with(
            r#"{"image": "img", "privileged": true,
                "runArgs": ["--security-opt", "unmask=ALL", "--device", "/dev/fuse",
                            "--device=/dev/net/tun", "--cap-add", "SYS_ADMIN"]}"#,
        );
        assert_eq!(
            privileged_run_args(&config),
            vec!["--security-opt=label=type:container_engine_t"]
        );
    }

    #[test]
    fn systemd_run_arg_is_allowed() {
        let (dir, config) =
            config_with(r#"{"image": "img", "runArgs": ["--userns=keep-id", "--systemd=always"]}"#);
        validate_security(&config, dir.path()).unwrap();
    }

    /// The build context was the one host path the config could name, and
    /// it was unchecked: `context: "/home/you"` plus `COPY . /loot` bakes a
    /// home directory into an image. It is not a value to validate — a
    /// machine-independent config has no business naming one at all.
    #[test]
    fn build_context_cannot_be_named_at_all() {
        let (_dir, config) = config_with(
            r#"{"build": {"dockerfile": "Containerfile", "context": "/home/someone"}}"#,
        );
        let error = validate_security(&config, Path::new("/work/p"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("machine-independent"), "{error}");

        // Relative is refused too: the point is that the key does not
        // exist, not that absolute paths are suspicious.
        let (_dir, relative) =
            config_with(r#"{"build": {"dockerfile": "Containerfile", "context": ".."}}"#);
        assert!(validate_security(&relative, Path::new("/work/p")).is_err());
    }

    #[test]
    fn the_dockerfile_may_only_be_a_file_name() {
        for name in [
            "../../etc/Containerfile",
            "/etc/Containerfile",
            "sub/Containerfile",
        ] {
            let (_dir, config) =
                config_with(&format!(r#"{{"build": {{"dockerfile": "{name}"}}}}"#));
            let error = validate_security(&config, Path::new("/work/p"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("plain file name"), "{name}: {error}");
        }
        let (_dir, ok) = config_with(r#"{"build": {"dockerfile": "Containerfile"}}"#);
        assert!(validate_security(&ok, Path::new("/work/p")).is_ok());
    }

    #[test]
    fn bind_mounts_outside_workspace_are_rejected() {
        let (dir, config) = config_with(
            r#"{
                "image": "img",
                "mounts": ["source=/home/user/.ssh,target=/root/.ssh,type=bind"]
            }"#,
        );
        assert!(validate_security(&config, dir.path()).is_err());
    }

    #[test]
    fn workspace_relative_bind_mounts_pass() {
        let (dir, config) = config_with(
            r#"{
                "image": "img",
                "mounts": ["source=${localWorkspaceFolder}/data,target=/data,type=bind"]
            }"#,
        );
        std::fs::create_dir(dir.path().join("data")).unwrap();
        validate_security(&config, dir.path()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_bind_sources_are_rejected() {
        let outside = tempfile::tempdir().unwrap();
        let (dir, config) = config_with(
            r#"{
                "image": "img",
                "mounts": ["source=${localWorkspaceFolder}/data,target=/data,type=bind"]
            }"#,
        );
        // Lexically inside the workspace, resolves outside it.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("data")).unwrap();
        let err = validate_security(&config, dir.path()).unwrap_err();
        assert!(err.to_string().contains("outside the workspace"));
    }

    #[test]
    fn traversal_in_bind_source_is_rejected() {
        let (dir, config) = config_with(
            r#"{
                "image": "img",
                "mounts": ["source=${localWorkspaceFolder}/../secrets,target=/s,type=bind"]
            }"#,
        );
        assert!(validate_security(&config, dir.path()).is_err());
    }
}
