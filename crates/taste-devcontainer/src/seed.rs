//! A new environment's project configuration, from Personal's working copy
//! when its own clone has none.
//!
//! A new environment is a clone of a commit, so configuration that exists
//! only uncommitted in Personal — a `.devcontainer/` being written, a
//! Taskfile not yet added — is not in it, and the environment came up on
//! the baseline with nothing to run (David, 2026-10-06: "If I spin up
//! additional environments and there's no devcontainer/Task config, fall
//! back to using the working copy one from my Personal env. This is so I
//! can launch other envs even if I only have uncommitted devcontainer
//! config"; and "Same for editorconfig").
//!
//! Each kind of configuration is taken whole or not at all, and only when
//! the clone has none of that kind: a clone with its own committed
//! `.devcontainer/` keeps it, even if Personal's working copy has moved on.
//! What is copied lands as untracked files, visible as any other change is:
//! the environment's snapshot keeps them, and an agent that repairs them in
//! safe mode does so where review can see it.

use std::path::Path;

use anyhow::{bail, Result};
use taste_core::files::{Files, Kind};

/// The kinds of configuration a new environment can take from Personal,
/// each as the paths that count as having it.
pub fn kinds() -> [(&'static str, Vec<&'static str>); 3] {
    [
        (
            "devcontainer config",
            vec![".devcontainer", ".devcontainer.json"],
        ),
        ("Taskfile", taste_core::conventions::TASKFILE_NAMES.to_vec()),
        (".editorconfig", vec![".editorconfig"]),
    ]
}

/// How deep a directory is followed: `.devcontainer/` holds a config, a
/// Containerfile, and scripts, not a tree.
const MAX_DEPTH: usize = 8;

/// Copy into the checkout at `to_root` each kind of configuration it has
/// none of, from Personal's working copy at `from_root`. The paths copied,
/// in order; empty when the clone had everything or Personal had nothing
/// to give.
pub fn from_personal(
    from: &Files,
    from_root: &Path,
    to: &Files,
    to_root: &Path,
) -> Result<Vec<String>> {
    let mut copied = Vec::new();
    for (_, paths) in kinds() {
        if paths.iter().any(|path| to.exists(&to_root.join(path))) {
            continue;
        }
        for path in paths {
            let source = from_root.join(path);
            if from.exists(&source) {
                copy(from, &source, to, &to_root.join(path), 0)?;
                copied.push(path.to_string());
            }
        }
    }
    Ok(copied)
}

/// One file or directory, recursively. Symlinks are left behind, as the
/// config mirror leaves them: a link out of the checkout would name a path
/// on the other side that is not the same file.
fn copy(from: &Files, source: &Path, to: &Files, target: &Path, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("{} is nested deeper than {MAX_DEPTH}", source.display());
    }
    let stat = from.stat(source)?;
    match stat.kind {
        Kind::Dir => {
            to.mkdir_all(target)?;
            for entry in from.list(source)? {
                copy(
                    from,
                    &source.join(&entry.name),
                    to,
                    &target.join(&entry.name),
                    depth + 1,
                )?;
            }
        }
        Kind::File => {
            to.write(target, &from.read(source)?)?;
            if stat.mode & 0o111 != 0 {
                let parent = target.parent().unwrap_or(target);
                let out = to.exec(
                    parent,
                    &[
                        "chmod".into(),
                        "+x".into(),
                        "--".into(),
                        target.display().to_string(),
                    ],
                )?;
                if !out.success() {
                    bail!(
                        "making {} executable: {}",
                        target.display(),
                        out.stderr_utf8().trim()
                    );
                }
            }
        }
        Kind::Symlink | Kind::Other => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn what_the_clone_lacks_comes_from_personal_whole() {
        let personal = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();
        write(personal.path(), ".devcontainer/devcontainer.json", "{}");
        write(personal.path(), ".devcontainer/setup.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            personal.path().join(".devcontainer/setup.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(personal.path(), "Taskfile.yml", "version: '3'\n");
        write(personal.path(), ".editorconfig", "root = true\n");

        let copied =
            from_personal(&Files::Local, personal.path(), &Files::Local, clone.path()).unwrap();

        assert_eq!(copied, [".devcontainer", "Taskfile.yml", ".editorconfig"]);
        assert_eq!(
            std::fs::read_to_string(clone.path().join(".devcontainer/devcontainer.json")).unwrap(),
            "{}"
        );
        let mode = std::fs::metadata(clone.path().join(".devcontainer/setup.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "the script stays executable");
        assert!(clone.path().join("Taskfile.yml").is_file());
        assert!(clone.path().join(".editorconfig").is_file());
    }

    #[test]
    fn a_kind_the_clone_has_is_left_as_it_is() {
        let personal = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();
        write(
            personal.path(),
            ".devcontainer/devcontainer.json",
            "{\"new\":1}",
        );
        write(personal.path(), "Taskfile.yml", "version: '3'\n");
        // The clone's own config is committed, in the other form, and its
        // Taskfile under another of task's names: both count as having one.
        write(clone.path(), ".devcontainer.json", "{\"committed\":1}");
        write(clone.path(), "taskfile.yaml", "version: '3'\n");

        let copied =
            from_personal(&Files::Local, personal.path(), &Files::Local, clone.path()).unwrap();

        assert!(copied.is_empty(), "{copied:?}");
        assert!(!clone.path().join(".devcontainer").exists());
        assert!(!clone.path().join("Taskfile.yml").exists());
    }
}
