//! The IDE's confinement policy.
//!
//! taste-ide has exactly two modes, both derived from whether the
//! devcontainer is running (`ExecContext::is_container()`):
//!
//! - **Container mode**: the only real working mode. Work happens in the
//!   devcontainer; the workspace is writable.
//! - **Safe mode**: the total fallback when the devcontainer is absent or
//!   won't start. Its sole purpose is defining, debugging, and entering the
//!   rootless devcontainer setup — so writes (by the user *and* the AI) are
//!   confined to the devcontainer configuration.
//!
//! In both modes — i.e. all the time — the AI has no general home-directory
//! access, and its remote git operations are read-only (fetch/pull yes,
//! push no). Anything needing broader host access is built *inside* the
//! devcontainer and deployed by the user. Enforcement lives in
//! `taste-acp::sandbox` (bubblewrap) and the editor/file-tree write checks;
//! this module is the shared policy definition they all consult.

use std::path::{Component, Path, PathBuf};

/// Where the agent's own home volume mounts when the agent runs inside a
/// project devcontainer. Not `/home/dev`: that is the USER home in there.
///
/// The volume's *name* is not here and is not machine-global: it is
/// [`crate::environment::env_home_volume`], one per environment. A single
/// shared `taste-agent-home` was already wrong across workspaces; across N
/// environments of one workspace it would mean N agents writing one home.
pub const AGENT_HOME_IN_DEVCONTAINER: &str = "/home/agent";

/// The paths that define the devcontainer setup.
pub fn devcontainer_scope(workspace_root: &Path) -> [PathBuf; 2] {
    [
        workspace_root.join(".devcontainer"),
        workspace_root.join(".devcontainer.json"),
    ]
}

/// The full safe-mode writable set: the devcontainer setup plus the
/// workspace-ergonomics dotfiles. Configuring the container is *work*, and
/// work deserves its comforts — editorconfig, ignore rules — without
/// unlocking project source.
pub fn safe_mode_scope(workspace_root: &Path) -> Vec<PathBuf> {
    let mut scope: Vec<PathBuf> = devcontainer_scope(workspace_root).into();
    for name in [".editorconfig", ".gitignore", ".gitattributes"] {
        scope.push(workspace_root.join(name));
    }
    scope
}

/// The files that configure the AGENT rather than the project: its
/// instructions, its settings, its skills.
///
/// These are the one exception to "the agent gets no workspace". An agent
/// loads them from its working directory at startup, before any ACP call
/// exists to fetch them through — so without them the agent arrives
/// knowing none of the project's conventions, which is precisely the
/// failure this project's own CLAUDE.md exists to prevent. They are bound
/// **read-only**, so the no-workspace property survives: the agent reads
/// its own instructions and still cannot reach project source except
/// through the IDE.
///
/// Read-only also means an agent cannot rewrite its own instructions or
/// silently add itself permissions mid-session — a property worth having
/// on purpose, though it does mean settings an agent would normally
/// persist here (e.g. a local settings file) will not stick.
///
/// The list is fixed, covering the cross-agent `AGENTS.md` convention plus
/// the per-agent locations for the agents in `taste-acp`'s registry.
/// Convention over configuration: new agents add their location here.
pub fn agent_context_scope(workspace_root: &Path) -> Vec<PathBuf> {
    [
        "AGENTS.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        ".claude",
        "GEMINI.md",
        ".gemini",
        ".github/copilot-instructions.md",
    ]
    .iter()
    .map(|name| workspace_root.join(name))
    .collect()
}

/// Lexically normalize a path (resolve `.`/`..` without touching the fs),
/// rejecting anything that escapes upward past its start.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Resolve `path` against the real filesystem as far as it exists: the
/// deepest existing ancestor is canonicalized (following symlinks), then the
/// not-yet-existing remainder is re-appended. This is what defeats
/// `workspace/evil -> /home/user` symlinks that pure lexical checks miss —
/// the repo is untrusted and can commit symlinks.
fn resolve_existing(path: &Path) -> Option<PathBuf> {
    let path = normalize(path)?;
    let mut existing = path.as_path();
    let mut remainder = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        remainder.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    }
    let mut resolved = existing.canonicalize().ok()?;
    for part in remainder.iter().rev() {
        resolved.push(part);
    }
    Some(resolved)
}

