//! Google Cloud, as the IDE uses it: one workspace's resources in the
//! user's own project, created as a keyless, least-privilege service
//! account the user's per-project gcloud sign-in impersonates.
//!
//! Two things are built on this crate, and it is shaped so that neither
//! owns it. The first is the GLM-5.3 route (ENVIRONMENTS → "A model on a
//! cloud VM"): a staging VM that mirrors pinned weights into the
//! project's bucket, and a serving VM with no way out that only the IDE
//! can reach, which pulls them into memory through a window the IDE opens
//! and closes. The second is the
//! substrate's cloud provisioner (ENVIRONMENTS → "Phase 3 — cloud
//! provisioners"), which places environments in GCP the way the local
//! pool places them under libvirt. So the layering is:
//!
//! - [`resources`]: what a workspace's networks, rules, disks, and
//!   instances look like as Compute API request bodies, built from typed
//!   specs and named and labelled by one convention. Generic: nothing in
//!   it knows about models or environments.
//! - [`model`]: a model's machines, composed from those builders — GLM-5.3,
//!   and the small model the route is proved with first — with the
//!   lockdown the design commits to stated as data so a test can read it
//!   back before anything is created.
//! - [`guest`]: what those machines are told to do, as Ignition configs.
//! - [`lifecycle`]: staging, serving, stopping, and tearing down.
//! - [`quota`]: whether the project's quotas let a machine be created,
//!   asked before it is, so a limit too low is named rather than met.
//! - [`setup`]: the one-time setup that grants the IDE its role, run as
//!   the user with the project's own gcloud sign-in.
//!
//! And how the IDE acts on them:
//!
//! - [`gcloud`]: the IDE's own pinned copy of the gcloud CLI, signed in
//!   per project — its tokens, and its IAP tunnel to the model's VM.
//! - [`project`]: what one project keeps — its choices file and its own
//!   gcloud configuration, in the workspace's IDE state.
//! - [`signed`]: V4 signed URLs, signed through IAM, which is how a VM
//!   with no credential of its own reads or writes one object.
//! - [`rest`]: Google's APIs, with tokens renewed before they lapse,
//!   failures read into a typed error, and Compute's operations awaited
//!   to their end.
//!
//! No GTK, and no IO in the builders: a plan is values, and creating it is
//! the caller's business.

pub mod gcloud;
pub mod guest;
pub mod lifecycle;
pub mod model;
pub mod project;
pub mod quota;
pub mod resources;
pub mod rest;
pub mod setup;
pub mod signed;

/// What the tests share.
#[cfg(test)]
pub(crate) mod testing {
    use std::io::Write;
    use std::path::Path;

    /// Write an executable stub at `path` from a short-lived `sh`, so this
    /// process never holds a file open for writing that it will later run.
    /// Written here, a stub could meet "Text file busy" at its first run:
    /// another test thread forking while the file was open hands the child
    /// that descriptor until the child execs, and the kernel refuses to run
    /// a file anyone has open for writing.
    pub fn install_stub(path: &Path, script: &str) {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "installing {}",
            path.display()
        );
    }
}
