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

/// Flags accepted for cross-ecosystem compatibility but never passed to
/// podman. Docker needs `--privileged` for systemd-in-container; rootless
/// podman does not (`--systemd` handles it), so a devcontainer.json shared
/// with VS Code / Codespaces keeps working here — with strictly *fewer*
/// privileges, never more.
pub const STRIPPED_FLAGS: &[&str] = &["--privileged"];

pub fn validate_security(config: &DevcontainerConfig, workspace_root: &Path) -> Result<()> {
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
        validate_mount(mount, workspace_root)?;
    }
    for mount in &config.mounts {
        if let Some(mount) = mount.as_str() {
            validate_mount(mount, workspace_root)?;
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
                 --userns=keep-id, --hostname, --init and labels pass)"
            );
        }
    }
    Ok(())
}

/// A mount string (`source=…,target=…,type=…`) may bind only paths inside
/// the workspace, or use named volumes.
fn validate_mount(mount: &str, workspace_root: &Path) -> Result<()> {
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
            // symlink pointing anywhere. Resolve and re-check; a source
            // that doesn't exist can't be mounted anyway.
            let canonical_root = workspace_root
                .canonicalize()
                .unwrap_or_else(|_| workspace_root.to_path_buf());
            match path.canonicalize() {
                Ok(resolved) if resolved.starts_with(&canonical_root) => Ok(()),
                Ok(resolved) => bail!(
                    "devcontainer.json mount \"{mount}\": source resolves to {} — \
                     outside the workspace (the repo is untrusted)",
                    resolved.display()
                ),
                Err(_) => {
                    bail!("devcontainer.json mount \"{mount}\": bind source does not exist")
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
            "--security-opt=label=disable",
            "--cap-add=ALL",
            "--device=/dev/kvm",
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
