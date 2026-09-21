//! **One image build, wherever it runs.**
//!
//! The supervisor builds a project's image with a streamed log and a
//! state machine around it; the keeper needs the baseline image built in a
//! VM's podman with neither. What they must not have is two argument
//! lists, because the list carries decisions — which capabilities a build
//! is denied, the memory ceiling, the label that ties an image to its
//! workspace for cleanup — and two copies of a decision are one that will
//! drift. [`build_args`] is the list; both callers compose from it.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::DevcontainerConfig;
use crate::substrate::Substrate;

/// The arguments to `podman build` for `config`, tagged `tag`, from a
/// staged copy of its context at `staged` (the Dockerfile at
/// `staged_dockerfile`).
pub(crate) fn build_args(
    config: &DevcontainerConfig,
    tag: &str,
    staged_dockerfile: &Path,
    staged: &Path,
    workspace_key: &str,
) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "-t".into(),
        tag.to_string(),
        "-f".into(),
        staged_dockerfile.display().to_string(),
        // A RUN step cannot reach the host filesystem, but it can still
        // allocate and sit there. Capabilities it never needs, and a
        // memory ceiling it does.
        //
        // **Not `--cap-drop=all`, which does not work.** It was here, and
        // it made every `microdnf install` — including the IDE's own
        // baseline — fail with `cpio: mkdir failed - Permission denied`:
        // rpm needs `CHOWN`, `FOWNER`, `DAC_OVERRIDE`, `SETFCAP` and
        // friends to unpack a package with correct ownership, and those
        // are exactly what podman grants a build by default. Verified live
        // on this host and inside a podman machine, both of which failed
        // identically with `all` and both of which pass with the set below
        // — so it was never a substrate difference, and the baseline rung
        // that exists so nothing else can break was itself broken.
        //
        // What is dropped instead is what a build genuinely never needs:
        // binding privileged ports and crafting raw packets. That is
        // hygiene, not a wall — CLAUDE.md is explicit that agent and repo
        // code are one principal, so a build's confinement is the
        // container's, not this flag's.
        //
        // No --pids-limit: that is a `podman run` flag, not a `podman
        // build` one, and passing it fails the build — which would strand
        // the IDE in safe mode. Verified against `podman build --help`
        // rather than assumed. A fork bomb in a RUN step is therefore
        // still unbounded; --ulimit may be the substitute, unverified.
        "--cap-drop=NET_BIND_SERVICE,NET_RAW".into(),
        "--memory".into(),
        "8g".into(),
        // The tag is shared between environments with identical config,
        // so the workspace tie an image needs for cleanup rides on a label
        // instead of on its name.
        "--label".into(),
        format!(
            "{}={workspace_key}",
            taste_core::environment::LABEL_WORKSPACE
        ),
    ];
    if let Some(build) = &config.build {
        for (k, v) in &build.args {
            args.push("--build-arg".into());
            args.push(format!("{k}={v}"));
        }
    }
    args.push(staged.display().to_string());
    args
}

/// The image `config` builds, present on `substrate`: its tag, built there
/// if it is not yet. For a config that names a registry image, that image
/// is pulled instead. No log streaming — this is for images the IDE
/// builds on its own account, where the failure is the error and the
/// error names podman's last line.
pub async fn ensure_image(
    substrate: &Substrate,
    config: &DevcontainerConfig,
    workspace_key: &str,
) -> Result<String> {
    let Some(dockerfile) = config.dockerfile_path() else {
        let image = config
            .image
            .clone()
            .context("the config names neither a Dockerfile nor an image")?;
        run(substrate, vec!["pull".into(), image.clone()]).await?;
        return Ok(image);
    };
    let tag = taste_core::environment::env_image_tag(&crate::hash::build_hash(config)?);
    if run(
        substrate,
        vec!["image".into(), "exists".into(), tag.clone()],
    )
    .await
    .is_ok()
    {
        return Ok(tag);
    }
    let staged = crate::supervisor::stage_build_context(&config.build_context(), &tag)?;
    let staged_dockerfile = dockerfile
        .file_name()
        .map(|f| staged.join(f))
        .unwrap_or_else(|| staged.join("Containerfile"));
    let args = build_args(config, &tag, &staged_dockerfile, &staged, workspace_key);
    run(substrate, args).await.context("building the image")?;
    Ok(tag)
}

async fn run(substrate: &Substrate, args: Vec<String>) -> Result<()> {
    let output = substrate
        .command(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running podman")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("podman gave no reason");
        bail!(
            "podman {}: {last}",
            args.first().map(String::as_str).unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list carries the decisions; this pins them so a change is a
    /// change to one place and one test.
    #[test]
    fn the_build_arguments_carry_the_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let dc = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(
            dc.join("devcontainer.json"),
            r#"{"build": {"dockerfile": "Containerfile", "args": {"A": "1"}}}"#,
        )
        .unwrap();
        std::fs::write(dc.join("Containerfile"), "FROM scratch\n").unwrap();
        let config = DevcontainerConfig::discover(dir.path()).unwrap().unwrap();
        let args = build_args(
            &config,
            "taste-img-abc",
            Path::new("/staged/Containerfile"),
            Path::new("/staged"),
            "799f",
        );
        assert_eq!(args[0], "build");
        assert!(args.contains(&"--cap-drop=NET_BIND_SERVICE,NET_RAW".to_string()));
        assert!(args.contains(&"--memory".to_string()));
        assert!(args.contains(&"taste.workspace=799f".to_string()));
        assert!(args.contains(&"A=1".to_string()));
        assert_eq!(args.last().unwrap(), "/staged", "the context comes last");
        assert!(!args.iter().any(|a| a.contains("pids-limit")));
    }
}
