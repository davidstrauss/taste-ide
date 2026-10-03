//! Bring one of a workspace's cloud hosts up by hand, and take it down
//! (`taste_devcontainer::cloud`).
//!
//! ```text
//! cloud-host up   WORKSPACE_ROOT
//! cloud-host stop WORKSPACE_ROOT NAME
//! cloud-host down WORKSPACE_ROOT NAME
//! ```
//!
//! `up` makes a host in the project the workspace's title-bar cloud is
//! set up with, waits for ssh through IAP and podman behind it, and runs
//! `podman info` there — the whole path an environment's start takes to a
//! cloud host, minus the environment. `stop` stops it (its disk stays);
//! `down` deletes it. A host is an `n4-standard-8`, about $0.40 an hour
//! while it runs.
//!
//! Build it in the devcontainer and run it on the host, where the
//! workspace's sign-in is: `cargo build -p taste-devcontainer --example
//! cloud-host`, then `target/debug/examples/cloud-host …`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use taste_devcontainer::cloud::CloudSession;

fn find(session: &CloudSession, name: &str) -> Result<taste_devcontainer::provision::Vm> {
    session
        .list()
        .into_iter()
        .find(|vm| vm.domain == name)
        .with_context(|| format!("{name} is not one of this workspace's cloud hosts"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let say = |line: &str| eprintln!("{line}");
    match args.as_slice() {
        ["up", root] => {
            let session = CloudSession::new(Path::new(root));
            let started = std::time::Instant::now();
            let vm = session.create(&say).await?;
            let facts = session.ensure_running(&vm, &say).await?;
            eprintln!(
                "{} ready in {:.0}s: {} vCPU, {} MiB",
                vm.domain,
                started.elapsed().as_secs_f64(),
                facts.cpus,
                facts.memory_mib
            );
            let target =
                taste_core::PodmanTarget::connection(&vm.domain, taste_core::podman::sandboxed());
            let (program, args) = target.argv([
                "info",
                "--format",
                "{{.Host.Hostname}} {{.Host.Kernel}} {{.Version.Version}}",
            ]);
            let output = tokio::process::Command::new(program)
                .args(args)
                .output()
                .await?;
            if !output.status.success() {
                bail!(
                    "podman info failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            println!("{}", String::from_utf8_lossy(&output.stdout).trim());
            println!("{}", vm.domain);
        }
        ["stop", root, name] => {
            let session = CloudSession::new(Path::new(root));
            session.stop(&find(&session, name)?).await?;
        }
        ["down", root, name] => {
            let session = CloudSession::new(Path::new(root));
            session.destroy(&find(&session, name)?).await?;
        }
        _ => bail!(
            "usage: cloud-host up WORKSPACE_ROOT\n       cloud-host stop|down WORKSPACE_ROOT NAME"
        ),
    }
    Ok(())
}
