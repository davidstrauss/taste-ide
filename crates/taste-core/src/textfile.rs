//! Text files on disk: what a load detected, what `.editorconfig` demands,
//! and the bytes a save should write.
//!
//! This is deliberately GUI-free. The editor's own saves go through it, and
//! so does the agent's write path — which must work for files the user has
//! not opened, with no widget, no realized view, and no display. That also
//! makes the rules below unit-testable, which they are not once they live
//! inside a `GtkTextBuffer`.

use std::path::{Path, PathBuf};

pub use encoding_rs::Encoding;

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
    /// The encoding the file is stored in when it is not UTF-8, as its
    /// project names it ([`read_text`]); `None` is UTF-8.
    pub encoding: Option<&'static encoding_rs::Encoding>,
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
            encoding: None,
        }
    }
}

impl FileFormat {
    /// How an agent's write is saved unless it asks for the editor's save:
    /// its text as sent, with no trailing whitespace trimmed and no final
    /// newline added, in the file's own line endings and BOM. Those two are
    /// how the file is stored rather than what it says, and an agent reads
    /// a CRLF file as LF, so writing its text back as LF would change every
    /// line. The trimming is the user's editor's habit, and an agent's
    /// trailing double space (a Markdown line break, a fixture) or blank
    /// lines at the end are what it meant (David, 2026-10-24: "allow an
    /// option for the agent to save a buffer literally … it should be the
    /// default").
    pub fn literal(&self) -> FileFormat {
        FileFormat {
            trim_trailing_ws: false,
            final_newline: false,
            eol_override: None,
            ..self.clone()
        }
    }

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
    /// `charset` naming an encoding other than UTF-8: `latin1`,
    /// `utf-16le`, or `utf-16be`.
    pub charset: Option<&'static encoding_rs::Encoding>,
}

impl EditorConfig {
    /// Read `.editorconfig` for `path`. **This does filesystem IO** — call
    /// it off the GTK main thread.
    pub fn read(path: &Path) -> Self {
        match ec4rs::properties_of(path) {
            Ok(props) => Self::from_properties(props),
            Err(_) => Self::unconfigured(),
        }
    }

    /// [`Self::read`], wherever the file is. Through a remote files service
    /// — which is where nearly every file the IDE opens is, since the
    /// checkouts live in VMs — each ancestor's `.editorconfig` is read
    /// through the service and handed to the same parser, nearest last, up
    /// to the first that says `root = true`: the walk `ec4rs` makes on this
    /// host's directories, made on the file's own. Until 2026-10-06 a remote
    /// file got no `.editorconfig` at all (David: "This IDE practically
    /// *only* opens files from VMs"). **IO, through the service** — off the
    /// GTK main thread.
    pub fn read_via(files: &crate::files::Files, path: &Path) -> Self {
        if files.is_local() {
            return Self::read(path);
        }
        // Nearest first, as found; applied outermost first.
        let mut found: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let mut dir = path.parent();
        while let Some(at) = dir {
            let candidate = at.join(".editorconfig");
            if let Ok(bytes) = files.read(&candidate) {
                let root = ec4rs::ConfigParser::new_buffered(bytes.as_slice())
                    .is_ok_and(|parser| parser.is_root);
                found.push((at.to_path_buf(), bytes));
                if root {
                    break;
                }
            }
            dir = at.parent();
        }
        let mut props = ec4rs::Properties::new();
        for (at, bytes) in found.iter().rev() {
            let Ok(mut parser) = ec4rs::ConfigParser::new_buffered(bytes.as_slice()) else {
                continue;
            };
            let relative = path.strip_prefix(at).unwrap_or(path);
            use ec4rs::PropertiesSource;
            let _ = (&mut parser).apply_to(&mut props, relative);
        }
        Self::from_properties(props)
    }

    /// No `.editorconfig` in force: the defaults a file is saved with.
    fn unconfigured() -> Self {
        Self {
            final_newline: true,
            ..Self::default()
        }
    }

