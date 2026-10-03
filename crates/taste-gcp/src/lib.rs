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
//! And the identity those permissions are granted to:
//!
//! - [`signer`]: a P-256 key that signs where it lives — in the product,
//!   the TPM — and is never anywhere else, with the adapters that let
//!   certificate building and TLS use it.
//! - [`identity`]: the workspace's CA and its two leaves, one for Google
//!   and one for the model's VM.
//! - [`sts`]: trading the Google leaf for an access token over mutual TLS
//!   (Workload Identity Federation with X.509 certificates).
//! - [`rest`]: Google's APIs as that identity — tokens renewed before they
//!   lapse, failures read into a typed error, and Compute's operations
//!   awaited to their end.
//!
//! No GTK, and no IO in the builders: a plan is values, and creating it is
//! the caller's business.

pub mod identity;
pub mod model;
pub mod resources;
pub mod rest;
pub mod setup;
pub mod signer;
pub mod sts;
