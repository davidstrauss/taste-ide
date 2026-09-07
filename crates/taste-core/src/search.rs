//! Find-in-project: case-insensitive substring search over the workspace,
//! honoring .gitignore, skipping binaries. Deliberately simple — a fast,
//! predictable default rather than a query language.

use std::path::{Path, PathBuf};

/// The user's word, plus the ghost toggle (docs/SEARCH.md). Case-insensitive
/// unless the query carries an uppercase letter ("smart case"); no regular
/// expressions — the box is for the word the user has in mind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    /// Highlight without filtering: surfaces that would hide rows dim them.
    pub ghost: bool,
    /// Include what the semantic index finds by meaning beside the literal
    /// hits (the toggle beside the ghost). On by default: a person asking
    /// the box a question wants the answer whichever way it was found.
    pub meaning: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            ghost: false,
            meaning: true,
        }
    }
}

impl Query {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.trim().to_string(),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn case_sensitive(&self) -> bool {
        self.text.chars().any(char::is_uppercase)
    }

    /// The needle as compared: lowercased unless the query is sensitive.
    fn needle(&self) -> String {
        if self.case_sensitive() {
            self.text.trim().to_string()
        } else {
            self.text.trim().to_lowercase()
        }
    }

    pub fn matches(&self, hay: &str) -> bool {
        let needle = self.needle();
        if needle.is_empty() {
            return true;
        }
        if self.case_sensitive() {
            hay.contains(&needle)
        } else {
            hay.to_lowercase().contains(&needle)
        }
    }

    /// Byte ranges of every match in `hay`, non-overlapping, in order.
    pub fn ranges(&self, hay: &str) -> Vec<(usize, usize)> {
        let needle = self.needle();
        if needle.is_empty() {
            return Vec::new();
        }
        let folded: String;
        let subject: &str = if self.case_sensitive() {
            hay
        } else {
            folded = hay.to_lowercase();
            if folded.len() != hay.len() {
                // Lowercasing changed byte lengths; fold char by char so
                // the ranges stay on this text's boundaries.
                return ranges_charwise(hay, &needle);
            }
            &folded
        };
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = subject[from..].find(&needle) {
            let start = from + at;
            let end = start + needle.len();
            out.push((start, end));
            from = end;
        }
        out
    }

    /// Pango markup: the text escaped, matches in bold.
    pub fn highlight_markup(&self, hay: &str) -> String {
        let ranges = self.ranges(hay);
        if ranges.is_empty() {
            return escape_markup(hay);
        }
        let mut out = String::new();
        let mut at = 0;
        for (start, end) in ranges {
            out.push_str(&escape_markup(&hay[at..start]));
            out.push_str("<b>");
            out.push_str(&escape_markup(&hay[start..end]));
            out.push_str("</b>");
            at = end;
        }
        out.push_str(&escape_markup(&hay[at..]));
        out
    }
}

fn ranges_charwise(hay: &str, needle_lower: &str) -> Vec<(usize, usize)> {
    let needle: Vec<char> = needle_lower.chars().collect();
    let chars: Vec<(usize, char)> = hay.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        let window = &chars[i..i + needle.len()];
        let hit = window
            .iter()
            .zip(&needle)
            .all(|((_, c), n)| c.to_lowercase().eq(n.to_lowercase()));
        if hit {
            let start = window[0].0;
            let end = match chars.get(i + needle.len()) {
                Some((next, _)) => *next,
                None => hay.len(),
            };
            out.push((start, end));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u32,
    /// The matching line, trimmed and clamped for display.
    pub text: String,
}

const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;
const MAX_LINE_DISPLAY: usize = 200;

/// Match one file against a lowercased query; returns false when the hit
/// cap is reached.
fn match_file(path: &Path, query: &str, hits: &mut Vec<SearchHit>, max_hits: usize) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    if meta.len() > MAX_FILE_SIZE {
        return true;
    }
    let Ok(content) = std::fs::read(path) else {
        return true;
    };
    // Binary sniff: NUL byte in the head means skip.
    if content.iter().take(8192).any(|&b| b == 0) {
        return true;
    }
    let Ok(text) = String::from_utf8(content) else {
        return true;
    };
    for (index, line) in text.lines().enumerate() {
        if line.to_lowercase().contains(query) {
            let mut display: String = line.trim().chars().take(MAX_LINE_DISPLAY).collect();
            if line.trim().chars().count() > MAX_LINE_DISPLAY {
                display.push('…');
            }
            hits.push(SearchHit {
                path: path.to_path_buf(),
                line: (index + 1) as u32,
                text: display,
            });
            if hits.len() >= max_hits {
                return false;
            }
        }
    }
    true
}

