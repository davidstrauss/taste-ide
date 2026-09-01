//! Devcontainer supervision over rootless Podman, one container per
//! **environment**.
//!
//! A devcontainer is a *supervised resource*: the IDE builds it, starts it,
//! watches its config for drift, and reconnects to it — all without the IDE
//! itself reloading, and without touching host-side agent sessions.
//!
//! A workspace supervises N of them. [`EnvironmentRegistry`] owns one
//! [`Supervisor`] per environment; the primary environment is the main
//! checkout, and every other environment is a git clone of it under
//! `$XDG_STATE_HOME`. See `docs/ENVIRONMENTS.md` for the design of record.

pub mod config;
pub mod hash;
pub mod reconcile;
pub mod registry;
pub mod security;
pub mod services;
pub mod supervisor;

pub use config::DevcontainerConfig;
pub use hash::{build_hash, config_hash};
pub use reconcile::SweepReport;
pub use registry::{DestroyReport, EnvironmentRegistry, ReconcileReport};
pub use supervisor::{
    EnvironmentIdentity, ResourceInfo, ResourceKind, Supervisor, SupervisorState,
};
