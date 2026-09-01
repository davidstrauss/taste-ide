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
use taste_core::environment::{legacy_container_name, legacy_image_tag, previous_generation_key};

use crate::substrate::Substrate;

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
            "Removed {} from this workspace's previous naming scheme; \
             its environments are rebuilt under the new names.",
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
pub(crate) fn label(value: &str) -> &str {
    let value = value.trim();
    if value == "<no value>" {
        ""
    } else {
        value
    }
}

/// Containers belonging to this workspace that a **previous naming
/// generation** created — the single-environment scheme, and the
/// multi-environment scheme from before the workspace key was re-derived.
///
/// The name is what scopes this to our own workspace, and it can do that
/// job because `taste_core::environment` domain-separates each generation's
/// key: `taste-<previous-generation-key>` is never a prefix of a current
/// name, so nothing this matches can be a container the running IDE owns.
/// Another project's containers hash to another stem and are invisible
/// here, which is the property
/// [`tests::a_foreign_workspaces_containers_are_invisible`] pins down.
///
/// Note what changed and why. The `taste.env` label used to be the deciding
/// fact — "a container with the label is current by definition" — and that
/// was true only while exactly one key derivation had ever existed. The
/// previous multi-environment generation labelled its containers too, so
/// that test would leave every one of them running and unmanaged behind the
/// re-key. The name is now the claim, and the **workspace** label is what
/// vetoes it: a container wearing this stem but claiming to belong to some
/// other workspace is not ours, whatever its name says. Single-environment
/// containers predate the labels entirely and carry none, which is why an
/// absent label is a match rather than a veto.
pub fn legacy_containers(workspace_root: &Path, entries: &[ContainerEntry]) -> Vec<ContainerEntry> {
    let legacy = legacy_container_name(workspace_root);
    let prefix = format!("{legacy}-");
    let ours = previous_generation_key(workspace_root);
    entries
        .iter()
        .filter(|entry| {
            let named_ours = entry.name == legacy || entry.name.starts_with(&prefix);
            let claim = label(&entry.workspace);
            named_ours && (claim.is_empty() || claim == ours)
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

/// A podman command against the workspace's substrate.
///
/// The one factory. It used to answer a single question — `podman`, or
/// `flatpak-spawn --host podman` under Flatpak — and now answers two, the
/// second being *which podman service*: the host's, a machine's, or a
/// remote one's. Both live on the [`Substrate`], so a call site that
/// composes podman arguments cannot get either wrong by omission.
pub(crate) fn podman(substrate: &Substrate, args: &[String]) -> tokio::process::Command {
    substrate.command(args)
}

async fn capture(substrate: &Substrate, args: Vec<String>) -> Result<String> {
    let output = podman(substrate, &args)
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
pub async fn list_containers(substrate: &Substrate) -> Result<Vec<ContainerEntry>> {
    let out = capture(
        substrate,
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
pub async fn sweep_legacy_resources(workspace_root: &Path, substrate: &Substrate) -> SweepReport {
    let mut report = SweepReport::default();

    let containers = list_containers(substrate).await.unwrap_or_default();
    for entry in legacy_containers(workspace_root, &containers) {
        let removed = capture(
            substrate,
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
        substrate,
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
        if capture(substrate, vec!["rmi".into(), image.clone()])
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

    /// A container with no labels at all — the single-environment scheme.
    fn entry(name: &str, env: &str) -> ContainerEntry {
        ContainerEntry {
            id: "deadbeef".into(),
            name: name.into(),
            workspace: String::new(),
            env: env.into(),
        }
    }

    /// A container that claims a workspace, as every labelled generation
    /// since the first one does.
    fn claimed(name: &str, workspace: &str, env: &str) -> ContainerEntry {
        ContainerEntry {
            workspace: workspace.into(),
            ..entry(name, env)
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

    /// The generation before the re-key labelled its containers, so the
    /// label alone cannot mean "current" any more — the *name* decides, and
    /// the old stem can never be a current name.
    #[test]
    fn the_previous_multi_environment_generation_is_swept_too() {
        let root = Path::new(ROOT);
        let stem = legacy_container_name(root);
        let key = previous_generation_key(root);
        let found = legacy_containers(
            root,
            &[
                claimed(&format!("{stem}-primary"), &key, "primary"),
                claimed(&format!("{stem}-review"), &key, "review"),
                // ...beside the current generation, which is untouched.
                claimed(
                    &env_container_name(root, &EnvironmentId::primary()),
                    &taste_core::environment::workspace_key(root),
                    "primary",
                ),
            ],
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|e| e.name.starts_with(&stem)));
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

    /// N windows are open at once by design. Another window's containers are
    /// invisible to this one — and the label is the veto that makes it
    /// invisible even in the case the name alone could not settle: a
    /// container wearing OUR old stem while claiming somebody else's
    /// workspace. That is not a shape podman produces on its own, which is
    /// the point — the sweep force-removes what it matches, so it declines
    /// anything whose own claim disagrees with it.
    #[test]
    fn a_foreign_workspaces_containers_are_invisible() {
        let root = Path::new(ROOT);
        let other = Path::new("/work/other");
        let foreign_key = taste_core::environment::workspace_key(other);
        let foreign_previous = previous_generation_key(other);

        // Every shape another window's containers can wear.
        let foreign = [
            claimed(
                &env_container_name(other, &EnvironmentId::primary()),
                &foreign_key,
                "primary",
            ),
            claimed(
                &env_container_name(other, &EnvironmentId::parse("review").unwrap()),
                &foreign_key,
                "review",
            ),
            claimed(
                &format!("{}-primary", legacy_container_name(other)),
                &foreign_previous,
                "primary",
            ),
            entry(&legacy_container_name(other), ""),
            // The adversarial one: our stem, their claim.
            claimed(
                &format!("{}-primary", legacy_container_name(root)),
                &foreign_key,
                "primary",
            ),
        ];
        let found = legacy_containers(root, &foreign);
        assert!(
            found.is_empty(),
            "swept another window's containers: {found:?}"
        );

        // ...and our own is still found when mixed in with all of them.
        let mut mixed = foreign.to_vec();
        mixed.push(entry(&legacy_container_name(root), ""));
        assert_eq!(legacy_containers(root, &mixed).len(), 1);
    }

    /// `<no value>` is podman's way of saying a label is absent, and an
    /// absent workspace claim is the single-environment scheme, which is
    /// exactly what the sweep is for.
    #[test]
    fn an_absent_label_reads_as_absent_not_as_a_claim() {
        let root = Path::new(ROOT);
        let legacy = legacy_container_name(root);
        assert_eq!(
            legacy_containers(root, &[claimed(&legacy, "<no value>", "<no value>")]).len(),
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
