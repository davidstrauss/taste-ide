//! Bring a workspace's GCP identity up by hand, before the IDE's own
//! surfaces for it exist (ENVIRONMENTS → "A model on a cloud VM", Phase 1).
//!
//! ```text
//! gcp-identity enroll DIR WORKSPACE PROJECT
//! gcp-identity check  DIR WORKSPACE PROJECT PROJECT_NUMBER
//! ```
//!
//! `enroll` creates the three keys in this machine's TPM (once; an
//! enrolled DIR keeps its keys), issues the CA and both leaves, and prints
//! the setup to paste into Cloud Shell — `build-aux/gcp-setup.sh` with the
//! values filled in. `check` trades the Google leaf for a token and asks
//! the project which of the role's permissions the identity holds. It
//! creates nothing, so it costs nothing.
//!
//! DIR holds the keys' blobs (wrapped by the TPM) and the certificates,
//! all public or useless off this machine. The IDE will keep the same
//! layout in the workspace's state directory. `TASTE_TPM_TCTI` points it
//! at a software TPM instead of `/dev/tpmrm0`.
//!
//! The devcontainer has no TPM tools, so build it there and run it on the
//! host: `cargo build -p taste-gcp --example gcp-identity`, then
//! `target/debug/examples/gcp-identity …`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rustls::pki_types::CertificateDer;
use taste_gcp::identity::{issue_ca, issue_leaf, Issued, Leaf};
use taste_gcp::resources::Workspace;
use taste_gcp::rest::{Endpoints, Gcp, TokenSource};
use taste_gcp::setup::{self, Federation, PERMISSIONS};
use taste_gcp::sts;
use taste_gcp::tpm::{Tpm, TpmKey};

fn tpm() -> Tpm {
    match std::env::var("TASTE_TPM_TCTI") {
        Ok(tcti) => Tpm::unmanaged(&tcti),
        Err(_) => Tpm::device(),
    }
}

fn key(tpm: &Tpm, dir: &Path, name: &str) -> Result<TpmKey> {
    let dir = dir.join(name);
    if dir.join("key.pub").exists() {
        tpm.open_key(&dir)
    } else {
        eprintln!("creating the {name} key in the TPM");
        tpm.create_key(&dir)
    }
}

fn save(dir: &Path, name: &str, issued: &Issued) -> Result<()> {
    std::fs::write(dir.join(format!("{name}.pem")), issued.pem())?;
    let secs = issued.not_after.duration_since(UNIX_EPOCH)?.as_secs();
    std::fs::write(dir.join(format!("{name}.not-after")), secs.to_string())?;
    Ok(())
}

fn load(dir: &Path, name: &str) -> Result<Issued> {
    let pem = std::fs::read_to_string(dir.join(format!("{name}.pem")))
        .with_context(|| format!("no {name} certificate in {}; enroll first", dir.display()))?;
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD.decode(body)?;
    let secs: u64 = std::fs::read_to_string(dir.join(format!("{name}.not-after")))?
        .trim()
        .parse()?;
    Ok(Issued {
        der: CertificateDer::from(der),
        not_after: UNIX_EPOCH + Duration::from_secs(secs),
    })
}

fn enroll(dir: &Path, ws: &Workspace, project: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tpm = tpm();
    let ca = key(&tpm, dir, "ca")?;
    let now = SystemTime::now();
    let ca_cert = match load(dir, "ca") {
        Ok(existing) => existing,
        Err(_) => {
            let issued = issue_ca(ws, &ca, now)?;
            save(dir, "ca", &issued)?;
            issued
        }
    };
    for (name, leaf) in [("google", Leaf::Google), ("tunnel", Leaf::Tunnel)] {
        let holder = key(&tpm, dir, name)?;
        let current = load(dir, name).ok();
        if current.as_ref().is_none_or(|c| c.needs_renewal(now)) {
            eprintln!("issuing the {name} certificate");
            save(dir, name, &issue_leaf(ws, &ca, &holder, leaf, now)?)?;
        }
    }
    eprintln!(
        "Enrolled in {}. Paste what follows into Cloud Shell, signed in as yourself:\n",
        dir.display()
    );
    println!(
        "{}",
        setup::cloud_shell_script(ws, project, &ca_cert.pem())?
    );
    Ok(())
}

async fn check(dir: &Path, ws: &Workspace, project: &str, number: u64) -> Result<()> {
    let google = load(dir, "google")?;
    let holder: Arc<TpmKey> = Arc::new(tpm().open_key(&dir.join("google"))?);
    let config = sts::mutual_tls_config(sts::google_roots(), vec![google.der.clone()], holder)?;
    let tokens = TokenSource::federated(
        config,
        Federation::new(ws, number).audience(),
        google.der.clone(),
    );
    let gcp = Gcp::new(Arc::new(tokens), Endpoints::default());
    let held = gcp.test_permissions(project, PERMISSIONS).await?;
    let missing: Vec<_> = PERMISSIONS
        .iter()
        .filter(|p| !held.iter().any(|h| h == *p))
        .collect();
    if missing.is_empty() {
        println!(
            "The identity holds all {} permissions on {project}.",
            PERMISSIONS.len()
        );
        Ok(())
    } else {
        for permission in &missing {
            println!("missing: {permission}");
        }
        bail!(
            "{} of {} permissions missing; run the setup again",
            missing.len(),
            PERMISSIONS.len()
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: gcp-identity enroll DIR WORKSPACE PROJECT\n       \
                 gcp-identity check DIR WORKSPACE PROJECT PROJECT_NUMBER";
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["enroll", dir, workspace, project] => {
            enroll(&PathBuf::from(dir), &Workspace::new(workspace)?, project)
        }
        ["check", dir, workspace, project, number] => {
            check(
                &PathBuf::from(dir),
                &Workspace::new(workspace)?,
                project,
                number.parse().context("the project number is digits")?,
            )
            .await
        }
        _ => bail!("{usage}"),
    }
}
