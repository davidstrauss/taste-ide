//! What a connection to a forwarded port does before it goes through: the
//! app's hook, given the environment and the container's port, brings the
//! port's service up when nothing is serving it, and returns once the
//! server listens or it gives up (David, 2026-10-24: "notice that there's
//! an attempt to access a port we're configured to forward and auto-start
//! the service if it's linked to the port").
//!
//! The supervisor relays every forwarded port of a container in a VM
//! (`Supervisor::sync_tunnel`), so it sees each connection; what a service
//! is and how one is started — the project's Taskfile, the task board, the
//! environment's `task` wrapper — are the app's, so the deciding is too.
//! One hook for every environment, set once at startup; none set, a
//! connection goes straight through, as it did before there was one.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use taste_core::environment::EnvironmentId;

/// Bring up what serves `port` (the container's) in `env`, if anything
/// should; resolves when the connection may go through.
pub type Activation =
    Arc<dyn Fn(EnvironmentId, u16) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

static HOOK: OnceLock<Activation> = OnceLock::new();

/// Set the hook. Once: a second call is ignored.
pub fn set(hook: Activation) {
    let _ = HOOK.set(hook);
}

/// Run the hook for a connection to `port` in `env`, if one is set.
pub async fn activate(env: EnvironmentId, port: u16) {
    if let Some(hook) = HOOK.get() {
        hook(env, port).await;
    }
}
