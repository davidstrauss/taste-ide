//! Google Cloud, as the IDE uses it: one workspace's resources in the
//! user's own project, created under an identity that lives in this
//! machine's TPM.
//!
//! Two things are built on this crate, and it is shaped so that neither
//! owns it. The first is the GLM-5.3 route (ENVIRONMENTS → "A model on a
//! cloud VM"): a staging VM that fetches pinned weights, and a serving VM
//! with no way out that only the IDE can reach. The second is the
//! substrate's cloud provisioner (ENVIRONMENTS → "Phase 3 — cloud
//! provisioners"), which places environments in GCP the way the local
//! pool places them under libvirt. So the layering is:
//!
//! - [`resources`]: what a workspace's networks, rules, disks, and
//!   instances look like as Compute API request bodies, built from typed
//!   specs and named and labelled by one convention. Generic: nothing in
//!   it knows about models or environments.
//! - [`model`]: the GLM-5.3 machines, composed from those builders, with
//!   the lockdown the design commits to stated as data so a test can read
//!   it back before anything is created.
//! - [`setup`]: the one-time Cloud Shell commands the user runs, which
//!   are how the IDE gets permission without ever holding the user's own
//!   credentials.
//!
//! No GTK, and no IO in the builders: a plan is values, and creating it is
//! the caller's business.

pub mod model;
pub mod resources;
pub mod setup;
