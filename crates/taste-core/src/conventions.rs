//! The conventional file locations (ARCHITECTURE → Conventions), as data.
//!
//! Single source of truth for two consumers: the file tree's ghost rows
//! (a missing convention shows faintly, one activation from existing) and
//! the MCP `ide_conventions` tool (agents bootstrapping a project should
//! reach for these fixed places instead of inventing configuration).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Convention {
    /// Absolute path where the file belongs in this workspace.
    pub path: PathBuf,
    /// What the file does, phrased for an agent deciding whether to
    /// create it.
    pub purpose: &'static str,
    pub exists: bool,
    /// Whether the file tree offers a ghost row when missing. `.taste.yaml`
    /// does not: it is reserved, and nothing needs it yet.
    pub ghost: bool,
}

/// Every conventional location, present or not.
pub fn conventions(root: &Path) -> Vec<Convention> {
    let has_devcontainer =
        root.join(".devcontainer").exists() || root.join(".devcontainer.json").exists();
    let mut list = vec![Convention {
        path: root.join(".devcontainer/devcontainer.json"),
        purpose: "devcontainer definition; the IDE builds and attaches to it \
                  (validated: no privileged flags, mounts stay in the workspace)",
        exists: has_devcontainer,
        ghost: true,
    }];
    for (name, purpose) in [
        (
            ".editorconfig",
            "editor behavior: indentation, charset, final newline",
        ),
        (".gitignore", "tree filtering and ignore rules"),
        (".gitattributes", "git text/eol and diff attributes"),
    ] {
        let path = root.join(name);
        list.push(Convention {
            exists: path.exists(),
            path,
            purpose,
            ghost: true,
        });
    }
    list.push(Convention {
        exists: root.join(".taste.yaml").exists(),
        path: root.join(".taste.yaml"),
        purpose: "reserved for repo-level IDE configuration; currently \
                  nothing needs it — prefer the conventions above over \
                  adding configuration",
        ghost: false,
    });
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_files_are_marked_and_ghosts_selectable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".editorconfig"), "root = true\n").unwrap();
        let list = conventions(dir.path());
        let editorconfig = list
            .iter()
            .find(|c| c.path.ends_with(".editorconfig"))
            .unwrap();
        assert!(editorconfig.exists);
        let ghosts: Vec<_> = list.iter().filter(|c| !c.exists && c.ghost).collect();
        assert!(ghosts.iter().any(|c| c.path.ends_with("devcontainer.json")));
        assert!(!ghosts.iter().any(|c| c.path.ends_with(".taste.yaml")));
    }
}
