//! The conventional file locations (ARCHITECTURE → Conventions), as data.
//!
//! Single source of truth for two consumers: the file tree's ghost rows
//! (a missing convention shows faintly, one activation from existing) and
//! the MCP `ide_conventions` tool (agents bootstrapping a project should
//! reach for these fixed places instead of inventing configuration).

use std::path::{Path, PathBuf};

/// The names `task` (taskfile.dev) looks for a Taskfile under, in its own
/// order; the first is the canonical one a new Taskfile gets.
pub const TASKFILE_NAMES: [&str; 8] = [
    "Taskfile.yml",
    "taskfile.yml",
    "Taskfile.yaml",
    "taskfile.yaml",
    "Taskfile.dist.yml",
    "taskfile.dist.yml",
    "Taskfile.dist.yaml",
    "taskfile.dist.yaml",
];

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
    /// A directory rather than a file: `.devcontainer/` itself, whose
    /// ghost creates the folder and reveals the ghosts of what goes in it.
    pub is_dir: bool,
}

/// Every conventional location, present or not.
///
/// The devcontainer is three entries that take turns (David, 2026-09-16:
/// "If .devcontainer/ exists but no json/Containerfile in it, show a ghost
/// entry for the json + Containerfile. If .devcontainer/ doesn't exist,
/// there should be a ghost to create the directory"): the directory, a
/// ghost while it is missing; then, inside it, `devcontainer.json` while
/// no config exists anywhere, and the `Containerfile` while the config is
/// missing or names a build file that is.
pub fn conventions(root: &Path) -> Vec<Convention> {
    conventions_via(&crate::files::Files::Local, root)
}

/// [`conventions`], wherever the checkout is: the same fixed places,
/// asked of the files service that has the tree.
pub fn conventions_via(files: &crate::files::Files, root: &Path) -> Vec<Convention> {
    // The CONFIG file decides existence: a leftover empty .devcontainer/
    // directory must not silence the suggestion to create the file in it.
    let dir = root.join(".devcontainer");
    let has_devcontainer = files.exists(&dir.join("devcontainer.json"))
        || files.exists(&root.join(".devcontainer.json"))
        || files
            .list(&dir)
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| files.exists(&dir.join(&e.name).join("devcontainer.json")))
            })
            .unwrap_or(false);
    let dir_exists = files.is_dir(&dir);
    let mut list = vec![
        Convention {
            exists: dir_exists || has_devcontainer,
            path: dir.clone(),
            purpose: "the devcontainer's folder: devcontainer.json and, when the image is \
                      built rather than pulled, its Containerfile",
            ghost: true,
            is_dir: true,
        },
        Convention {
            path: dir.join("devcontainer.json"),
            purpose: "devcontainer definition; the IDE builds and attaches to it \
                      (validated: privilege only as nested podman needs it, mounts stay in the workspace)",
            exists: has_devcontainer,
            // Offered inside the folder, so only once the folder is there.
            ghost: dir_exists,
            is_dir: false,
        },
    ];
    if dir_exists {
        if let Some(build_file) = wanted_build_file(files, &dir) {
            list.push(Convention {
                exists: false,
                path: dir.join(build_file),
                purpose: "the image the devcontainer builds from, referenced by \
                          devcontainer.json's build.dockerfile; leave it out and pull an \
                          image instead",
                ghost: true,
                is_dir: false,
            });
        }
    }
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
            exists: files.exists(&path),
            path,
            purpose,
            ghost: true,
            is_dir: false,
        });
    }
    // The project's named commands, for the Tasks section (taskfile.dev).
    // Present under any of the names `task` itself looks for; the ghost
    // offers the canonical one.
    list.push(Convention {
        exists: TASKFILE_NAMES
            .iter()
            .any(|name| files.exists(&root.join(name))),
        path: root.join(TASKFILE_NAMES[0]),
        purpose: "the project's named commands (taskfile.dev): the IDE lists them under \
                  Tasks and runs them in the environment",
        ghost: true,
        is_dir: false,
    });
    list.push(Convention {
        exists: files.exists(&root.join(".taste.yaml")),
        path: root.join(".taste.yaml"),
        purpose: "reserved for repo-level IDE configuration; currently \
                  nothing needs it — prefer the conventions above over \
                  adding configuration",
        ghost: false,
        is_dir: false,
    });
    list
}

