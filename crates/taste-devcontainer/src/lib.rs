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

pub mod baseline;
pub mod channel;
pub mod config;
pub mod configwatch;
pub mod console;
pub mod guest;
pub mod hash;
pub mod image;
pub mod keeper;
pub mod keys;
pub mod migration;
pub mod peer;
pub mod pool;
pub mod ports;
pub mod provision;
pub mod reconcile;
pub mod registry;
pub mod security;
pub mod sizing;
pub mod substrate;
pub mod supervisor;
pub mod worktree;

pub use channel::{ChannelPaths, ChannelServices, ChannelStream, EnvChannel, Service};
pub use config::DevcontainerConfig;
pub use hash::{build_hash, config_hash};
pub use keeper::Keeper;
pub use pool::{Pool, PoolError};
pub use ports::PortTraffic;
pub use provision::{DomainState, LibvirtSession, Vm, VmFacts};
pub use reconcile::SweepReport;
pub use registry::VmUsage;
pub use registry::{DestroyReport, DiskBudget, EnvironmentRegistry, FreeDisk, ReconcileReport};
pub use substrate::{Provider, Substrate};
pub use supervisor::{
    AgentHosting, CheckoutWalk, DiskSample, DiskUsage, EnvironmentIdentity, ResolvedConfig,
    ResourceInfo, ResourceKind, Supervisor, SupervisorState,
};
pub use worktree::Worktree;
