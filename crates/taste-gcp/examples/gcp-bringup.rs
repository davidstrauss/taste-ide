//! Bring a project's GCP access up by hand, before the IDE's own surfaces
//! for it exist (ENVIRONMENTS → "A model on a cloud VM", Phase 1).
//!
//! ```text
//! gcp-bringup install
//! gcp-bringup sign-in STATE_DIR PROJECT
//! gcp-bringup setup   STATE_DIR PROJECT
//! gcp-bringup check   STATE_DIR PROJECT
//! ```
//!
//! `install` fetches the pinned gcloud into the IDE's data directory —
//! never the base system. `sign-in` runs its browser sign-in into
//! `STATE_DIR/gcloud`, the project's own configuration; STATE_DIR is the
//! workspace's state directory (`…/taste-ide/workspaces/<name>-<id>/`), so
//! the IDE finds the sign-in there later. `setup` runs
//! `build-aux/gcp-setup.sh` with it, as you. `check` asks the project, as
//! the service account, which of the role's permissions it holds; it
//! creates nothing, so it costs nothing.
//!
//! Build it in the devcontainer and run it on the host, where a browser
//! is: `cargo build -p taste-gcp --example gcp-bringup`, then
//! `target/debug/examples/gcp-bringup …`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use taste_gcp::gcloud::{self, Gcloud};
use taste_gcp::rest::{Endpoints, Gcp, TokenSource};
use taste_gcp::setup::{self, PERMISSIONS};

async fn installed() -> Result<PathBuf> {
    let root = gcloud::sdk_root();
    let shown = std::sync::atomic::AtomicU64::new(u64::MAX);
    gcloud::ensure_installed(&root, move |done, total| {
        let mib = done >> 20;
        if shown.swap(mib, std::sync::atomic::Ordering::Relaxed) == mib {
            return;
        }
        eprint!(
            "\rfetching gcloud {}: {} of {} MiB",
            gcloud::SDK.version,
            done >> 20,
            total >> 20
        );
    })
    .await
}

fn project_gcloud(binary: PathBuf, state: &Path, project: &str) -> Result<Gcloud> {
    if !setup::valid_project_id(project) {
        bail!("{project:?} is not a GCP project id");
    }
    Ok(Gcloud {
        binary,
        config_dir: state.join("gcloud"),
        project: project.to_string(),
        impersonate: Some(setup::service_account(project)),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["install"] => {
            let binary = installed().await?;
            eprintln!();
            println!("{}", binary.display());
        }
        ["sign-in", state, project] => {
            let gcloud = project_gcloud(installed().await?, Path::new(state), project)?;
            if !gcloud.sign_in().status().await?.success() {
                bail!("the sign-in did not finish");
            }
        }
        ["setup", state, project] => {
            let gcloud = project_gcloud(installed().await?, Path::new(state), project)?;
            if !gcloud.setup(false).status().await?.success() {
                bail!("the setup failed; its output above says where");
            }
        }
        ["check", state, project] => {
            let gcloud = project_gcloud(installed().await?, Path::new(state), project)?;
            let gcp = Gcp::new(Arc::new(TokenSource::gcloud(gcloud)), Endpoints::default());
            let held = gcp.test_permissions(project, PERMISSIONS).await?;
            let missing: Vec<_> = PERMISSIONS
                .iter()
                .filter(|p| !held.iter().any(|h| h == *p))
                .collect();
            for permission in &missing {
                println!("missing: {permission}");
            }
            if !missing.is_empty() {
                bail!(
                    "{} of {} permissions missing; run setup again",
                    missing.len(),
                    PERMISSIONS.len()
                );
            }
            println!(
                "{} holds all {} permissions on {project}.",
                setup::service_account(project),
                PERMISSIONS.len()
            );
        }
        _ => bail!(
            "usage: gcp-bringup install\n       \
             gcp-bringup sign-in|setup|check STATE_DIR PROJECT"
        ),
    }
    Ok(())
}
