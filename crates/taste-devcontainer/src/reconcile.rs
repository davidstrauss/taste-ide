//! Startup reconciliation: matching podman's reality to the environments
//! this workspace believes in, and removing what an older naming scheme
//! left behind.
//!
//! Two rules, both deliberate:
//!
//! - **Reconcile by label, never by name.** A container's
//!   `taste.workspace`/`taste.env` labels are its own claim about what it
//!   is; a name is only what some build of the IDE happened to compute. The
//!   naming scheme has already changed once and will change again.
//! - **Old-scheme resources are removed, not adopted.** taste-ide is alpha
//!   (see ARCHITECTURE → Compatibility posture): a container from the
//!   single-environment scheme holds this workspace's forwarded ports and
//!   answers to nobody, so it is stopped and the removal is reported once.
//!   Pick up the pieces; do not carry them.
//!
//! The decisions live in pure functions over names and labels so they are
//! tested without a container runtime; only [`sweep_legacy_resources`]
//! talks to podman.

use std::path::Path;

use anyhow::Result;
use taste_core::environment::{legacy_container_name, legacy_image_tag};

/// One container as podman reports it, reduced to what a decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEntry {
    pub id: String,
    pub name: String,
    /// The `taste.workspace` label, empty when absent.
    pub workspace: String,
    /// The `taste.env` label, empty when absent — which is exactly what
    /// makes a container old-scheme.
    pub env: String,
}

/// What a sweep removed, for the one report the user is owed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub containers: Vec<String>,
    pub images: Vec<String>,
}

impl SweepReport {
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty() && self.images.is_empty()
    }

    /// One sentence, said once. A silent reset looks like a bug; a reset
    /// explained once is alpha.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.containers.is_empty() {
            parts.push(format!(
                "container{} {}",
                plural(self.containers.len()),
                self.containers.join(", ")
            ));
        }
        if !self.images.is_empty() {
            parts.push(format!(
                "image{} {}",
                plural(self.images.len()),
                self.images.join(", ")
            ));
        }
        format!(
            "Removed {} from the previous single-environment naming scheme; \
             this workspace's environments are rebuilt under the new names.",
            parts.join(" and ")
        )
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Podman prints a missing template key this way; treat it as absent.
fn label(value: &str) -> &str {
    let value = value.trim();
    if value == "<no value>" {
        ""
    } else {
        value
    }
}

/// Containers belonging to this workspace that the single-environment
/// scheme created.
///
/// A container is old-scheme when it wears this workspace's pre-environment
/// name **and** carries no `taste.env` label. Both halves matter: the name
/// keeps the sweep to our own workspace (another project's container is
/// none of our business), and the missing label is what says no environment
/// owns it. A container with the label is current by definition, whatever
/// its name.
pub fn legacy_containers(workspace_root: &Path, entries: &[ContainerEntry]) -> Vec<ContainerEntry> {
    let legacy = legacy_container_name(workspace_root);
    let prefix = format!("{legacy}-");
    entries
        .iter()
        .filter(|entry| {
            label(&entry.env).is_empty()
                && (entry.name == legacy || entry.name.starts_with(&prefix))
        })
        .cloned()
        .collect()
}

/// Images belonging to this workspace that the single-environment scheme
/// built. Podman reports local images as `localhost/<tag>`, so both forms
/// are matched.
pub fn legacy_images(workspace_root: &Path, repositories: &[String]) -> Vec<String> {
    let legacy = legacy_image_tag(workspace_root);
    let suffix = format!("/{legacy}");
    repositories
        .iter()
        .filter(|repo| {
            let repo = repo.trim();
            repo == legacy || repo.ends_with(&suffix)
        })
        .cloned()
        .collect()
}

/// `podman`, or `flatpak-spawn --host podman` when the IDE is sandboxed —
/// podman always runs on the host.
pub(crate) fn podman(sandboxed: bool, args: &[String]) -> tokio::process::Command {
    if sandboxed {
        let mut cmd = tokio::process::Command::new("flatpak-spawn");
        cmd.arg("--host").arg("podman").args(args);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("podman");
        cmd.args(args);
        cmd
    }
}

