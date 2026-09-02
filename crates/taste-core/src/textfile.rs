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
        EditorConfig::read(path).apply_to(self);
    }
}

/// Everything `.editorconfig` says about one file, read once.
///
/// `ec4rs::properties_of` walks every ancestor directory looking for
/// `.editorconfig` — a real filesystem cost, and a cold one in a checkout
/// some agent is writing to. The editor used to pay it twice per open, on
/// the GTK main thread: once for indentation and the margin guide, and
/// again inside [`FileFormat::apply_editorconfig`] for the save-time half.
/// This is that walk as a value, so the read happens once, off the main
/// thread, and both halves are applied from memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorConfig {
    /// `indent_style`: `Some(true)` = spaces, `Some(false)` = tabs.
    pub indent_spaces: Option<bool>,
    /// `indent_size`, in columns; `Some(-1)` is `indent_size = tab`, which
    /// means "follow tab_width".
    pub indent_width: Option<i32>,
    pub tab_width: Option<u32>,
    pub max_line_len: Option<u32>,
    trim_trailing_ws: bool,
    final_newline: bool,
    eol_override: Option<bool>,
    /// `charset`: `Some(true)` demands a BOM, `Some(false)` forbids one,
    /// `None` leaves whatever the file had.
    bom: Option<bool>,
}

impl EditorConfig {
    /// Read `.editorconfig` for `path`. **This does filesystem IO** — call
    /// it off the GTK main thread.
    pub fn read(path: &Path) -> Self {
        use ec4rs::property::*;
        let mut out = Self {
            final_newline: true,
            ..Self::default()
        };
        let Ok(mut props) = ec4rs::properties_of(path) else {
            return out;
        };
        props.use_fallbacks();
        out.indent_spaces = props
            .get::<IndentStyle>()
            .ok()
            .map(|s| s == IndentStyle::Spaces);
        out.indent_width = match props.get::<IndentSize>() {
            Ok(IndentSize::Value(size)) => Some(size as i32),
            Ok(IndentSize::UseTabWidth) => Some(-1),
            Err(_) => None,
        };
        out.tab_width = match props.get::<TabWidth>() {
            Ok(TabWidth::Value(width)) => Some(width as u32),
            _ => None,
        };
        out.max_line_len = match props.get::<MaxLineLen>() {
            Ok(MaxLineLen::Value(width)) => Some(width as u32),
            _ => None,
        };
        out.trim_trailing_ws = props.get::<TrimTrailingWs>() == Ok(TrimTrailingWs::Value(true));
        out.final_newline = props.get::<FinalNewline>() != Ok(FinalNewline::Value(false));
        // cr-only files are museum pieces; treated as lf.
        out.eol_override = match props.get::<EndOfLine>() {
            Ok(EndOfLine::CrLf) => Some(true),
            Ok(EndOfLine::Lf) | Ok(EndOfLine::Cr) => Some(false),
            Err(_) => None,
        };
        // Other charsets are not re-encoded: buffers are UTF-8.
        out.bom = match props.get::<Charset>() {
            Ok(Charset::Utf8Bom) => Some(true),
            Ok(Charset::Utf8) => Some(false),
            _ => None,
        };
        out
    }

    /// Fold the save-time half into a format detected at load. What the
    /// file *had* (CRLF, a BOM) survives unless `.editorconfig` overrules
    /// it, which is the same rule the two-walk version applied.
    pub fn apply_to(&self, format: &mut FileFormat) {
        format.trim_trailing_ws = self.trim_trailing_ws;
        format.final_newline = self.final_newline;
        format.eol_override = self.eol_override;
        if let Some(bom) = self.bom {
            format.bom = bom;
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

    /// One read has to say everything the two reads said, or the editor
    /// quietly loses a project's indentation rules.
    #[test]
    fn one_read_answers_both_halves() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".editorconfig"),
            "root = true\n\n[*]\nindent_style = space\nindent_size = 3\n\
             tab_width = 7\nmax_line_length = 88\ntrim_trailing_whitespace = true\n\
             insert_final_newline = false\nend_of_line = crlf\n",
        )
        .unwrap();
        let file = tmp.path().join("src.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let config = EditorConfig::read(&file);
        assert_eq!(config.indent_spaces, Some(true));
        assert_eq!(config.indent_width, Some(3));
        assert_eq!(config.tab_width, Some(7));
        assert_eq!(config.max_line_len, Some(88));

        // The save-time half must match what the old second read produced.
        let mut folded = FileFormat::default();
        config.apply_to(&mut folded);
        let mut directly = FileFormat::default();
        directly.apply_editorconfig(&file);
        assert_eq!(folded, directly);
        assert!(folded.trim_trailing_ws);
        assert!(!folded.final_newline);
        assert_eq!(folded.eol_override, Some(true));
    }

    /// What the file had survives a `.editorconfig` with no opinion — the
    /// rule the two-walk version applied, and the one a save depends on.
    #[test]
    fn a_silent_editorconfig_keeps_what_the_file_had() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("src.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut format = FileFormat {
            crlf: true,
            bom: true,
            ..FileFormat::default()
        };
        EditorConfig::read(&file).apply_to(&mut format);
        assert!(
            format.bom,
            "a BOM the file carried is not an opinion to drop"
        );
        assert!(format.crlf, "nor is the line ending it was read with");
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-core perf_ -- --ignored --nocapture`
    ///
    /// The `.editorconfig` ancestor walk, which every editor open pays.
    /// The number that matters is **walks per open**: it was two — the
    /// editor read the file for indentation, then `FileFormat` read it
    /// again for the save-time half — both on the GTK main thread. Depth is
    /// swept because the walk is per ancestor directory, and an
    /// environment's clone sits several levels deeper than the workspace.
    #[test]
    #[ignore]
    fn perf_editorconfig_walk() {
        const OPENS: usize = 300;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".editorconfig"),
            "root = true\n\n[*]\nindent_style = space\nindent_size = 4\n",
        )
        .unwrap();
        for depth in [2usize, 6, 12] {
            let mut dir = tmp.path().to_path_buf();
            for level in 0..depth {
                dir = dir.join(format!("d{depth}-{level}"));
            }
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("src.rs");
            std::fs::write(&file, "fn main() {}\n").unwrap();

            let start = std::time::Instant::now();
            for _ in 0..OPENS {
                let mut format = FileFormat::default();
                std::hint::black_box(ec4rs::properties_of(&file).ok());
                format.apply_editorconfig(&file);
                std::hint::black_box(format);
            }
            let before = start.elapsed();

            let start = std::time::Instant::now();
            for _ in 0..OPENS {
                let config = EditorConfig::read(&file);
                let mut format = FileFormat::default();
                config.apply_to(&mut format);
                std::hint::black_box((config, format));
            }
            let after = start.elapsed();

            println!(
                "editorconfig walk: depth {depth:>2} → before {:>9.1?}/open (2 walks), \
                 after {:>9.1?}/open (1 walk) ({:.1}x)",
                before / OPENS as u32,
                after / OPENS as u32,
                before.as_secs_f64() / after.as_secs_f64().max(f64::EPSILON),
            );
            assert!(after < before, "one walk must beat two");
        }
    }

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
