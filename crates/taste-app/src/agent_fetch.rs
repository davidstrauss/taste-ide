//! **An agent's fetch, done by the IDE.**
//!
//! An agent's checkout cannot reach a remote: the user's keys never enter a
//! VM. So `git_fetch` (`taste_mcp`) asks the IDE, which fetches in the
//! user's folder on this host, as the file tree's Fetch does, and gives the
//! checkouts what moved — Personal's as its remote-tracking refs, every
//! other environment's as `upstream/*` (`share`, the window's).
//!
//! A fetch whose key would ask the user for a touch, a PIN, or a passphrase
//! runs only when the agent said it may (`ask_user`), and then through the
//! same strip and agent as the file tree's Pull (`ssh_agent::network_env`).
//! Otherwise it is held back before it starts, as the background fetch is
//! (`taste_git::presence`), and runs with nothing that can prompt (David,
//! 2026-10-19: "it's allowed, but it shouldn't be accidental").

use std::path::PathBuf;
use std::sync::Arc;

use taste_core::{Event, EventBus};
use taste_mcp::{GitFetchOutcome, GitFetcher};

/// The fetcher the MCP server's `git_fetch` calls, for the folder at
/// `root`.
pub fn fetcher(
    root: PathBuf,
    share: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    events: EventBus,
) -> GitFetcher {
    Arc::new(move |ask_user| {
        let (root, share, events) = (root.clone(), share.clone(), events.clone());
        Box::pin(fetch(root, ask_user, share, events))
    })
}

async fn fetch(
    root: PathBuf,
    ask_user: bool,
    share: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    events: EventBus,
) -> GitFetchOutcome {
    let prepared = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            let git = taste_git::GitWorkspace::discover(&root)?;
            let remote = git.upstream_remote_url();
            let held = (!ask_user)
                .then(|| {
                    remote
                        .as_deref()
                        .and_then(taste_git::presence::fetch_needs_presence)
                })
                .flatten();
            Some((
                git.fetch_all_command(),
                remote,
                held,
                git.refs_under("refs/remotes/").unwrap_or_default(),
            ))
        })
        .await
        .ok()
        .flatten()
    };
    let Some(((program, args), remote, held, before)) = prepared else {
        return GitFetchOutcome {
            error: Some("the user's folder is not a git repository".to_string()),
            ..GitFetchOutcome::default()
        };
    };
    if let Some(reason) = held {
        return GitFetchOutcome {
            needs_user: Some(reason),
            ..GitFetchOutcome::default()
        };
    }
    let envs = if ask_user {
        crate::ssh_agent::network_env(root.clone(), remote).await
    } else {
        taste_git::non_interactive_env()
    };
    if let Err(reason) = crate::filetree::run_git_step(program, args, envs.clone()).await {
        return GitFetchOutcome {
            error: Some(reason),
            ..GitFetchOutcome::default()
        };
    }
    // The submodules, with the same keys, as the file tree's Fetch takes
    // them: one that fails is the agent's to hear about, and the rest go on.
    let commands = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || crate::filetree::submodule_fetch_commands(&root))
            .await
            .unwrap_or_default()
    };
    let mut failed = Vec::new();
    for (name, (program, args)) in commands {
        if let Err(reason) = crate::filetree::run_git_step(program, args, envs.clone()).await {
            failed.push(format!("the submodule {name}: {reason}"));
        }
    }
    let after = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            taste_git::GitWorkspace::discover(&root)
                .and_then(|git| git.refs_under("refs/remotes/").ok())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    };
    let moved = moved_refs(&before, &after);
    if !moved.is_empty() {
        if let Ok(Err(reason)) = tokio::task::spawn_blocking(move || share()).await {
            failed.push(format!("giving the checkouts what moved: {reason}"));
        }
        events.publish(Event::GitStatusChanged);
    }
    GitFetchOutcome {
        fetched: true,
        moved,
        needs_user: None,
        error: (!failed.is_empty()).then(|| failed.join("; ")),
    }
}

/// The refs that differ between two listings, by their short name:
/// moved, created, or removed.
fn moved_refs(
    before: &[(String, taste_git::Oid)],
    after: &[(String, taste_git::Oid)],
) -> Vec<String> {
    let old: std::collections::HashMap<&str, taste_git::Oid> =
        before.iter().map(|(n, o)| (n.as_str(), *o)).collect();
    let new: std::collections::HashMap<&str, taste_git::Oid> =
        after.iter().map(|(n, o)| (n.as_str(), *o)).collect();
    let mut names: Vec<String> = old
        .keys()
        .chain(new.keys())
        .filter(|name| old.get(*name) != new.get(*name))
        .map(|name| name.trim_start_matches("refs/remotes/").to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_moved_is_what_differs() {
        let a = taste_git::Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        let b = taste_git::Oid::from_str("2222222222222222222222222222222222222222").unwrap();
        let before = vec![
            ("refs/remotes/origin/main".to_string(), a),
            ("refs/remotes/origin/gone".to_string(), a),
            ("refs/remotes/origin/same".to_string(), a),
        ];
        let after = vec![
            ("refs/remotes/origin/main".to_string(), b),
            ("refs/remotes/origin/new".to_string(), b),
            ("refs/remotes/origin/same".to_string(), a),
        ];
        assert_eq!(
            moved_refs(&before, &after),
            ["origin/gone", "origin/main", "origin/new"]
        );
    }
}
