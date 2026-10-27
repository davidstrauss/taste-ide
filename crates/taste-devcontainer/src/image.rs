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
///
/// Blocking: its callers are the keeper's container setup, which runs on
/// a blocking thread, and a live test.
pub fn ensure_image(
    substrate: &Substrate,
    config: &DevcontainerConfig,
    workspace_key: &str,
) -> Result<String> {
    ensure_image_with(substrate, config, workspace_key, &|_| {})
}

/// Whether the image `config` builds to is already on `substrate`: what
/// decides whether `ensure_image_with` will be a moment or minutes, and
/// so whether a stage is worth announcing.
pub fn image_exists(substrate: &Substrate, config: &DevcontainerConfig) -> bool {
    match config.dockerfile_path() {
        None => true,
        Some(_) => match crate::hash::build_hash(config) {
            Ok(hash) => run(
                substrate,
                vec![
                    "image".into(),
                    "exists".into(),
                    taste_core::environment::env_image_tag(&hash),
                ],
            )
            .is_ok(),
            Err(_) => false,
        },
    }
}

/// [`ensure_image`], telling `on_line` every line the build prints as it
/// prints it — podman's `STEP n/m`, the layer commit's silence, the
/// error — so a build that takes minutes in a VM is not minutes of
/// nothing (David, 2026-09-22: a first launch "hanging" on the stage
/// before it while the files service's image built).
pub fn ensure_image_with(
    substrate: &Substrate,
    config: &DevcontainerConfig,
    workspace_key: &str,
    on_line: &dyn Fn(String),
) -> Result<String> {
    let Some(dockerfile) = config.dockerfile_path() else {
        let image = config
            .image
            .clone()
            .context("the config names neither a Dockerfile nor an image")?;
        run(substrate, vec!["pull".into(), image.clone()])?;
        return Ok(image);
    };
    let tag = taste_core::environment::env_image_tag(&crate::hash::build_hash(config)?);
    if run(
        substrate,
        vec!["image".into(), "exists".into(), tag.clone()],
    )
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
    run_streaming(substrate, args, on_line).context("building the image")?;
    Ok(tag)
}

/// [`ensure_image_with`], from this machine's copy of the image when there
/// is one: what a VM of `workspace_key`'s built is saved here, and the
/// workspace's next VM loads it rather than building it again — a second,
/// not a minute of packages fetched and installed (David, 2026-10-22:
/// "Could we optimize the files service to be at least cached as a
/// build?"). A copy that will not load is removed and the image built.
/// A VM made from a golden base has the image already, so this matters
/// for an image the base does not carry: a baseline changed since the
/// base was made, or a VM on a base image with no golden one.
///
/// **Per workspace, never shared.** The copy comes out of a VM, where
/// agents run, and the image it is goes under every checkout in the VM
/// that loads it: one VM's image seeding another project's would carry
/// whatever was done to it across the line between projects' VMs. A
/// workspace's own VMs are already one project's.
pub fn ensure_image_kept(
    substrate: &Substrate,
    config: &DevcontainerConfig,
    workspace_key: &str,
    on_line: &dyn Fn(String),
) -> Result<String> {
    if config.dockerfile_path().is_none() {
        return ensure_image_with(substrate, config, workspace_key, on_line);
    }
    let tag = taste_core::environment::env_image_tag(&crate::hash::build_hash(config)?);
    let exists = |substrate: &Substrate| {
        run(
            substrate,
            vec!["image".into(), "exists".into(), tag.clone()],
        )
        .is_ok()
    };
    let kept = kept_path(workspace_key, &tag);
    // Already there — from the golden base the VM was made from
    // (`crate::golden`), which carries it for every workspace — and no
    // copy is wanted: the next VM is made from the same base.
    if exists(substrate) {
        return Ok(tag);
    }
    if kept.is_file() {
        on_line(format!(
            "loading {tag} from this machine's copy, saved by an earlier VM of this workspace"
        ));
        let loaded = run(
            substrate,
            vec![
                "load".into(),
                "-q".into(),
                "-i".into(),
                kept.display().to_string(),
            ],
        );
        match loaded {
            Ok(()) if exists(substrate) => return Ok(tag),
            Ok(()) => on_line(format!("the copy did not hold {tag}; building it")),
            Err(e) => on_line(format!("the copy would not load ({e:#}); building it")),
        }
        let _ = std::fs::remove_file(&kept);
    }
    let tag = ensure_image_with(substrate, config, workspace_key, on_line)?;
    keep_in_background(substrate, &tag, kept);
    Ok(tag)
}

