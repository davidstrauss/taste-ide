//! User-level file templates.
//!
//! Convention over configuration over code: templates are plain files in a
//! fixed location — `$XDG_CONFIG_HOME/taste-ide/templates/<file-name>/…` —
//! one directory per target file name (`.editorconfig`,
//! `devcontainer.json`, …), one file per template variant. No manifest, no
//! registry, no scripting: the directory listing *is* the configuration.
//! Ghost-file creation offers these variants alongside the built-in
//! default.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The variant's display name (the file name in the templates dir).
    pub name: String,
    pub path: PathBuf,
}

/// `$XDG_CONFIG_HOME/taste-ide` (or `~/.config/taste-ide`).
pub fn user_config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".config")
        });
    base.join("taste-ide")
}

/// Template variants available for creating `file_name`.
pub fn templates_for(file_name: &str) -> Vec<Template> {
    templates_in(&user_config_dir(), file_name)
}

/// Testable core: list `<config_dir>/templates/<file_name>/*`, sorted.
pub fn templates_in(config_dir: &Path, file_name: &str) -> Vec<Template> {
    let dir = config_dir.join("templates").join(file_name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut templates: Vec<Template> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| Template {
            name: e.file_name().to_string_lossy().to_string(),
            path: e.path(),
        })
        .collect();
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    templates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_variants_sorted_and_ignores_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("templates/.editorconfig");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("rust"), "indent_size = 4\n").unwrap();
        std::fs::write(t.join("web"), "indent_size = 2\n").unwrap();
        std::fs::create_dir(t.join("not-a-template")).unwrap();

        let templates = templates_in(dir.path(), ".editorconfig");
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["rust", "web"]);
    }

    #[test]
    fn missing_dir_means_no_variants() {
        let dir = tempfile::tempdir().unwrap();
        assert!(templates_in(dir.path(), ".gitignore").is_empty());
    }
}
