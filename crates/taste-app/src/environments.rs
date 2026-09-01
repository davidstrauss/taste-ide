//! Making an environment: the one path, and the names it uses.
//!
//! An environment is created in exactly two situations — a person pressing
//! New Environment in the panel, and an orchestrator calling
//! `chat_create` — and both come through here. They must agree about
//! naming (one vocabulary, so an environment a person made and one an agent
//! made are told apart by what they hold, never by how they are spelled)
//! and about the clone never running on the GTK thread.

use std::sync::Arc;

use gtk::glib;
use taste_core::environment::EnvironmentId;
use taste_devcontainer::EnvironmentRegistry;

/// Slug vocabulary for generated environment ids.
///
/// An environment id lands in container names, volume names, socket
/// filenames and a directory path, and the user reads it in the environment
/// panel and on a chat's own row. `env-3` would be all of correct, unique
/// and unmemorable; `brisk-3` is the same id a person can say out loud.
const ENVIRONMENT_ADJECTIVES: [&str; 12] = [
    "brisk", "calm", "clever", "eager", "keen", "lucid", "nimble", "plucky", "quiet", "spry",
    "steady", "wry",
];

/// A readable, unused environment id at this ordinal.
///
/// The walk exists because the clone directory — not any list the window
/// holds — is the inventory of record, and a name may be taken by an
/// environment restored from disk.
pub fn fresh_environment_id(
    ordinal: u32,
    taken: &[EnvironmentId],
) -> anyhow::Result<EnvironmentId> {
    let adjective = ENVIRONMENT_ADJECTIVES[ordinal as usize % ENVIRONMENT_ADJECTIVES.len()];
    let mut suffix = ordinal;
    // Bounded: a walk that cannot end is a hung click, and a saturating
    // suffix would otherwise retry the same taken name forever.
    for _ in 0..1000 {
        let candidate = format!("{adjective}-{suffix}");
        if !taken.iter().any(|id| id.as_str() == candidate) {
            return EnvironmentId::parse(candidate);
        }
        suffix = suffix.saturating_add(1);
    }
    anyhow::bail!("no free environment id near {adjective}-{ordinal}")
}

/// Clone the workspace into a new environment, off the main thread, and say
/// how it went.
///
/// The container is deliberately NOT started. Environments are lazy — clone
/// on creation, build on first need — and starting one runs its config's
/// lifecycle commands, which is the user's call through the existing reload
/// gates and never a side effect of creating one.
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

/// The next environment to make, given what exists.
pub fn next_id(registry: &EnvironmentRegistry) -> anyhow::Result<EnvironmentId> {
    let taken = registry.ids();
    fresh_environment_id(taken.len() as u32, &taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    /// Generated ids have to survive `EnvironmentId`'s validation, because
    /// they land verbatim in container names, volume names and a socket
    /// path — every ordinal, not just the small ones.
    #[test]
    fn generated_ids_are_readable_and_always_valid() {
        assert_eq!(fresh_environment_id(1, &[]).unwrap().as_str(), "calm-1");
        for ordinal in [0, 1, 7, 12, 13, 99, 100_000, u32::MAX] {
            let id = fresh_environment_id(ordinal, &[]).unwrap();
            assert!(id.as_str().len() <= taste_core::environment::MAX_ID_LEN);
            assert!(!id.is_primary());
            // Round-trips through the validator that container names use.
            assert_eq!(EnvironmentId::parse(id.as_str()).unwrap(), id);
        }
    }

    /// The disk is the inventory of record, so a name may already be taken
    /// by an environment restored from a clone. Walk rather than collide:
    /// `registry.create` would refuse, and the user would see a failure
    /// with nothing they could do about it.
    #[test]
    fn a_taken_name_is_walked_past() {
        let taken = vec![env("calm-1"), env("calm-2"), env("primary")];
        assert_eq!(
            fresh_environment_id(1, &taken).unwrap().as_str(),
            "calm-3",
            "the first free suffix, not the first candidate"
        );
        // A different ordinal's adjective is unaffected by another's
        // collisions.
        assert_eq!(
            fresh_environment_id(2, &taken).unwrap().as_str(),
            "clever-2"
        );
    }
}
