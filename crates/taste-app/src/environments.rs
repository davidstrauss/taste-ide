//! Making an environment: the one path, and the names it uses.
//!
//! An environment is created in exactly two situations — a person pressing
//! Start on an issue in the backlog, and an orchestrator calling
//! `issue_start` — and both come through here. An environment is an issue
//! in progress (docs/spikes/issue-is-the-environment.md), so it takes the
//! issue's id and nothing is generated; what the two paths must agree on
//! is that the clone never runs on the GTK thread.

use std::sync::Arc;

use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_devcontainer::EnvironmentRegistry;

pub fn create(
    registry: Arc<EnvironmentRegistry>,
    id: EnvironmentId,
    then: Box<dyn FnOnce(Result<EnvironmentId, String>)>,
) {
    glib::spawn_future_local(async move {
        let for_worker = id.clone();
        // Never on the GTK thread: this is a git clone.
        let handle = crate::runtime::runtime()
            .spawn_blocking(move || registry.create(for_worker).map(|_| ()));
        match handle.await {
            Ok(Ok(())) => then(Ok(id)),
            Ok(Err(e)) => then(Err(format!("{e:#}"))),
            Err(e) => then(Err(format!("the clone task did not finish: {e}"))),
        }
    });
}

/// The environment an issue gets when it is started: the issue's own id,
/// which is a valid environment id by construction (`i-0007`).
pub fn for_issue(issue_id: &str) -> anyhow::Result<EnvironmentId> {
    EnvironmentId::parse(issue_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issue_id_is_an_environment_id() {
        let env = for_issue("i-0007").unwrap();
        assert_eq!(env.as_str(), "i-0007");
        assert!(!env.is_primary());
    }
}
