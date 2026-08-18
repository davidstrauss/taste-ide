//! Text files on disk: what a load detected, what `.editorconfig` demands,
//! and the bytes a save should write.
//!
//! This is deliberately GUI-free. The editor's own saves go through it, and
//! so does the agent's write path — which must work for files the user has
//! not opened, with no widget, no realized view, and no display. That also
//! makes the rules below unit-testable, which they are not once they live
//! inside a `GtkTextBuffer`.

use std::path::Path;

/// How a file is stored: detected at load, overridden by `.editorconfig`.
///
/// Buffers are LF-only and BOM-free internally; this is what puts the file
/// back the way its project wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFormat {
    /// The file used CRLF when it was read.
    pub crlf: bool,
    /// The file carried a UTF-8 BOM (or `charset = utf-8-bom` demands one).
    pub bom: bool,
    pub trim_trailing_ws: bool,
    pub final_newline: bool,
    /// `end_of_line`: `Some(true)` = CRLF, `Some(false)` = LF, `None` =
    /// keep whatever the file had.
    pub eol_override: Option<bool>,
}

impl Default for FileFormat {
    fn default() -> Self {
        Self {
            crlf: false,
            bom: false,
            trim_trailing_ws: false,
            // A trailing newline is the default everywhere that has an
            // opinion; `.editorconfig` can still turn it off.
            final_newline: true,
            eol_override: None,
        }
    }
}

impl FileFormat {
    /// Fold `.editorconfig`'s save-time properties in. Indentation and the
    /// right-margin guide are the editor's business and stay there; these
    /// are the ones that change the bytes.
    pub fn apply_editorconfig(&mut self, path: &Path) {
        use ec4rs::property::*;
        let Ok(mut props) = ec4rs::properties_of(path) else {
            return;
        };
        props.use_fallbacks();
        self.trim_trailing_ws = props.get::<TrimTrailingWs>() == Ok(TrimTrailingWs::Value(true));
        self.final_newline = props.get::<FinalNewline>() != Ok(FinalNewline::Value(false));
        // cr-only files are museum pieces; treated as lf.
        self.eol_override = match props.get::<EndOfLine>() {
            Ok(EndOfLine::CrLf) => Some(true),
            Ok(EndOfLine::Lf) | Ok(EndOfLine::Cr) => Some(false),
            Err(_) => None,
        };
        // Other charsets are not re-encoded: buffers are UTF-8.
        match props.get::<Charset>() {
            Ok(Charset::Utf8Bom) => self.bom = true,
            Ok(Charset::Utf8) => self.bom = false,
            _ => {}
        }
    }
}

/// Strip the BOM and fold CRLF to LF, reporting what was there so a save can
/// put it back.
pub fn normalize_load(raw: &str) -> (String, bool, bool) {
    let bom = raw.starts_with('\u{feff}');
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let crlf = text.contains("\r\n");
    let clean = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    };
    (clean, crlf, bom)
}

/// Read a file and split it into buffer text plus the format to save it
/// back with, `.editorconfig` included. A missing file is not an error —
/// it is a new file, and its format comes from `.editorconfig` alone.
pub fn load(path: &Path) -> std::io::Result<(String, FileFormat)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let (text, crlf, bom) = normalize_load(&raw);
    let mut format = FileFormat {
        crlf,
        bom,
        ..FileFormat::default()
    };
    format.apply_editorconfig(path);
    Ok((text, format))
}

/// The exact bytes a save should write for `text`.
pub fn render(text: &str, format: &FileFormat) -> String {
    let mut out = if format.trim_trailing_ws {
        text.lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };
    if format.final_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    if format.eol_override.unwrap_or(format.crlf) {
        out = out.replace('\n', "\r\n");
    }
    if format.bom {
        out.insert(0, '\u{feff}');
    }
    out
}

/// Stable hash of the bytes a save wrote. The watcher echoes our own writes
/// back as changes; matching this is what tells an echo from a real one.
pub fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Render and write `text` to `path`, refusing whatever the write policy
/// refuses. Returns the hash of the bytes written.
///
/// The policy check binds the agent exactly as it binds the user: in safe
/// mode only the safe-mode scope is writable, whoever is asking.
pub fn save(
    root: &Path,
    safe_mode: bool,
    path: &Path,
    text: &str,
    format: &FileFormat,
) -> Result<u64, String> {
    if !crate::policy::write_allowed(root, safe_mode, path) {
        return Err(format!(
            "{} is read-only in safe mode — only devcontainer setup and \
             workspace dotfiles are editable until the devcontainer runs",
            path.display()
        ));
    }
    let rendered = render(text, format);
    // Ghost-flow and agent writes both land in directories that may not
    // exist yet.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, &rendered).map_err(|e| e.to_string())?;
    Ok(content_hash(&rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reports_what_it_stripped() {
        let (text, crlf, bom) = normalize_load("\u{feff}one\r\ntwo\r\n");
        assert_eq!(text, "one\ntwo\n");
        assert!(crlf);
        assert!(bom);

        let (text, crlf, bom) = normalize_load("one\ntwo\n");
        assert_eq!(text, "one\ntwo\n");
        assert!(!crlf);
        assert!(!bom);
    }

    #[test]
    fn render_restores_line_endings_and_bom() {
        let format = FileFormat {
            crlf: true,
            bom: true,
            ..FileFormat::default()
        };
        assert_eq!(render("one\ntwo\n", &format), "\u{feff}one\r\ntwo\r\n");
    }

    #[test]
    fn eol_override_beats_detection() {
        let lf = FileFormat {
            crlf: true,
            eol_override: Some(false),
            ..FileFormat::default()
        };
        assert_eq!(render("one\n", &lf), "one\n");
        let crlf = FileFormat {
            crlf: false,
            eol_override: Some(true),
            ..FileFormat::default()
        };
        assert_eq!(render("one\n", &crlf), "one\r\n");
    }

    #[test]
    fn whitespace_policies_apply() {
        let trim = FileFormat {
            trim_trailing_ws: true,
            ..FileFormat::default()
        };
        assert_eq!(render("one   \ntwo\t\n", &trim), "one\ntwo\n");

        let no_newline = FileFormat {
            final_newline: false,
            ..FileFormat::default()
        };
        assert_eq!(render("one", &no_newline), "one");
        assert_eq!(render("one", &FileFormat::default()), "one\n");
    }

    #[test]
    fn a_round_trip_changes_nothing() {
        for raw in ["one\ntwo\n", "\u{feff}one\r\ntwo\r\n", "one\r\ntwo\r\n"] {
            let (text, crlf, bom) = normalize_load(raw);
            let format = FileFormat {
                crlf,
                bom,
                ..FileFormat::default()
            };
            assert_eq!(render(&text, &format), raw, "round trip of {raw:?}");
        }
    }

    #[test]
    fn save_refuses_what_the_policy_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blocked = root.join("src/main.rs");
        let format = FileFormat::default();
        // Safe mode: only devcontainer setup and dotfiles are writable.
        let err = save(root, true, &blocked, "x", &format).unwrap_err();
        assert!(err.contains("read-only in safe mode"), "{err}");
        assert!(!blocked.exists(), "a refused save must not write");

        // Container mode: the same path goes through, parents and all.
        let hash = save(root, false, &blocked, "x", &format).unwrap();
        assert_eq!(std::fs::read_to_string(&blocked).unwrap(), "x\n");
        assert_eq!(hash, content_hash("x\n"));
    }
}
