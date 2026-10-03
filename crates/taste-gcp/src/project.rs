//! What one project keeps about its GCP access: the choices in
//! `cloud-model.json`, and the gcloud configuration beside it.
//!
//! Both live in the workspace's IDE state directory
//! (`taste_core::state::workspace_state_dir`), never the checkout, for the
//! reason the private model's file gives: an agent that could write them
//! could aim the IDE's own requests, and its GCP spend, wherever it liked.
//! Neither is a secret by itself except the sign-in inside `gcloud/`,
//! which is gcloud's own and is written by gcloud.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::gcloud::Gcloud;
use crate::setup;

/// The project's choices file, in its state directory.
pub const FILE: &str = "cloud-model.json";
/// The project's own gcloud configuration, in its state directory.
pub const GCLOUD_DIR: &str = "gcloud";

/// The choices. Only the project so far; the zone and the monthly cap
/// join it when the machines do (ENVIRONMENTS → "The ledger, and the one
/// setting").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudProject {
    pub project: String,
}

pub fn load(state_dir: &Path) -> Result<Option<CloudProject>> {
    let path = state_dir.join(FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn store(state_dir: &Path, choices: &CloudProject) -> Result<()> {
    if !setup::valid_project_id(&choices.project) {
        bail!("{:?} is not a GCP project id", choices.project);
    }
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join(FILE);
    let part = path.with_extension("json.part");
    std::fs::write(&part, serde_json::to_vec_pretty(choices)?)?;
    std::fs::rename(&part, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The workspace a state directory is for. The directory is named
/// `<name>-<hash of the root>` (`taste_core::state::workspace_state_dir`),
/// and a workspace's GCP id is that hash's first eight digits, so the two
/// are recognisably the same workspace.
pub fn workspace(state_dir: &Path) -> Result<crate::resources::Workspace> {
    let name = state_dir
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no name", state_dir.display()))?;
    let hash = name.rsplit('-').next().unwrap_or("");
    crate::resources::Workspace::new(hash.get(..8).unwrap_or(hash))
        .with_context(|| format!("{name} is not a workspace's state directory"))
}

/// The project's gcloud: the IDE's pinned copy at `binary`, this
/// project's configuration, and its calls impersonating the IDE's service
/// account there.
pub fn gcloud(state_dir: &Path, binary: PathBuf, project: &str) -> Gcloud {
    Gcloud {
        binary,
        config_dir: state_dir.join(GCLOUD_DIR),
        project: project.to_string(),
        impersonate: Some(setup::service_account(project)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_round_trip_and_a_bad_id_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
        let choices = CloudProject {
            project: "my-project-1".into(),
        };
        store(dir.path(), &choices).unwrap();
        assert_eq!(load(dir.path()).unwrap(), Some(choices));
        assert!(store(
            dir.path(),
            &CloudProject {
                project: "Not A Project".into()
            }
        )
        .is_err());
    }

    #[test]
    fn a_state_directory_names_its_workspace() {
        let ws = workspace(Path::new("/x/workspaces/taste-ide-f4ef24a9f365b5e2")).unwrap();
        assert_eq!(ws.id(), "f4ef24a9");
        assert!(workspace(Path::new("/x/workspaces/not-a-hash")).is_err());
    }

    #[test]
    fn the_projects_gcloud_lives_beside_its_choices() {
        let g = gcloud(
            Path::new("/state"),
            PathBuf::from("/data/gcloud"),
            "my-project-1",
        );
        assert_eq!(g.config_dir, Path::new("/state/gcloud"));
        assert_eq!(
            g.impersonate.as_deref(),
            Some("taste-ide@my-project-1.iam.gserviceaccount.com")
        );
    }
}
