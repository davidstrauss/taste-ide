//! Google Cloud, as the IDE uses it: one workspace's resources in the
//! user's own project, created as a keyless, least-privilege service
//! account the user's per-project gcloud sign-in impersonates.
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
//! - [`setup`]: the one-time setup that grants the IDE its role, run as
//!   the user with the project's own gcloud sign-in.
//!
//! And how the IDE acts on them:
//!
//! - [`gcloud`]: the IDE's own pinned copy of the gcloud CLI, signed in
//!   per project — its tokens, and its IAP tunnel to the model's VM.
//! - [`rest`]: Google's APIs, with tokens renewed before they lapse,
//!   failures read into a typed error, and Compute's operations awaited
//!   to their end.
//!
//! No GTK, and no IO in the builders: a plan is values, and creating it is
//! the caller's business.

pub mod gcloud;
pub mod model;
pub mod resources;
pub mod rest;
pub mod setup;