/// Whether writing `path` is allowed under the current mode.
///
/// Always requires the path to be inside the workspace — after resolving
/// symlinks, since the repo itself is untrusted. In safe mode it must
/// additionally be inside the devcontainer scope.
pub fn write_allowed(workspace_root: &Path, safe_mode: bool, path: &Path) -> bool {
    let Some(root) = normalize(workspace_root) else {
        return false;
    };
    // Compare in resolved space when the root itself resolves (it exists in
    // real use; pure-lexical tests fall back to normalized comparison).
    let (path, root) = match (resolve_existing(path), resolve_existing(&root)) {
        (Some(p), Some(r)) => (p, r),
        _ => match normalize(path) {
            Some(p) => (p, root),
            None => return false,
        },
    };
    if !path.starts_with(&root) {
        return false;
    }
    // Never allow touching the git object store directly, in either mode.
    if path.starts_with(root.join(".git")) {
        return false;
    }
    if safe_mode {
        safe_mode_scope(&root)
            .iter()
            .any(|scope| path.starts_with(scope))
    } else {
        true
    }
}

/// Whether `path` lives in an environment's checkout — the predicate that
/// makes watching read-only.
///
/// A non-primary environment's checkout is a clone under
/// `$XDG_STATE_HOME/taste-ide/environments/`, so [`write_allowed`] already
/// refuses every path in it: it is not inside the workspace. This does not
/// re-decide that. It answers the *other* question — **which** environment
/// a file belongs to — so the refusal can name it ("read-only: calm-1's
/// checkout") instead of saying "outside the workspace", and so an editor
/// tab opened from a watched environment stays read-only after the user has
/// returned to their own checkout.
///
/// Symlink-resolving for the same reason `write_allowed` is: an agent's
/// clone is as untrusted as the repository it came from, and a link out of
/// it must not read as a file inside it.
pub fn in_environment_checkout(env_root: &Path, path: &Path) -> bool {
    let (path, root) = match (resolve_existing(path), resolve_existing(env_root)) {
        (Some(p), Some(r)) => (p, r),
        _ => match (normalize(path), normalize(env_root)) {
            (Some(p), Some(r)) => (p, r),
            _ => return false,
        },
    };
    path.starts_with(&root)
}

/// A directory git will find no hooks in. Agent git runs with
/// `core.hooksPath` pointed here, so an untrusted repo cannot hijack an
/// agent's `git commit` with a hook of its own. Git treats a hooksPath
/// that does not exist as "no hooks", which is exactly the intent.
pub const AGENT_HOOKS_PATH: &str = "/nonexistent/taste-ide-no-hooks";

/// The git configuration every agent-run git inherits, as key/value pairs.
///
/// One definition, two renderings: `taste-acp::sandbox` writes it to the
/// `GIT_CONFIG_GLOBAL` file a sandboxed agent gets, and
/// [`crate::ExecContext::resolve_for_agent`] passes it as `GIT_CONFIG_*`
/// environment on agent-brokered commands. They must not drift, so neither
/// one spells the policy out itself.
///
/// `pushInsteadOf` rewrites push URLs only, leaving fetch untouched; the
/// schemes below cover https, ssh, git and scp-style remotes. Note this is
/// defense-in-depth and UX (a clear error instead of an auth prompt), not
/// the enforcement — an agent controls its own environment once it is
/// running. What actually makes push impossible is the absence of
/// credentials: no ssh keys and no credential helper are reachable from
/// either the agent's sandbox or the devcontainer, and
/// `taste-devcontainer::security` refuses any repo config that would mount
/// some in.
pub fn agent_git_config() -> Vec<(String, String)> {
    let mut config: Vec<(String, String)> = ["https://", "ssh://", "git://", "git@"]
        .iter()
        .enumerate()
        .map(|(index, scheme)| {
            (
                format!("url.push-blocked-{index}://.pushInsteadOf"),
                (*scheme).to_string(),
            )
        })
        .collect();
    config.push(("core.hooksPath".into(), AGENT_HOOKS_PATH.into()));
    config
}

