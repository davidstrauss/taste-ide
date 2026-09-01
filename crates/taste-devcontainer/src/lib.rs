//! Devcontainer supervision over rootless Podman.
//!
//! The devcontainer is a *supervised resource*: the IDE builds it, starts
//! it, watches its config for drift, and reconnects to it — all without the
//! IDE itself reloading, and without touching host-side agent sessions.

pub mod config;
pub mod hash;
pub mod security;
pub mod services;
pub mod supervisor;

pub use config::DevcontainerConfig;
pub use hash::{build_hash, config_hash};
pub use supervisor::{
    EnvironmentIdentity, ResourceInfo, ResourceKind, Supervisor, SupervisorState,
};