/// One file's matches, counted in full: `count` is every matching line,
/// `hits` the first `per_file` of them for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    pub path: PathBuf,
    pub count: usize,
    pub hits: Vec<SearchHit>,
}

/// A file's text, or `None` for what search does not read: missing,
/// over-size, binary (a NUL in the head), not UTF-8.
fn readable_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_SIZE {
        return None;
    }
    let content = std::fs::read(path).ok()?;
    if content.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    String::from_utf8(content).ok()
}

/// Search a file list and count every match.
///
/// The capped search above answers "show me some matches" and stops at
/// `max_hits`; asked a common word, it stopped partway down the tree and
/// every file after that point reported zero — including the one open in
/// the editor, whose zero the tree renders in good faith. The tree's
/// per-file counts have to be complete, so nothing here stops early: what
/// is bounded is what is KEPT per file (`per_file` lines of text), and the
/// whole run answers to `cancel`, which a newer query raises so an older
/// one stops reading files nobody will look at. `None` means cancelled.
pub fn search_files_complete(
    files: &[PathBuf],
    query: &str,
    per_file: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<Vec<FileMatches>> {
    search_files_reporting(files, &Query::new(query), per_file, cancel, &mut |_| {})
}

/// The content search every surface reads: every file, the complete count
/// per file, at most `per_file` lines kept, and `progress(files_done)`
/// every few files so a listing can say how far it is. `None` when the
/// stop flag was raised — a query the user moved on from.
pub fn search_files_reporting(
    files: &[PathBuf],
    query: &Query,
    per_file: usize,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(usize),
) -> Option<Vec<FileMatches>> {
    use std::sync::atomic::Ordering;
    if query.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for (done, path) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if done % 64 == 0 {
            progress(done);
        }
        let Some(text) = readable_text(path) else {
            continue;
        };
        let mut count = 0;
        let mut hits = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if !query.matches(line) {
                continue;
            }
            count += 1;
            if hits.len() < per_file {
                let mut display: String = line.trim().chars().take(MAX_LINE_DISPLAY).collect();
                if line.trim().chars().count() > MAX_LINE_DISPLAY {
                    display.push('…');
                }
                hits.push(SearchHit {
                    path: path.to_path_buf(),
                    line: (index + 1) as u32,
                    text: display,
                });
            }
        }
        if count > 0 {
            out.push(FileMatches {
                path: path.to_path_buf(),
                count,
                hits,
            });
        }
    }
    progress(files.len());
    Some(out)
}

/// Search text that is already in memory — an open buffer, a log, a
/// transcript — the same way files are searched: complete count, capped
/// lines, one-based line numbers.
pub fn search_text(text: &str, query: &Query, per_file: usize) -> (usize, Vec<(u32, String)>) {
    if query.is_empty() {
        return (0, Vec::new());
    }
    let mut count = 0;
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if !query.matches(line) {
            continue;
        }
        count += 1;
        if hits.len() < per_file {
            let mut display: String = line.trim().chars().take(MAX_LINE_DISPLAY).collect();
            if line.trim().chars().count() > MAX_LINE_DISPLAY {
                display.push('…');
            }
            hits.push(((index + 1) as u32, display));
        }
    }
    (count, hits)
}

/// Definitions, by each language's own convention. Not a parser: the
/// line-shapes that mean "here is where `name` is introduced" — `fn name`,
/// `struct Name`, `def name`, `class Name`, `function name`, a Markdown
/// heading — indexed with the file index so "find the symbol" is a lookup,
/// and honest about being a convention: the listing shows the line.
pub mod symbols {
    use super::Query;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Symbol {
        pub path: PathBuf,
        pub line: u32,
        /// `fn`, `struct`, `class`, `def`, `heading`, …
        pub kind: &'static str,
        pub name: String,
        /// The defining line, trimmed.
        pub text: String,
    }