/// [`agent_git_config`] rendered as `GIT_CONFIG_COUNT`/`_KEY_n`/`_VALUE_n`
/// environment variables — git's own way to carry config in an environment.
///
/// The third rendering of one policy, and the reason it lives here rather
/// than at each call site: `ExecContext::resolve_for_agent` passes it on
/// every agent-brokered command, and the relocated agent spawn passes it to
/// the agent's *own* git inside its environment's container. (The file
/// rendering, `taste-acp::sandbox`, is the outside-confined topology's.)
///
/// Additive rather than replacing, unlike `GIT_CONFIG_GLOBAL`: the
/// container's global config already carries the identity the supervisor
/// inherited from the host at start, and this must not wipe it out — an
/// agent whose commits die with "Author identity unknown" cannot publish.
pub fn agent_git_config_env() -> Vec<(String, String)> {
    let config = agent_git_config();
    let mut env: Vec<(String, String)> =
        vec![("GIT_CONFIG_COUNT".into(), config.len().to_string())];
    for (index, (key, value)) in config.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/project";

    #[test]
    fn container_mode_allows_workspace_writes_only() {
        let root = Path::new(ROOT);
        assert!(write_allowed(root, false, &root.join("src/main.rs")));
        assert!(!write_allowed(root, false, Path::new("/home/user/.bashrc")));
        assert!(!write_allowed(root, false, &root.join(".git/config")));
    }

    #[test]
    fn safe_mode_confines_writes_to_devcontainer_scope() {
        let root = Path::new(ROOT);
        assert!(write_allowed(
            root,
            true,
            &root.join(".devcontainer/devcontainer.json")
        ));
        assert!(write_allowed(
            root,
            true,
            &root.join(".devcontainer/Containerfile")
        ));
        assert!(write_allowed(root, true, &root.join(".devcontainer.json")));
        assert!(!write_allowed(root, true, &root.join("src/main.rs")));
        assert!(!write_allowed(root, true, &root.join("Cargo.toml")));
    }

    #[test]
    fn safe_mode_allows_workspace_ergonomics_dotfiles() {
        let root = Path::new(ROOT);
        assert!(write_allowed(root, true, &root.join(".editorconfig")));
        assert!(write_allowed(root, true, &root.join(".gitignore")));
        assert!(write_allowed(root, true, &root.join(".gitattributes")));
        // But not arbitrary dotfiles.
        assert!(!write_allowed(root, true, &root.join(".env")));
        // And not similarly-named files elsewhere... the scope is exact.
        assert!(!write_allowed(root, true, &root.join("src/.editorconfig")));
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_cannot_smuggle_writes_outside_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("evil")).unwrap();

        // Lexically inside; resolves outside → denied in both modes.
        let target = ws.path().join("evil/steal.txt");
        assert!(!write_allowed(ws.path(), false, &target));
        assert!(!write_allowed(ws.path(), true, &target));
        // Honest paths still work, including not-yet-existing files.
        assert!(write_allowed(
            ws.path(),
            false,
            &ws.path().join("new/file.rs")
        ));
    }

    #[test]
    fn traversal_cannot_escape_the_workspace() {
        let root = Path::new(ROOT);
        assert!(!write_allowed(
            root,
            false,
            &root.join("../outside/file.txt")
        ));
        assert!(!write_allowed(
            root,
            true,
            &root.join(".devcontainer/../../etc/passwd")
        ));
        // `..` that stays inside is fine.
        assert!(write_allowed(
            root,
            true,
            &root.join(".devcontainer/sub/../devcontainer.json")
        ));
    }

    /// Agent context is readable, never writable: the scope exists so an
    /// agent arrives knowing the project's conventions, not so it can
    /// rewrite them. `write_allowed` must not be softened for it.
    #[test]
    fn agent_context_is_not_writable_in_either_mode() {
        let root = Path::new(ROOT);
        for path in agent_context_scope(root) {
            assert!(
                !write_allowed(root, true, &path),
                "{} writable in safe mode",
                path.display()
            );
        }
        // In container mode the whole workspace is writable by policy —
        // the read-only-ness of these comes from the mount, and that is
        // the mount's job, not this function's.
        assert!(write_allowed(root, false, &root.join("CLAUDE.md")));
    }

    /// Watching is looking, never editing. An environment's clone lives
    /// outside the workspace, so the ordinary write check refuses it in
    /// both modes — and `in_environment_checkout` says whose it is, which
    /// is what lets the refusal name the environment.
    #[test]
    fn another_environments_checkout_is_read_only_and_named() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let clone = state.path().join("environments/ws/calm-1/repo");
        std::fs::create_dir_all(clone.join("src")).unwrap();
        let file = clone.join("src/main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        // The user's own write path refuses it, in either mode, with no
        // new rule: it is not in the workspace.
        assert!(!write_allowed(workspace.path(), false, &file));
        assert!(!write_allowed(workspace.path(), true, &file));

        // And it is attributable: this file belongs to that environment,
        // even once the panes are aimed back at the primary.
        assert!(in_environment_checkout(&clone, &file));
        assert!(in_environment_checkout(&clone, &clone.join("new/file.rs")));
        // A sibling environment's clone is not this one's.
        let sibling = state.path().join("environments/ws/spry-2/repo");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!in_environment_checkout(&sibling, &file));
        // Nor is the user's checkout.
        assert!(!in_environment_checkout(
            &clone,
            &workspace.path().join("a.rs")
        ));
    }

    /// A symlink planted in an agent's clone must not launder a path into
    /// looking like the clone's own — the clone is as untrusted as the
    /// repository it was made from.
    #[test]
    #[cfg(unix)]
    fn a_link_out_of_a_clone_does_not_belong_to_it() {
        let outside = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), clone.path().join("escape")).unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        assert!(!in_environment_checkout(
            clone.path(),
            &clone.path().join("escape/secret")
        ));
    }

    #[test]
    fn agent_git_config_blocks_every_push_scheme_and_masks_hooks() {
        let config = agent_git_config();
        let values: Vec<&str> = config.iter().map(|(_, v)| v.as_str()).collect();
        for scheme in ["https://", "ssh://", "git://", "git@"] {
            assert!(values.contains(&scheme), "{scheme} not blocked: {config:?}");
        }
        // pushInsteadOf only — fetch and pull must keep working.
        assert!(config
            .iter()
            .all(|(k, _)| k.ends_with("pushInsteadOf") || k == "core.hooksPath"));
        assert!(config
            .iter()
            .any(|(k, v)| k == "core.hooksPath" && v == AGENT_HOOKS_PATH));
        // Distinct subsections, or git keeps only the last of them.
        let keys: std::collections::HashSet<&String> = config.iter().map(|(k, _)| k).collect();
        assert_eq!(keys.len(), config.len(), "duplicate keys: {config:?}");
    }

    /// The environment rendering is the same policy: a scheme blocked in
    /// one must be blocked in the other, and the count must match or git
    /// silently ignores the entries past it.
    #[test]
    fn the_environment_rendering_covers_every_entry_of_the_policy() {
        let config = agent_git_config();
        let env = agent_git_config_env();
        let lookup = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(lookup("GIT_CONFIG_COUNT"), config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            assert_eq!(&lookup(&format!("GIT_CONFIG_KEY_{index}")), key);
            assert_eq!(&lookup(&format!("GIT_CONFIG_VALUE_{index}")), value);
        }
        // Additive, not replacing: nothing here may clear the container's
        // inherited git identity out from under the agent's commits.
        assert!(!env.iter().any(|(k, _)| k == "GIT_CONFIG_GLOBAL"));
    }
}
