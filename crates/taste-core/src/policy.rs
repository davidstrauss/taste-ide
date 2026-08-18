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
}