    fn from_properties(mut props: ec4rs::Properties) -> Self {
        use ec4rs::property::*;
        let mut out = Self::unconfigured();
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
        out.bom = match props.get::<Charset>() {
            Ok(Charset::Utf8Bom) => Some(true),
            Ok(Charset::Utf8) => Some(false),
            _ => None,
        };
        // Buffers are UTF-8; these are decoded at a load and encoded again
        // at a save ([`read_text`], [`encode`]). Latin-1 is read as
        // windows-1252, whose decoder keeps the bytes 0x80 to 0x9F as the
        // C1 controls Latin-1 has there, so a file goes back as it came.
        out.charset = match props.get::<Charset>() {
            Ok(Charset::Latin1) => Some(encoding_rs::WINDOWS_1252),
            Ok(Charset::Utf16Le) => Some(encoding_rs::UTF_16LE),
            Ok(Charset::Utf16Be) => Some(encoding_rs::UTF_16BE),
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
        // A file read as plain ASCII still saves in the charset its
        // project names, once something non-ASCII is typed into it.
        if format.encoding.is_none() {
            format.encoding = self.charset;
        }
    }
}

/// A file's text: UTF-8, as nearly every file is, or decoded from the
/// encoding its project says it is in, with that encoding to save it back
/// in. **IO, through the service** — off the GTK main thread.
///
/// What says so, first to last: a UTF-16 byte-order mark; `.editorconfig`'s
/// `charset`; and the file's git attributes, `working-tree-encoding` (git
/// keeps the file UTF-8 in the repository and in this encoding in the
/// working tree) or `encoding` (the one `gitk` and `git gui` show it in),
/// read with `git check-attr` beside the file, which reads attributes and
/// runs nothing. They are asked only of a file that is not plain UTF-8 —
/// one that does not decode as UTF-8, or has a NUL in it, as UTF-16 does —
/// so a UTF-8 file costs no more than it did. The BOM is kept in the text,
/// where [`normalize_load`] finds it and a save puts it back, in the
/// file's own byte order. A file that is not UTF-8 and names no encoding
/// is refused as it always was.
pub fn read_text(
    files: &crate::files::Files,
    path: &Path,
) -> std::io::Result<(String, Option<&'static encoding_rs::Encoding>)> {
    let bytes = files.read(path)?;
    let utf8 = std::str::from_utf8(&bytes).is_ok();
    if utf8 && !bytes.contains(&0) {
        return Ok((String::from_utf8(bytes).unwrap_or_default(), None));
    }
    let named = declared_encoding(files, path, &bytes);
    match named {
        Some(encoding) if encoding != encoding_rs::UTF_8 => {
            let (text, malformed) = encoding.decode_without_bom_handling(&bytes);
            if malformed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("not valid {}", encoding.name()),
                ));
            }
            Ok((text.into_owned(), Some(encoding)))
        }
        _ if utf8 => Ok((String::from_utf8(bytes).unwrap_or_default(), None)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )),
    }
}

/// The encoding `path`'s bytes, its `.editorconfig`, or its attributes name
/// ([`read_text`]).
fn declared_encoding(
    files: &crate::files::Files,
    path: &Path,
    bytes: &[u8],
) -> Option<&'static encoding_rs::Encoding> {
    match bytes {
        [0xFF, 0xFE, ..] => return Some(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => return Some(encoding_rs::UTF_16BE),
        _ => {}
    }
    if let Some(charset) = EditorConfig::read_via(files, path).charset {
        return Some(charset);
    }
    let (dir, name) = (path.parent()?, path.file_name()?);
    let argv: Vec<String> = [
        "env",
        "-u",
        "GIT_ATTR_SOURCE",
        "git",
        "check-attr",
        "-z",
        "working-tree-encoding",
        "encoding",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([name.to_string_lossy().into_owned()])
    .collect();
    let out = files.exec(dir, &argv).ok()?;
    if !out.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let fields: Vec<&str> = text.split('\0').collect();
    fields.chunks(3).find_map(|entry| match entry {
        [_, _, value] if !matches!(*value, "unspecified" | "set" | "unset") => {
            encoding_rs::Encoding::for_label(value.as_bytes())
        }
        _ => None,
    })
}

/// The bytes `rendered` is saved as in `format`'s encoding: UTF-8, or the
/// encoding it was read in. A character that encoding has no byte for is
/// refused with its name rather than written as something else — the
/// encoder's own fallback is an HTML character reference, which in a
/// source file is a corruption nobody asked for.
pub fn encode(rendered: &str, format: &FileFormat) -> Result<Vec<u8>, String> {
    let Some(encoding) = format.encoding.filter(|e| *e != encoding_rs::UTF_8) else {
        return Ok(rendered.as_bytes().to_vec());
    };
    // The WHATWG encoder writes UTF-8 for either UTF-16, so those two are
    // written here.
    if encoding == encoding_rs::UTF_16LE {
        return Ok(rendered.encode_utf16().flat_map(u16::to_le_bytes).collect());
    }
    if encoding == encoding_rs::UTF_16BE {
        return Ok(rendered.encode_utf16().flat_map(u16::to_be_bytes).collect());
    }
    let (bytes, _, unmappable) = encoding.encode(rendered);
    if unmappable {
        let missing = rendered
            .chars()
            .find(|c| {
                let mut one = [0u8; 4];
                encoding.encode(c.encode_utf8(&mut one)).2
            })
            .map(|c| format!(" (“{c}”)"))
            .unwrap_or_default();
        return Err(format!(
            "it has a character {} cannot hold{missing}; it is kept in the editor, unsaved",
            encoding.name()
        ));
    }
    Ok(bytes.into_owned())
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
    let (raw, encoding) = match read_text(&crate::files::Files::Local, path) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e),
    };
    let (text, crlf, bom) = normalize_load(&raw);
    let mut format = FileFormat {
        crlf,
        bom,
        encoding,
        ..FileFormat::default()
    };
    format.apply_editorconfig(path);
    Ok((text, format))
}

