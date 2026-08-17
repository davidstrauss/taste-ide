//! Flatpak manifest discovery.
//!
//! Opinionated locations: `build-aux/flatpak/*.json` (preferred) and
//! reverse-DNS-named `*.json` at the workspace root. A candidate counts
//! when it parses as JSON(C) and carries an `app-id`/`id`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakManifest {
    pub path: PathBuf,
    pub app_id: String,
}

impl FlatpakManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut stripped = String::new();
        json_comments::StripComments::new(raw.as_bytes())
            .read_to_string(&mut stripped)
            .context("stripping JSONC comments")?;
        let value: serde_json::Value = serde_json::from_str(&stripped)
            .with_context(|| format!("parsing {}", path.display()))?;
        let app_id = value["app-id"]
            .as_str()
            .or_else(|| value["id"].as_str())
            .with_context(|| format!("{}: no app-id", path.display()))?
            .to_string();
        Ok(Self {
            path: path.to_path_buf(),
            app_id,
        })
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// The `cargo-sources.json` files this manifest references, resolved
    /// relative to the manifest — used for a friendly missing-file
    /// diagnostic before a long build fails obscurely.
    pub fn referenced_cargo_sources(&self) -> Vec<PathBuf> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut stripped = String::new();
        if json_comments::StripComments::new(raw.as_bytes())
            .read_to_string(&mut stripped)
            .is_err()
        {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&stripped) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        collect_source_strings(&value, &mut found);
        found
            .into_iter()
            .filter(|s| s.ends_with("cargo-sources.json"))
            .map(|s| self.dir().join(s))
            .collect()
    }
}

fn collect_source_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(sources) = map.get("sources").and_then(|s| s.as_array()) {
                for source in sources {
                    if let Some(s) = source.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
            for v in map.values() {
                collect_source_strings(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_source_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Find the workspace's Flatpak manifest, if any.
pub fn discover(workspace_root: &Path) -> Option<FlatpakManifest> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(workspace_root.join("build-aux/flatpak")) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .filter(|p| {
                p.file_name()
                    .map(|n| n != "cargo-sources.json")
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        candidates.extend(paths);
    }
    // Root-level reverse-DNS-named manifests (e.g. org.example.App.json).
    if let Ok(entries) = std::fs::read_dir(workspace_root) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .filter(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.matches('.').count() >= 2)
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        candidates.extend(paths);
    }
    candidates
        .iter()
        .find_map(|path| FlatpakManifest::load(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_manifest_in_build_aux() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("build-aux/flatpak");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::write(
            fp.join("org.example.App.json"),
            r#"{"app-id": "org.example.App", "modules": []}"#,
        )
        .unwrap();
        let manifest = discover(dir.path()).unwrap();
        assert_eq!(manifest.app_id, "org.example.App");
    }

    #[test]
    fn cargo_sources_json_is_not_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("build-aux/flatpak");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::write(fp.join("cargo-sources.json"), r#"[{"type": "file"}]"#).unwrap();
        assert!(discover(dir.path()).is_none());
    }

    #[test]
    fn discovers_reverse_dns_manifest_at_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("io.foo.Bar.json"),
            r#"{"id": "io.foo.Bar"}"#,
        )
        .unwrap();
        // Non-manifest json at root is ignored.
        std::fs::write(dir.path().join("package.json"), r#"{"name": "x"}"#).unwrap();
        let manifest = discover(dir.path()).unwrap();
        assert_eq!(manifest.app_id, "io.foo.Bar");
    }

    #[test]
    fn finds_referenced_cargo_sources() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("build-aux/flatpak");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::write(
            fp.join("org.example.App.json"),
            r#"{
                "app-id": "org.example.App",
                "modules": [
                    {"name": "app", "sources": [{"type": "dir", "path": "."}, "cargo-sources.json"]}
                ]
            }"#,
        )
        .unwrap();
        let manifest = discover(dir.path()).unwrap();
        let refs = manifest.referenced_cargo_sources();
        assert_eq!(refs, vec![fp.join("cargo-sources.json")]);
    }
}
