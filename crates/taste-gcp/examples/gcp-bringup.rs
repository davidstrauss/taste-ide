//! Bring a project's GCP access up by hand, before the IDE's own surfaces
//! for it exist (ENVIRONMENTS → "A model on a cloud VM", Phase 1).
//!
//! ```text
//! gcp-bringup install
//! gcp-bringup sign-in STATE_DIR PROJECT
//! gcp-bringup setup   STATE_DIR PROJECT
//! gcp-bringup check   STATE_DIR PROJECT
//! gcp-bringup smoke    STATE_DIR PROJECT ZONE [--keep]
//! gcp-bringup teardown STATE_DIR PROJECT ZONE
//! ```
//!
//! `install` fetches the pinned gcloud into the IDE's data directory —
//! never the base system. `sign-in` runs its browser sign-in into
//! `STATE_DIR/gcloud`, the project's own configuration; STATE_DIR is the
//! workspace's state directory (`…/taste-ide/workspaces/<name>-<id>/`), so
//! the IDE finds the sign-in there later. `setup` runs
//! `build-aux/gcp-setup.sh` with it, as you. `check` asks the project, as
//! the service account, which of the role's permissions it holds, and
//! whether the quotas let a cloud environment host be created; it creates
//! nothing, so it costs nothing.
//!
//! `smoke` proves the whole model route with a small model, gpt-oss-20b
//! (David, 2026-10-03: "Let's start with a less ambitious model to test
//! that things can work"): it mirrors the pinned weights into the
//! project's bucket with a staging VM (once), brings the serving VM up
//! with no address, pulling the weights into memory through the window
//! and saying how fast, opens gcloud's IAP tunnel to it once the window
//! is shut, asks the model one question through the Messages API with
//! the VM's key, prints the answer, and deletes the VM (`--keep` leaves it
//! running). About $0.40 an hour while anything runs, and well under a
//! dollar for the whole test; the bucket's 12 GB, about 25 cents a month,
//! stays, and `teardown` deletes everything else the plan made.
//!
//! Build it in the devcontainer and run it on the host, where a browser
//! is: `cargo build -p taste-gcp --example gcp-bringup`, then
//! `target/debug/examples/gcp-bringup …`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use taste_gcp::gcloud::{self, Gcloud};
use taste_gcp::model::{self, GuestConfigs, ModelPlan};
use taste_gcp::resources::Location;
use taste_gcp::rest::{Endpoints, Gcp, TokenSource};
use taste_gcp::setup::{self, PERMISSIONS};
use taste_gcp::{guest, lifecycle};

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

/// A fresh key for llama-server, 32 random bytes as hex.
fn random_key() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn smoke_plan(state: &Path, loc: &Location, key: Option<String>) -> Result<ModelPlan> {
    let spec = &model::GPT_OSS_20B;
    model::plan(
        &taste_gcp::project::workspace(state)?,
        loc,
        &guest::fcos_image(guest::FCOS_RELEASE),
        spec,
        &spec.machine,
        &GuestConfigs {
            staging_user_data: Some(guest::staging_ignition(spec)),
            serving_user_data: Some(guest::serving_ignition(spec)),
            serving_key: key,
        },
    )
}

/// One question to the model through the tunnel's loopback end, retried
/// while the tunnel finishes coming up.
async fn ask(port: u16, key: &str) -> Result<String> {
    let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let body = lifecycle::smoke_request(model::GPT_OSS_20B.label).to_string();
    let mut last = None;
    for _ in 0..12 {
        let request = http::Request::post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("content-type", "application/json")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .body(Full::new(Bytes::from(body.clone())))?;
        match client.request(request).await {
            Ok(response) => {
                let status = response.status();
                let bytes = response.into_body().collect().await?.to_bytes();
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if !status.is_success() {
                    bail!("the model answered {status}: {text}");
                }
                return Ok(text);
            }
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    bail!("the tunnel never carried the request: {:?}", last)
}

async fn smoke(state: &Path, project: &str, zone: &str, keep: bool) -> Result<()> {
    let gcloud = project_gcloud(installed().await?, state, project)?;
    let gcp = Gcp::new(
        Arc::new(TokenSource::gcloud(gcloud.clone())),
        Endpoints::default(),
    );
    let loc = Location::new(project, zone)?;
    let plan = smoke_plan(state, &loc, Some(random_key()?))?;
    let say = |line: &str| eprintln!("{line}");
    lifecycle::stage(&gcp, &loc, &plan, &model::GPT_OSS_20B, &say).await?;
    lifecycle::serve(&gcp, &loc, &plan, &model::GPT_OSS_20B, &say).await?;
    let key = lifecycle::serving_key(&gcp, &loc, &plan).await?;

    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    eprintln!("opening the IAP tunnel on 127.0.0.1:{port}");
    let mut tunnel = gcloud
        .tunnel(&plan.names.serving, zone, model::SERVER_PORT, port)
        .spawn()
        .context("starting gcloud's IAP tunnel")?;
    let answer = ask(port, &key).await;
    let _ = tunnel.kill();
    let _ = tunnel.wait();
    if !keep {
        eprintln!("deleting the serving VM; the weights stay in the bucket");
        lifecycle::stop(&gcp, &loc, &plan).await?;
    }
    println!("{}", answer?);
    Ok(())
}

async fn teardown(state: &Path, project: &str, zone: &str) -> Result<()> {
    let gcloud = project_gcloud(installed().await?, state, project)?;
    let gcp = Gcp::new(Arc::new(TokenSource::gcloud(gcloud)), Endpoints::default());
    let loc = Location::new(project, zone)?;
    let plan = smoke_plan(state, &loc, None)?;
    lifecycle::teardown(&gcp, &loc, &plan, &|line: &str| eprintln!("{line}")).await
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
            let hosts = taste_gcp::hosts::HOST_FALLBACKS;
            for machine in std::iter::once(taste_gcp::hosts::HOST_MACHINE).chain(
                hosts
                    .iter()
                    .copied()
                    .filter(|m| *m != taste_gcp::hosts::HOST_MACHINE),
            ) {
                let short =
                    taste_gcp::quota::shortfalls(&gcp, project, model::DEFAULT_REGION, machine)
                        .await?;
                if short.is_empty() {
                    println!("{machine} fits the quotas in {}.", model::DEFAULT_REGION);
                } else {
                    println!("{}.", taste_gcp::quota::sentence(machine, &short));
                }
            }
        }
        ["smoke", state, project, zone] => smoke(Path::new(state), project, zone, false).await?,
        ["smoke", state, project, zone, "--keep"] => {
            smoke(Path::new(state), project, zone, true).await?
        }
        ["teardown", state, project, zone] => teardown(Path::new(state), project, zone).await?,
        _ => bail!(
            "usage: gcp-bringup install\n       \
             gcp-bringup sign-in|setup|check STATE_DIR PROJECT\n       \
             gcp-bringup smoke STATE_DIR PROJECT ZONE [--keep]\n       \
             gcp-bringup teardown STATE_DIR PROJECT ZONE"
        ),
    }
    Ok(())
}