/// [`load`], wherever the file is: the bytes and the `.editorconfig`s
/// alike come through the service ([`EditorConfig::read_via`]).
pub fn load_via(files: &crate::files::Files, path: &Path) -> std::io::Result<(String, FileFormat)> {
    if files.is_local() {
        return load(path);
    }
    let (raw, encoding) = match read_text(files, path) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e),
    };
    let (text, crlf, bom) = normalize_load(&raw);
    let mut format = FileFormat {
        crlf,
        bom,
        encoding,
        ..FileFormat::default()
    };
    EditorConfig::read_via(files, path).apply_to(&mut format);
    Ok((text, format))
}

/// [`save`], wherever the file is. The same policy check, the same bytes;
/// only the write goes through the service.
pub fn save_via(
    files: &crate::files::Files,
    root: &Path,
    safe_mode: bool,
    path: &Path,
    text: &str,
    format: &FileFormat,
) -> Result<u64, String> {
    if files.is_local() {
        return save(root, safe_mode, path, text, format);
    }
    if !crate::policy::write_allowed(root, safe_mode, path) {
        return Err(format!(
            "{} is read-only in safe mode — only devcontainer setup and \
             workspace dotfiles are editable until the devcontainer runs",
            path.display()
        ));
    }
    let rendered = render(text, format);
    let bytes = encode(&rendered, format).map_err(|e| format!("{}: {e}", path.display()))?;
    files
        .write(path, &bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(content_hash(&rendered))
}

/// The exact bytes a save should write for `text`.
pub fn render(text: &str, format: &FileFormat) -> String {
    // Split on the newlines rather than by `lines()`, which drops the last
    // one: trimming took a file's final newline with it, and with
    // `insert_final_newline` off nothing put it back.
    let mut out = if format.trim_trailing_ws {
        text.split('\n')
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
    let bytes = encode(&rendered, format).map_err(|e| format!("{}: {e}", path.display()))?;
    // Ghost-flow and agent writes both land in directories that may not
    // exist yet.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, &bytes).map_err(|e| e.to_string())?;
    Ok(content_hash(&rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in the encoding its attributes name opens as its text and
    /// saves as the same bytes: Latin-1 through `working-tree-encoding`,
    /// UTF-16 through its byte-order mark, its BOM and byte order kept. A
    /// character Latin-1 has no byte for is refused, never written as
    /// something else.
    #[test]
    fn a_file_in_its_projects_encoding_reads_and_saves_as_itself() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(
            root.join(".gitattributes"),
            "*.txt working-tree-encoding=ISO-8859-1\n",
        )
        .unwrap();
        let latin = root.join("menu.txt");
        std::fs::write(&latin, b"caf\xe9\n").unwrap();
        let (text, format) = load(&latin).unwrap();
        assert_eq!(text, "caf\u{e9}\n");
        assert_eq!(format.encoding, Some(encoding_rs::WINDOWS_1252));
        save(root, false, &latin, "caf\u{e9} cr\u{e8}me\n", &format).unwrap();
        assert_eq!(std::fs::read(&latin).unwrap(), b"caf\xe9 cr\xe8me\n");
        let refused = save(root, false, &latin, "\u{2603}\n", &format).unwrap_err();
        assert!(refused.contains('\u{2603}'), "{refused}");
        assert_eq!(std::fs::read(&latin).unwrap(), b"caf\xe9 cr\xe8me\n");

        let wide = root.join("wide.dat");
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("hi\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&wide, &bytes).unwrap();
        let (text, format) = load(&wide).unwrap();
        assert_eq!((text.as_str(), format.bom), ("hi\n", true));
        save(root, false, &wide, &text, &format).unwrap();
        assert_eq!(std::fs::read(&wide).unwrap(), bytes);

        // Plain UTF-8 asks nobody, and is what it always was.
        let plain = root.join("plain.md");
        std::fs::write(&plain, "caf\u{e9}\n").unwrap();
        assert_eq!(load(&plain).unwrap().1.encoding, None);
    }

    /// Trimming takes the spaces at the ends of lines and nothing else: the
    /// final newline and the blank lines before it stay.
    #[test]
    fn trimming_keeps_the_final_newline_and_the_blank_lines() {
        let trim = FileFormat {
            trim_trailing_ws: true,
            final_newline: false,
            ..FileFormat::default()
        };
        assert_eq!(render("a  \nb\t\n\n\n", &trim), "a\nb\n\n\n");
        assert_eq!(render("a  ", &trim), "a");
    }

    /// A literal save writes the text as sent, in the file's own line
    /// endings and BOM, whatever `.editorconfig` would trim or add.
    #[test]
    fn a_literal_save_is_the_text_as_sent() {
        let editor = FileFormat {
            crlf: true,
            bom: false,
            trim_trailing_ws: true,
            final_newline: true,
            eol_override: Some(false),
            encoding: None,
        };
        let text = "**Star pitch**  \nline\n\n\n";
        assert_eq!(render(text, &editor), "**Star pitch**\nline\n\n\n");
        assert_eq!(
            render(text, &editor.literal()),
            "**Star pitch**  \r\nline\r\n\r\n\r\n"
        );
        let lf = FileFormat::default();
        assert_eq!(render("no newline", &lf.literal()), "no newline");
        assert_eq!(render("no newline", &lf), "no newline\n");
    }

    /// A files service that is not this host's, over a temporary directory:
    /// what a checkout in a VM looks like to the editor.
    #[derive(Debug)]
    struct Elsewhere;

    impl crate::files::RemoteFiles for Elsewhere {
        fn describe(&self) -> String {
            "elsewhere".into()
        }
        fn stat(&self, path: &Path) -> std::io::Result<crate::files::Stat> {
            crate::files::Files::Local.stat(path)
        }
        fn list(&self, path: &Path) -> std::io::Result<Vec<crate::files::Entry>> {
            crate::files::Files::Local.list(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            std::fs::read(path)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            std::fs::write(path, bytes)
        }
        fn mkdir_all(&self, path: &Path) -> std::io::Result<()> {
            std::fs::create_dir_all(path)
        }
        fn remove(&self, path: &Path, _recursive: bool) -> std::io::Result<()> {
            std::fs::remove_file(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            std::fs::rename(from, to)
        }
        fn exec(&self, _cwd: &Path, _argv: &[String]) -> std::io::Result<crate::files::ExecOutput> {
            Err(std::io::Error::other("no exec here"))
        }
    }

    #[test]
    fn a_remote_file_takes_its_editorconfigs_nearest_last_up_to_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        // Above the root: never read.
        std::fs::write(dir.path().join(".editorconfig"), "[*]\nindent_size = 8\n").unwrap();
        std::fs::write(
            checkout.join(".editorconfig"),
            "root = true\n[*]\nindent_style = space\nindent_size = 4\n[*.md]\ntrim_trailing_whitespace = true\n",
        )
        .unwrap();
        std::fs::write(
            checkout.join("src/.editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .unwrap();
        let files = crate::files::Files::Remote(std::sync::Arc::new(Elsewhere));

        let rust = EditorConfig::read_via(&files, &checkout.join("src/main.rs"));
        assert_eq!(rust.indent_spaces, Some(true));
        assert_eq!(rust.indent_width, Some(2), "the nearer file wins");

        let readme = EditorConfig::read_via(&files, &checkout.join("README.md"));
        assert_eq!(readme.indent_width, Some(4), "the root stops the walk");
        assert!(
            readme.trim_trailing_ws,
            "a glob is matched against the path below its file"
        );
        assert_eq!(
            EditorConfig::read_via(&files, &checkout.join("README.md")),
            EditorConfig::read(&checkout.join("README.md"))
        );
    }

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