/// Whether this machine has a copy of the image `config` builds, kept for
/// `workspace_key` — what decides whether a VM without the image will load
/// it in seconds or build it in minutes, and so how that is said.
pub fn has_kept_copy(config: &DevcontainerConfig, workspace_key: &str) -> bool {
    config.dockerfile_path().is_some()
        && crate::hash::build_hash(config).is_ok_and(|hash| {
            kept_path(
                workspace_key,
                &taste_core::environment::env_image_tag(&hash),
            )
            .is_file()
        })
}

/// Save a copy of `tag` to `kept`, off the caller's thread: what the caller
/// waited for is the image in the VM, and the copy is for the next one.
fn keep_in_background(substrate: &Substrate, tag: &str, kept: std::path::PathBuf) {
    let (substrate, tag) = (substrate.clone(), tag.to_string());
    std::thread::spawn(move || {
        if let Err(e) = keep(&substrate, &tag, &kept) {
            tracing::warn!("keeping a copy of {tag}: {e:#}");
        }
    });
}

/// Where this machine keeps a workspace's copy of an image:
/// `$XDG_CACHE_HOME/taste-ide/images/<workspace key>/<tag>.oci.tar`.
fn kept_path(workspace_key: &str, tag: &str) -> std::path::PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".cache")
        })
        .join("taste-ide")
        .join("images")
        .join(workspace_key)
        .join(format!("{tag}.oci.tar"))
}

/// Save `tag` from `substrate` to `kept`, under a name of its own until it
/// is whole, and remove the workspace's copies of any other image: one
/// copy a workspace, the image it builds now.
fn keep(substrate: &Substrate, tag: &str, kept: &Path) -> Result<()> {
    let dir = kept.parent().context("a kept image has a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = kept.with_extension(format!("part-{}", std::process::id()));
    let saved = run(
        substrate,
        vec![
            "save".into(),
            "--format".into(),
            "oci-archive".into(),
            "-o".into(),
            part.display().to_string(),
            tag.to_string(),
        ],
    );
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, kept).with_context(|| format!("keeping {}", kept.display()))?;
    // Other finished copies only: a `.part-` is another save under way.
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path != kept && path.extension().is_some_and(|x| x == "tar") {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// `run`, with every line of the command's output handed on as it comes.
/// Both streams: podman's build steps go to stdout and its complaints to
/// stderr, and a reader wants them in one story.
fn run_streaming(substrate: &Substrate, args: Vec<String>, on_line: &dyn Fn(String)) -> Result<()> {
    use std::io::{BufRead, BufReader};
    let mut child = substrate
        .std_command(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running podman")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);
    let mut last = String::new();
    for line in rx {
        if !line.trim().is_empty() {
            last = line.clone();
        }
        on_line(line);
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait().context("waiting for podman")?;
    if !status.success() {
        bail!(
            "podman {}: {}",
            args.first().map(String::as_str).unwrap_or_default(),
            if last.is_empty() {
                "podman gave no reason"
            } else {
                &last
            }
        );
    }
    Ok(())
}

fn run(substrate: &Substrate, args: Vec<String>) -> Result<()> {
    let output = substrate
        .std_command(&args)
        .stdin(std::process::Stdio::null())
        .output()
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

    /// A workspace's copy of an image is its own: the same image for two
    /// workspaces is two copies, never one that both load.
    #[test]
    fn a_kept_image_is_one_workspaces() {
        let a = kept_path("aaaa", "taste-img-1");
        let b = kept_path("bbbb", "taste-img-1");
        assert_ne!(a, b);
        assert!(a.ends_with("aaaa/taste-img-1.oci.tar"), "{}", a.display());
    }

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