    /// Index every readable file. `None` when cancelled.
    pub fn index(files: &[PathBuf], cancel: &std::sync::atomic::AtomicBool) -> Option<Vec<Symbol>> {
        let mut out = Vec::new();
        for path in files {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let Some(language) = language_of(path) else {
                continue;
            };
            let Some(text) = super::readable_text(path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if let Some((kind, name)) = definition(language, line) {
                    out.push(Symbol {
                        path: path.clone(),
                        line: (index + 1) as u32,
                        kind,
                        name,
                        text: line.trim().chars().take(160).collect(),
                    });
                }
            }
        }
        Some(out)
    }

    /// Symbols whose name matches, exact-name matches first.
    pub fn find<'a>(symbols: &'a [Symbol], query: &Query) -> Vec<&'a Symbol> {
        if query.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<&Symbol> = symbols
            .iter()
            .filter(|symbol| query.matches(&symbol.name))
            .collect();
        let exact = query.text.trim();
        hits.sort_by(|a, b| {
            let a_key = (
                !a.name.eq_ignore_ascii_case(exact),
                a.name.len(),
                &a.path,
                a.line,
            );
            let b_key = (
                !b.name.eq_ignore_ascii_case(exact),
                b.name.len(),
                &b.path,
                b.line,
            );
            a_key.cmp(&b_key)
        });
        hits
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Language {
        Rust,
        Python,
        JavaScript,
        Go,
        Shell,
        Markdown,
    }

    pub fn language_of(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Language::JavaScript,
            "go" => Language::Go,
            "sh" | "bash" | "zsh" => Language::Shell,
            "md" | "markdown" => Language::Markdown,
            _ => return None,
        })
    }

    fn identifier(text: &str) -> Option<String> {
        let name: String = text
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let starts_with_digit = name.chars().next().is_some_and(|c| c.is_ascii_digit());
        (!name.is_empty() && !starts_with_digit).then_some(name)
    }

    /// `keyword` as a whole word at the start of `line`, and what follows it.
    fn after_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
        let rest = line.strip_prefix(keyword)?;
        if rest.starts_with(char::is_whitespace) || rest.starts_with('!') {
            Some(rest.trim_start_matches('!').trim_start())
        } else {
            None
        }
    }

    /// Strip Rust visibility and qualifiers: `pub(crate) async unsafe fn`.
    fn strip_rust_prefixes(mut rest: &str) -> Option<(&str, Option<(&'static str, String)>)> {
        loop {
            let before = rest;
            if let Some(r) = rest.strip_prefix("pub") {
                if r.starts_with('(') {
                    let close = r.find(')')?;
                    rest = r[close + 1..].trim_start();
                } else if r.starts_with(char::is_whitespace) {
                    rest = r.trim_start();
                }
            }
            for qualifier in ["async", "unsafe", "default"] {
                if let Some(r) = after_keyword(rest, qualifier) {
                    rest = r;
                }
            }
            if let Some(r) = after_keyword(rest, "extern") {
                let r = r.trim_start_matches(|c: char| c == '"' || c.is_alphanumeric() || c == '-');
                rest = r.trim_start();
            }
            if let Some(r) = after_keyword(rest, "const") {
                // `const fn` is a qualifier; `const NAME` is the definition.
                if r.starts_with("fn") || r.starts_with("unsafe") || r.starts_with("async") {
                    rest = r;
                } else {
                    return Some((rest, Some(("const", identifier(r)?))));
                }
            }
            if rest == before {
                return Some((rest, None));
            }
        }
    }

    /// The definition a line introduces, if any.
    pub fn definition(language: Language, line: &str) -> Option<(&'static str, String)> {
        let line = line.trim();
        match language {
            Language::Rust => {
                let (rest, constant) = strip_rust_prefixes(line)?;
                if constant.is_some() {
                    return constant;
                }
                for (keyword, kind) in [
                    ("fn", "fn"),
                    ("struct", "struct"),
                    ("enum", "enum"),
                    ("trait", "trait"),
                    ("type", "type"),
                    ("mod", "mod"),
                    ("static", "static"),
                    ("macro_rules", "macro"),
                    ("union", "union"),
                ] {
                    if let Some(after) = after_keyword(rest, keyword) {
                        return Some((kind, identifier(after)?));
                    }
                }
                // `impl<T> Name` has no space after the keyword.
                let impl_rest = after_keyword(rest, "impl")
                    .or_else(|| rest.strip_prefix("impl").filter(|r| r.starts_with('<')));
                if let Some(after) = impl_rest {
                    let after = skip_generics(after);
                    let target = match after.find(" for ") {
                        Some(at) => &after[at + 5..],
                        None => after,
                    };
                    return Some(("impl", identifier(target.trim_start_matches(['&', '*']))?));
                }
                None
            }
            Language::Python => {
                let rest = after_keyword(line, "async").unwrap_or(line);
                if let Some(after) = after_keyword(rest, "def") {
                    return Some(("def", identifier(after)?));
                }
                if let Some(after) = after_keyword(rest, "class") {
                    return Some(("class", identifier(after)?));
                }
                None
            }
            Language::JavaScript => {
                let mut rest = line;
                for prefix in ["export default", "export", "declare", "async"] {
                    if let Some(r) = after_keyword(rest, prefix) {
                        rest = r;
                    }
                }
                if let Some(after) = after_keyword(rest, "function") {
                    let after = after.trim_start_matches('*').trim_start();
                    return Some(("function", identifier(after)?));
                }
                for (keyword, kind) in [
                    ("class", "class"),
                    ("interface", "interface"),
                    ("type", "type"),
                    ("enum", "enum"),
                ] {
                    if let Some(after) = after_keyword(rest, keyword) {
                        return Some((kind, identifier(after)?));
                    }
                }
                for keyword in ["const", "let", "var"] {
                    if let Some(after) = after_keyword(rest, keyword) {
                        let name = identifier(after)?;
                        // A binding of a function or class is a definition
                        // worth listing; `const x = 3` is not.
                        let value = after[name.len()..]
                            .split_once('=')
                            .map(|(_, v)| v.trim_start())
                            .unwrap_or("");
                        let callable = value.starts_with("function")
                            || value.starts_with("async")
                            || value.starts_with("class")
                            || value.starts_with('(')
                            || value.contains("=>");
                        return callable.then_some((keyword, name));
                    }
                }
                None
            }
            Language::Go => {
                if let Some(after) = after_keyword(line, "func") {
                    let after = if after.starts_with('(') {
                        after.find(')').map(|at| after[at + 1..].trim_start())?
                    } else {
                        after
                    };
                    return Some(("func", identifier(after)?));
                }
                if let Some(after) = after_keyword(line, "type") {
                    return Some(("type", identifier(after)?));
                }
                None
            }
            Language::Shell => {
                if let Some(after) = after_keyword(line, "function") {
                    return Some(("function", identifier(after)?));
                }
                let name = identifier(line)?;
                let rest = line[name.len()..].trim_start();
                rest.starts_with("()").then_some(("function", name))
            }
            Language::Markdown => {
                let hashes = line.chars().take_while(|c| *c == '#').count();
                if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
                    let title = line[hashes..].trim();
                    return (!title.is_empty()).then(|| ("heading", title.to_string()));
                }
                None
            }
        }
    }

    fn skip_generics(text: &str) -> &str {
        if !text.starts_with('<') {
            return text;
        }
        let mut depth = 0usize;
        for (at, ch) in text.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return text[at + 1..].trim_start();
                    }
                }
                _ => {}
            }
        }
        text
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn def(language: Language, line: &str) -> Option<(&'static str, String)> {
            definition(language, line)
        }

        #[test]
        fn rust_definitions_by_convention() {
            assert_eq!(
                def(Language::Rust, "pub fn render(&self) -> String {"),
                Some(("fn", "render".into()))
            );
            assert_eq!(
                def(Language::Rust, "    pub(crate) async fn fetch() {"),
                Some(("fn", "fetch".into()))
            );
            assert_eq!(
                def(Language::Rust, "pub struct Query {"),
                Some(("struct", "Query".into()))
            );
            assert_eq!(
                def(Language::Rust, "impl<'a> Iterator for Rows<'a> {"),
                Some(("impl", "Rows".into()))
            );
            assert_eq!(
                def(Language::Rust, "impl BacklogPanel {"),
                Some(("impl", "BacklogPanel".into()))
            );
            assert_eq!(
                def(Language::Rust, "pub const VISIBLE_ROWS: i32 = 6;"),
                Some(("const", "VISIBLE_ROWS".into()))
            );
            assert_eq!(
                def(Language::Rust, "const fn zero() -> u8 { 0 }"),
                Some(("fn", "zero".into()))
            );
            assert_eq!(
                def(Language::Rust, "macro_rules! tell {"),
                Some(("macro", "tell".into()))
            );
            assert_eq!(def(Language::Rust, "let fnord = 3;"), None);
            assert_eq!(def(Language::Rust, "// fn not_a_definition()"), None);
        }

        #[test]
        fn other_languages() {
            assert_eq!(
                def(Language::Python, "    async def handle(self):"),
                Some(("def", "handle".into()))
            );
            assert_eq!(
                def(Language::Python, "class Foo(Bar):"),
                Some(("class", "Foo".into()))
            );
            assert_eq!(
                def(
                    Language::JavaScript,
                    "export default async function main() {"
                ),
                Some(("function", "main".into()))
            );
            assert_eq!(
                def(Language::JavaScript, "const handler = async (req) => {"),
                Some(("const", "handler".into()))
            );
            assert_eq!(def(Language::JavaScript, "const limit = 3;"), None);
            assert_eq!(
                def(Language::Go, "func (s *Server) Serve() error {"),
                Some(("func", "Serve".into()))
            );
            assert_eq!(
                def(Language::Shell, "shoot() {"),
                Some(("function", "shoot".into()))
            );
            assert_eq!(
                def(Language::Markdown, "## The philosophy, in five rules"),
                Some(("heading", "The philosophy, in five rules".into()))
            );
            assert_eq!(def(Language::Markdown, "#hashtag"), None);
        }

        #[test]
        fn find_puts_exact_names_first() {
            let symbol = |name: &str| Symbol {
                path: "a.rs".into(),
                line: 1,
                kind: "fn",
                name: name.into(),
                text: String::new(),
            };
            let symbols = vec![symbol("render_all"), symbol("render")];
            let hits = find(&symbols, &Query::new("render"));
            assert_eq!(
                hits.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                ["render", "render_all"]
            );
            assert!(find(&symbols, &Query::new("")).is_empty());
        }
    }
}

