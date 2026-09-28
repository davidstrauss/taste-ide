//! A file's icon, from Material Icon Theme.
//!
//! The desktop's own theme has about thirty file-type icons, faint ones,
//! and none for a Cargo.toml, a Taskfile, or a README: the tree read as a
//! column of the same pale page (David, 2026-09-28: "These icons are too
//! low-contrast. I'd prefer an icon set with more color and variety for
//! specific formats"). Material Icon Theme is the set most developers
//! already read at a glance, MIT, vendored and pinned by
//! `build-aux/vendor-file-icons.sh`: its SVGs are installed beside the
//! app's own icons as `taste-mi-<name>`, so the icon theme does the
//! loading, the caching, and the scaling, and its manifest is here.
//!
//! A name resolves the way the set's own manifest means it to: the exact
//! file name first, then the longest extension (`d.ts` before `ts`), then
//! the plain file icon — folders by their name, open or closed. The light
//! scheme's overrides come before the dark ones, since the set is drawn
//! for a dark background and swaps the few icons that vanish on a light
//! one. VS Code's third step, a file's language, is left out: it needs
//! VS Code's language detection, and the extensions cover what it would.

use std::collections::HashMap;
use std::sync::OnceLock;

const MANIFEST: &str = include_str!("../../../data/file-icons/material-icons.json");

/// What is being drawn: a file, or a folder open or closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder { expanded: bool },
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Names {
    #[serde(default)]
    file_extensions: HashMap<String, String>,
    #[serde(default)]
    file_names: HashMap<String, String>,
    #[serde(default)]
    folder_names: HashMap<String, String>,
    #[serde(default)]
    folder_names_expanded: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
struct Definition {
    #[serde(rename = "iconPath")]
    icon_path: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    icon_definitions: HashMap<String, Definition>,
    #[serde(flatten)]
    dark: Names,
    #[serde(default)]
    light: Names,
    file: String,
    folder: String,
    folder_expanded: String,
}

static ICONS: OnceLock<Option<Manifest>> = OnceLock::new();

/// Parse the manifest off the GTK thread, once, at startup. Until it is in,
/// [`icon_name`] answers `None` and callers draw the desktop's icon; the
/// tree's first rows land well after it, since they wait on the folder's
/// own listing.
pub fn init() {
    std::thread::spawn(|| {
        ICONS.get_or_init(parse);
    });
}

fn parse() -> Option<Manifest> {
    match serde_json::from_str(MANIFEST) {
        Ok(manifest) => Some(manifest),
        Err(e) => {
            tracing::warn!("the file icon manifest did not parse: {e}");
            None
        }
    }
}

/// The icon theme name for `name` (a file or folder name, not a path), or
/// `None` while the manifest is not in yet.
pub fn icon_name(name: &str, kind: Kind, dark: bool) -> Option<String> {
    let manifest = ICONS.get()?.as_ref()?;
    let id = resolve(manifest, name, kind, dark);
    let definition = manifest.icon_definitions.get(id)?;
    let stem = std::path::Path::new(&definition.icon_path)
        .file_stem()?
        .to_str()?;
    Some(format!("taste-mi-{stem}"))
}

/// The manifest's icon id for `name`.
fn resolve<'a>(manifest: &'a Manifest, name: &str, kind: Kind, dark: bool) -> &'a str {
    let lower = name.to_lowercase();
    // Light first on a light scheme; the dark names are the set's own.
    let schemes: &[&Names] = if dark {
        &[&manifest.dark]
    } else {
        &[&manifest.light, &manifest.dark]
    };
    let first = |pick: fn(&Names) -> &HashMap<String, String>, key: &str| {
        schemes
            .iter()
            .find_map(|names| pick(names).get(key).map(String::as_str))
    };
    match kind {
        Kind::Folder { expanded } => {
            let pick: fn(&Names) -> &HashMap<String, String> = if expanded {
                |n| &n.folder_names_expanded
            } else {
                |n| &n.folder_names
            };
            first(pick, &lower).unwrap_or(if expanded {
                &manifest.folder_expanded
            } else {
                &manifest.folder
            })
        }
        Kind::File => first(|n| &n.file_names, &lower)
            .or_else(|| {
                // Every suffix after a dot, longest first.
                lower
                    .match_indices('.')
                    .map(|(at, _)| &lower[at + 1..])
                    .filter(|ext| !ext.is_empty())
                    .find_map(|ext| first(|n| &n.file_extensions, ext))
            })
            .unwrap_or(&manifest.file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(file: &str, kind: Kind) -> String {
        let manifest = parse().expect("the vendored manifest parses");
        let id = resolve(&manifest, file, kind, true).to_string();
        id
    }

    #[test]
    fn a_file_resolves_by_name_then_extension_then_default() {
        // By exact name, case aside.
        assert_eq!(name("README.md", Kind::File), "readme");
        assert_eq!(name("LICENSE", Kind::File), "license");
        assert_eq!(name(".gitignore", Kind::File), "git");
        // By extension, the longest first.
        assert_eq!(name("main.rs", Kind::File), "rust");
        assert_eq!(name("Cargo.toml", Kind::File), "toml");
        assert_ne!(name("types.d.ts", Kind::File), name("app.ts", Kind::File));
        // Neither: the set's plain file.
        assert_eq!(name("net.davidstrauss.Taste.desktop", Kind::File), "file");
    }

    #[test]
    fn a_folder_resolves_by_name_open_or_closed() {
        let closed = name("src", Kind::Folder { expanded: false });
        let open = name("src", Kind::Folder { expanded: true });
        assert_ne!(closed, "folder");
        assert_ne!(closed, open);
        assert_eq!(
            name("zzz-nothing", Kind::Folder { expanded: false }),
            "folder"
        );
    }

    /// Every icon the manifest names is one the vendor script installed.
    #[test]
    fn every_definition_has_its_svg() {
        let manifest = parse().unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/icons/hicolor/scalable/mimetypes");
        let missing: Vec<&str> = manifest
            .icon_definitions
            .values()
            .filter_map(|d| std::path::Path::new(&d.icon_path).file_stem()?.to_str())
            .filter(|stem| !dir.join(format!("taste-mi-{stem}.svg")).is_file())
            .collect();
        assert!(missing.is_empty(), "no SVG for {missing:?}");
    }
}