async fn capture(sandboxed: bool, args: Vec<String>) -> Result<String> {
    let output = podman(sandboxed, &args)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    // The exit status is checked, not just the spawn: the sweep reports
    // what it REMOVED, and a `podman rm` that refused must not be counted
    // as a removal in the one message the user gets.
    if !output.status.success() {
        anyhow::bail!(
            "podman {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Every container podman knows about, with the labels a decision needs.
pub async fn list_containers(sandboxed: bool) -> Result<Vec<ContainerEntry>> {
    let out = capture(
        sandboxed,
        vec![
            "ps".into(),
            "-a".into(),
            "--format".into(),
            format!(
                "{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{index .Labels \"{}\"}}}}\t{{{{index .Labels \"{}\"}}}}",
                taste_core::environment::LABEL_WORKSPACE,
                taste_core::environment::LABEL_ENV,
            ),
        ],
    )
    .await?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some(ContainerEntry {
                id: fields.next()?.to_string(),
                name: fields.next()?.to_string(),
                workspace: fields.next().unwrap_or_default().to_string(),
                env: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

/// Find and remove this workspace's old-scheme containers and images.
///
/// Best-effort: a removal that fails is simply not reported as removed. The
/// sweep must never be able to keep the IDE from starting.
pub async fn sweep_legacy_resources(workspace_root: &Path, sandboxed: bool) -> SweepReport {
    let mut report = SweepReport::default();

    let containers = list_containers(sandboxed).await.unwrap_or_default();
    for entry in legacy_containers(workspace_root, &containers) {
        let removed = capture(
            sandboxed,
            vec![
                "rm".into(),
                "-f".into(),
                "-t".into(),
                "2".into(),
                entry.name.clone(),
            ],
        )
        .await
        .is_ok();
        if removed {
            report.containers.push(entry.name);
        }
    }

    let repositories: Vec<String> = capture(
        sandboxed,
        vec!["images".into(), "--format".into(), "{{.Repository}}".into()],
    )
    .await
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect();
    for image in legacy_images(workspace_root, &repositories) {
        // Not `-f`: an image another environment's container is using must
        // survive, and podman refusing is the right answer.
        if capture(sandboxed, vec!["rmi".into(), image.clone()])
            .await
            .is_ok()
        {
            report.images.push(image);
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use taste_core::environment::{env_container_name, EnvironmentId};

    const ROOT: &str = "/work/project";

    fn entry(name: &str, env: &str) -> ContainerEntry {
        ContainerEntry {
            id: "deadbeef".into(),
            name: name.into(),
            workspace: String::new(),
            env: env.into(),
        }
    }

    #[test]
    fn the_old_scheme_container_is_recognised_and_the_new_one_is_not() {
        let root = Path::new(ROOT);
        let legacy = legacy_container_name(root);
        let current = env_container_name(root, &EnvironmentId::primary());

        let found = legacy_containers(
            root,
            &[
                entry(&legacy, ""),
                entry(&current, "primary"),
                entry(
                    &env_container_name(root, &EnvironmentId::parse("review").unwrap()),
                    "review",
                ),
            ],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, legacy);
    }

    /// Another project's container is not ours to remove, however old it is.
    #[test]
    fn other_workspaces_are_left_alone() {
        let root = Path::new(ROOT);
        let other = legacy_container_name(Path::new("/work/other"));
        assert!(legacy_containers(root, &[entry(&other, "")]).is_empty());
        // Nor is anything that simply is not ours.
        assert!(legacy_containers(root, &[entry("postgres", "")]).is_empty());
    }

    /// The label is the deciding fact: a container wearing an old-looking
    /// name but claiming an environment belongs to that environment.
    #[test]
    fn a_labelled_container_is_never_swept_whatever_its_name() {
        let root = Path::new(ROOT);
        let legacy = legacy_container_name(root);
        assert!(legacy_containers(root, &[entry(&legacy, "primary")]).is_empty());
        assert!(legacy_containers(root, &[entry(&format!("{legacy}-odd"), "review")]).is_empty());
        // `<no value>` is podman's way of saying the label is absent.
        assert_eq!(
            legacy_containers(root, &[entry(&legacy, "<no value>")]).len(),
            1
        );
    }

    #[test]
    fn legacy_images_match_bare_and_localhost_forms() {
        let root = Path::new(ROOT);
        let legacy = legacy_image_tag(root);
        let found = legacy_images(
            root,
            &[
                legacy.clone(),
                format!("localhost/{legacy}"),
                "taste-img-abcdef012345".into(),
                "docker.io/library/rust".into(),
            ],
        );
        assert_eq!(found.len(), 2, "{found:?}");
    }

    /// A sweep that removed nothing says nothing; one that did says it once,
    /// naming what went.
    #[test]
    fn the_report_names_what_it_removed() {
        assert!(SweepReport::default().is_empty());
        let report = SweepReport {
            containers: vec!["taste-f4ef24a9f365".into()],
            images: vec!["localhost/taste-f4ef24a9f365-image".into()],
        };
        let summary = report.summary();
        assert!(summary.contains("taste-f4ef24a9f365"), "{summary}");
        assert!(summary.contains("container "), "{summary}");
        assert!(summary.contains("image "), "{summary}");
    }
}