/// The build file `.devcontainer/` is missing, if it is missing one: the
/// file `devcontainer.json`'s `build.dockerfile` names when that file is
/// absent, or `Containerfile` when there is no config yet. `None` when a
/// build file is present, or when the config pulls an image and names
/// none — an image-based config wants no Containerfile ghost beside it.
fn wanted_build_file(files: &crate::files::Files, dir: &Path) -> Option<String> {
    let config = files.read_to_string(&dir.join("devcontainer.json")).ok();
    let named = config.as_deref().and_then(build_dockerfile);
    match (config.is_some(), named) {
        (true, Some(name)) => (!files.exists(&dir.join(&name))).then_some(name),
        (true, None) => None,
        (false, _) => {
            let present = ["Containerfile", "Dockerfile"]
                .iter()
                .any(|name| files.exists(&dir.join(name)));
            (!present).then(|| "Containerfile".to_string())
        }
    }
}

/// `build.dockerfile` (or the older top-level `dockerFile`) out of a
/// devcontainer.json, read leniently: the file may carry comments and
/// trailing commas, so the value is found rather than parsed.
fn build_dockerfile(text: &str) -> Option<String> {
    for key in ["\"dockerfile\"", "\"dockerFile\""] {
        let Some(at) = text.find(key) else { continue };
        let rest = &text[at + key.len()..];
        let rest = rest.trim_start().strip_prefix(':')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        let name = rest[..end].trim();
        if !name.is_empty() && !name.contains('/') && !name.contains("..") {
            return Some(name.to_string());
        }
    }
    None
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
        // No folder yet: the folder is the ghost, and the file waits for it.
        assert!(ghosts
            .iter()
            .any(|c| c.path.ends_with(".devcontainer") && c.is_dir));
        assert!(!ghosts.iter().any(|c| c.path.ends_with("devcontainer.json")));
        assert!(!ghosts.iter().any(|c| c.path.ends_with(".taste.yaml")));
    }

    #[test]
    fn a_taskfile_under_any_of_tasks_names_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let taskfile = |list: Vec<Convention>| {
            list.into_iter()
                .find(|c| c.path.ends_with("Taskfile.yml"))
                .unwrap()
        };
        assert!(!taskfile(conventions(dir.path())).exists);
        std::fs::write(dir.path().join("taskfile.yaml"), "version: '3'\n").unwrap();
        assert!(taskfile(conventions(dir.path())).exists);
    }

    #[test]
    fn an_empty_devcontainer_folder_offers_the_config_and_a_containerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".devcontainer")).unwrap();
        let ghosts = |list: Vec<Convention>| -> Vec<String> {
            list.into_iter()
                .filter(|c| !c.exists && c.ghost)
                .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            ghosts(conventions(dir.path())),
            vec![
                "devcontainer.json",
                "Containerfile",
                ".editorconfig",
                ".gitignore",
                ".gitattributes",
                "Taskfile.yml"
            ]
        );

        // An image-based config wants no Containerfile; a build-based one
        // wants the file it names, until it exists.
        let config = dir.path().join(".devcontainer/devcontainer.json");
        std::fs::write(&config, r#"{"image": "fedora:44"}"#).unwrap();
        assert!(!ghosts(conventions(dir.path()))
            .iter()
            .any(|g| g.ends_with("file")));
        std::fs::write(
            &config,
            "{\n  // built\n  \"build\": { \"dockerfile\": \"Containerfile.dev\" },\n}",
        )
        .unwrap();
        assert!(ghosts(conventions(dir.path())).contains(&"Containerfile.dev".to_string()));
        std::fs::write(
            dir.path().join(".devcontainer/Containerfile.dev"),
            "FROM x\n",
        )
        .unwrap();
        assert!(!ghosts(conventions(dir.path())).contains(&"Containerfile.dev".to_string()));
    }
}