/// Search the workspace by walking it. Returns at most `max_hits` hits.
pub fn search(root: &Path, query: &str, max_hits: usize) -> Vec<SearchHit> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let walk = ignore::WalkBuilder::new(root).hidden(false).build();
    for entry in walk.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path
            .strip_prefix(root)
            .map(|r| r.starts_with(".git"))
            .unwrap_or(false)
        {
            continue;
        }
        if !match_file(path, &query, &mut hits, max_hits) {
            break;
        }
    }
    hits
}

/// Build the search index: the workspace's searchable file list, reported
/// incrementally via `progress` (call count grows monotonically).
pub fn collect_files(root: &Path, mut progress: impl FnMut(usize)) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walk = ignore::WalkBuilder::new(root).hidden(false).build();
    for entry in walk.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path
            .strip_prefix(root)
            .map(|r| r.starts_with(".git"))
            .unwrap_or(false)
        {
            continue;
        }
        files.push(path.to_path_buf());
        if files.len() % 128 == 0 {
            progress(files.len());
        }
    }
    progress(files.len());
    files
}

/// Search against a prebuilt index — skips the walk entirely.
pub fn search_files(files: &[PathBuf], query: &str, max_hits: usize) -> Vec<SearchHit> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for path in files {
        if !match_file(path, &query, &mut hits, max_hits) {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_is_smart_case_and_marks_its_matches() {
        let q = Query::new("  gauge ");
        assert!(q.matches("The Gauge module"));
        assert!(!q.case_sensitive());
        assert_eq!(q.ranges("gauge or GAUGE"), vec![(0, 5), (9, 14)]);
        assert_eq!(
            q.highlight_markup("a <gauge> & more"),
            "a &lt;<b>gauge</b>&gt; &amp; more"
        );
        let sensitive = Query::new("Gauge");
        assert!(sensitive.case_sensitive());
        assert!(!sensitive.matches("gauge"));
        assert!(Query::new("").matches("anything"));
        assert!(Query::new("").ranges("x").is_empty());
        assert_eq!(Query::new("straße").ranges("STRASSE Straße"), vec![(8, 15)]);
    }

    #[test]
    fn text_in_memory_is_searched_like_a_file() {
        let (count, hits) = search_text("a\nneedle\nb\nNEEDLE", &Query::new("needle"), 1);
        assert_eq!(count, 2);
        assert_eq!(hits, vec![(2, "needle".to_string())]);
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // .gitignore only applies inside a git repo.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {\n    needle();\n}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.rs"), "// NEEDLE in caps\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "needle but ignored\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 159, 110, 101, 101]).unwrap();
        dir
    }

    /// The tree's counts come from here, and a count is a count: every
    /// file is read to the end, the text kept is bounded per file, and a
    /// raised flag stops the run instead of finishing it.
    #[test]
    fn complete_counts_are_complete_and_bounded_only_in_text() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("many.rs"), "x\n".repeat(5)).unwrap();
        std::fs::write(dir.path().join("one.rs"), "y\nx\n").unwrap();
        let files = vec![dir.path().join("many.rs"), dir.path().join("one.rs")];
        let cancel = AtomicBool::new(false);
        let got = search_files_complete(&files, "x", 2, &cancel).unwrap();
        assert_eq!(got.len(), 2);
        assert!(
            search_files_complete(&files, "X", 2, &cancel)
                .unwrap()
                .is_empty(),
            "an uppercase needle is case-sensitive"
        );
        assert_eq!((got[0].count, got[0].hits.len()), (5, 2));
        assert_eq!((got[1].count, got[1].hits.len()), (1, 1));
        assert_eq!(got[1].hits[0].line, 2);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(search_files_complete(&files, "x", 2, &cancel), None);
    }

    #[test]
    fn finds_case_insensitive_matches_across_files() {
        let dir = workspace();
        let hits = search(dir.path(), "needle", 100);
        let mut files: Vec<String> = hits
            .iter()
            .map(|h| h.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(files, vec!["a.rs", "c.rs"]);
        let a = hits.iter().find(|h| h.path.ends_with("a.rs")).unwrap();
        assert_eq!(a.line, 2);
        assert_eq!(a.text, "needle();");
    }

    #[test]
    fn respects_gitignore_and_skips_binaries() {
        let dir = workspace();
        let hits = search(dir.path(), "needle", 100);
        assert!(!hits.iter().any(|h| h.path.ends_with("ignored.txt")));
        assert!(!hits.iter().any(|h| h.path.ends_with("bin.dat")));
    }

    #[test]
    fn caps_results() {
        let dir = tempfile::tempdir().unwrap();
        let many = "hit\n".repeat(50);
        std::fs::write(dir.path().join("many.txt"), many).unwrap();
        assert_eq!(search(dir.path(), "hit", 10).len(), 10);
    }

    #[test]
    fn indexed_search_matches_walked_search() {
        let dir = workspace();
        let files = collect_files(dir.path(), |_| {});
        assert!(files.len() >= 3);
        let walked = search(dir.path(), "needle", 100);
        let indexed = search_files(&files, "needle", 100);
        assert_eq!(walked.len(), indexed.len());
    }

    #[test]
    fn empty_query_is_empty() {
        let dir = workspace();
        assert!(search(dir.path(), "", 100).is_empty());
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-core perf_ -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_search_large_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let body = "fn quite_ordinary_function() { let value = 42; }\n".repeat(40); // ~2KB
        for i in 0..1000 {
            let sub = dir.path().join(format!("mod{}", i % 25));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("file{i}.rs")), &body).unwrap();
        }
        let start = std::time::Instant::now();
        let hits = search(dir.path(), "ordinary_function", 200);
        println!(
            "search: 1000 files (~2MB) → {} hits (capped) in {:?}",
            hits.len(),
            start.elapsed()
        );
    }
}
