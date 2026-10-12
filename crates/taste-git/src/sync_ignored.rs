//! Ignored paths that sync anyway: the `taste-sync` attribute.
//!
//! The folder mirrors Personal's checkout less what is ignored
//! (`crate::mirror`), and what is ignored is decluttering, not a lock. A
//! path git ignores but whose `taste-sync` attribute is set is carried both
//! ways all the same: the checkout's snapshot takes it, the folder's working
//! copy counts it, and the mirror writes it and sends it (David, 2026-10-11:
//! "forces bidirectional sync *despite* being ignored").
//!
//! **It is a git attribute** because `.gitattributes` is where git keeps
//! per-path facts that tools act on and git itself does not (Git LFS's
//! `filter=lfs`, Linguist's `linguist-generated`, `export-ignore`): it is
//! committed, nests by directory, takes globs, and — unlike a marker file
//! inside an ignored directory — never has to be un-ignored itself. No other
//! tool reads `taste-sync`. The file tree's Sync Ignored Files writes one
//! line per path to the root `.gitattributes` ([`with_path`],
//! [`without_path`]): `/dir/** taste-sync` for a directory, `/file
//! taste-sync` for a file; any other line setting it, written by hand or by
//! an agent, counts the same.
//!
//! Who decides, where: in the VM, git itself (`:(attr:taste-sync)`
//! pathspecs in the snapshot script, [`SNAPSHOT_LINES`]); on this host,
//! [`Rules`], which reads the same lines with gitignore's matcher, since
//! the rules there sometimes come from a tree not yet in the folder, which
//! libgit2's attribute lookup cannot read. A test holds the two to the same
//! answers. Neither walks every ignored file: each line's literal directory
//! bounds the walk ([`root_of`]), so `build/keep/** taste-sync` reads
//! `build/keep` and not the rest of `build`. A line with no directory in it
//! (`*.onnx taste-sync`) applies at any depth, so it walks every ignored
//! file under its `.gitattributes`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;

/// The attribute's name.
pub const ATTR: &str = "taste-sync";

/// The most ignored files the attribute adds to one working copy: a line
/// that names a build directory is a lot of files, and a snapshot that
/// reads a million of them is a stalled sync.
pub const MAX_SYNCED_FILES: usize = 50_000;

/// The comment the IDE writes above its first line in `.gitattributes`.
pub const HEADER: &str = "# Ignored paths that sync between the folder and Personal (taste-ide)";

/// What one `.gitattributes` line says about [`ATTR`]: `Some(true)` set,
/// `Some(false)` unset, unspecified, or given a value, `None` silent. The
/// last mention on the line wins, as in git.
fn says(attrs: &[&str]) -> Option<bool> {
    let mut said = None;
    for attr in attrs {
        if *attr == ATTR {
            said = Some(true);
        } else if attr.strip_prefix(['-', '!']) == Some(ATTR)
            || attr
                .strip_prefix(ATTR)
                .is_some_and(|rest| rest.starts_with('='))
        {
            said = Some(false);
        }
    }
    said
}

/// A line's pattern as written (quotes and all) and its attributes; `None`
/// for a blank line, a comment, or a macro definition.
fn split_line(line: &str) -> Option<(&str, Vec<&str>)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') || line.starts_with("[attr]") {
        return None;
    }
    let end = if line.starts_with('"') {
        // The closing quote: the first one not escaped.
        let bytes = line.as_bytes();
        let mut i = 1;
        loop {
            match bytes.get(i) {
                None => return None,
                Some(b'\\') => i += 2,
                Some(b'"') => break i + 1,
                Some(_) => i += 1,
            }
        }
    } else {
        line.find([' ', '\t']).unwrap_or(line.len())
    };
    let (pattern, rest) = line.split_at(end);
    Some((pattern, rest.split_whitespace().collect()))
}

/// A pattern as git reads it: a C-quoted one unquoted (`\"`, `\\`, `\t`,
/// `\n`, and octal), anything else as written.
fn unquote(pattern: &str) -> String {
    let Some(inner) = pattern.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return pattern.to_string();
    };
    let mut out: Vec<u8> = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = bytes[i + 1];
        match next {
            b'0'..=b'7' => {
                let digits: Vec<u8> = bytes[i + 1..]
                    .iter()
                    .take(3)
                    .take_while(|b| (b'0'..=b'7').contains(b))
                    .copied()
                    .collect();
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                out.push(value as u8);
                i += 1 + digits.len();
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            other => {
                out.push(other);
                i += 2;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The pattern the file tree writes for `path` (relative to the root):
/// anchored, git's glob characters escaped so it names exactly that path,
/// `/**` after a directory, and C-quoted when it has a space, a tab, or a
/// quote in it.
pub fn pattern(path: &Path, is_dir: bool) -> String {
    let mut glob = String::from("/");
    for c in path.to_string_lossy().trim_end_matches('/').chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            glob.push('\\');
        }
        glob.push(c);
    }
    if is_dir {
        glob.push_str("/**");
    }
    if !glob.contains([' ', '\t', '"']) {
        return glob;
    }
    let mut quoted = String::from("\"");
    for c in glob.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn own_line(path: &Path, is_dir: bool) -> String {
    format!("{} {ATTR}", pattern(path, is_dir))
}

/// `.gitattributes` text with `path`'s line in it, under [`HEADER`]. Text
/// that already has the line comes back as it is.
pub fn with_path(text: &str, path: &Path, is_dir: bool) -> String {
    let line = own_line(path, is_dir);
    if text.lines().any(|l| l.trim() == line) {
        return text.to_string();
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.lines().any(|l| l == HEADER) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(HEADER);
        out.push('\n');
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// `.gitattributes` text without `path`'s line, and without [`HEADER`]
/// (and the blank line before it) once no line mentions the attribute.
/// Empty when nothing else was there.
pub fn without_path(text: &str, path: &Path, is_dir: bool) -> String {
    let line = own_line(path, is_dir);
    let mut lines: Vec<&str> = text.lines().filter(|l| l.trim() != line).collect();
    let mentioned = lines.iter().any(|l| {
        split_line(l)
            .map(|(_, attrs)| says(&attrs).is_some())
            .unwrap_or(false)
    });
    if !mentioned {
        if let Some(at) = lines.iter().position(|l| *l == HEADER) {
            lines.remove(at);
            if at > 0 && lines.get(at - 1).is_some_and(|l| l.trim().is_empty()) {
                lines.remove(at - 1);
            }
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    lines.join("\n") + "\n"
}

/// The paths a `.gitattributes` names in the file tree's own form —
/// exactly [`pattern`] and [`ATTR`] — which is what its toggle shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toggled {
    pub dirs: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
}

impl Toggled {
    /// Read from the root `.gitattributes`.
    pub fn from_text(text: &str) -> Self {
        let mut toggled = Toggled::default();
        for line in text.lines() {
            let Some((raw, attrs)) = split_line(line) else {
                continue;
            };
            if attrs != [ATTR] {
                continue;
            }
            let glob = unquote(raw);
            let Some(glob) = glob.strip_prefix('/') else {
                continue;
            };
            let (body, is_dir) = match glob.strip_suffix("/**") {
                Some(body) => (body, true),
                None => (glob, false),
            };
            // Escapes undone; a glob left in it is not a toggle's line.
            let mut path = String::new();
            let mut chars = body.chars();
            let mut literal = true;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => path.extend(chars.next()),
                    '*' | '?' | '[' => literal = false,
                    c => path.push(c),
                }
            }
            if !literal || path.is_empty() {
                continue;
            }
            if is_dir {
                toggled.dirs.insert(PathBuf::from(path));
            } else {
                toggled.files.insert(PathBuf::from(path));
            }
        }
        toggled
    }

    /// Whether `path` has its own line.
    pub fn has(&self, path: &Path, is_dir: bool) -> bool {
        if is_dir {
            self.dirs.contains(path)
        } else {
            self.files.contains(path)
        }
    }

    /// Whether a directory above `path` has a line, which syncs it already.
    pub fn covers(&self, path: &Path) -> bool {
        path.ancestors().skip(1).any(|dir| self.dirs.contains(dir))
    }
}

/// Where a walk for a line's pattern starts, relative to the root: the
/// pattern's literal directory under `base` (the `.gitattributes`'s own
/// directory) — `base` itself when the pattern applies at any depth, is
/// quoted, or globs from its first component. Never narrower than git's
/// match, so a wrong guess costs a longer walk and never a missed file.
/// [`ROOTS_AWK`] is the same rule for the VM's shell.
pub fn root_of(base: &Path, raw: &str) -> PathBuf {
    if raw.starts_with('"') {
        return base.to_path_buf();
    }
    // No slash but a trailing one: it matches at any depth.
    if !raw.trim_end_matches('/').contains('/') {
        return base.to_path_buf();
    }
    let body = raw.strip_prefix('/').unwrap_or(raw);
    let literal = match body.find(['*', '?', '[', '\\']) {
        None => body,
        Some(at) => match body[..at].rfind('/') {
            Some(slash) => &body[..slash],
            None => "",
        },
    };
    let literal = literal.trim_end_matches('/');
    if literal.is_empty() {
        base.to_path_buf()
    } else {
        base.join(literal)
    }
}

/// The awk program that prints [`root_of`] for each line of one
/// `.gitattributes` that sets [`ATTR`]; `base` is its directory.
pub const ROOTS_AWK: &str = r#"/^[ \t]*(#|\[attr\])/ { next }
{
  set = 0
  for (i = 2; i <= NF; i++) {
    a = $i
    if (a == "taste-sync") set = 1
    else if (a == "-taste-sync" || a == "!taste-sync" || index(a, "taste-sync=") == 1) set = 0
  }
  if (!set) next
  p = $1
  if (substr(p, 1, 1) == "\"") { print base; next }
  body = p; sub(/\/+$/, "", body)
  if (index(body, "/") == 0) { print base; next }
  sub(/^\//, "", p)
  at = match(p, /[*?[\\]/)
  if (at == 0) lit = p
  else { lit = substr(p, 1, at - 1); if (!sub(/\/[^\/]*$/, "", lit)) lit = "" }
  sub(/\/+$/, "", lit)
  if (base != "" && lit != "") print base "/" lit
  else if (base != "") print base
  else print lit
}
"#;

/// The shell lines a checkout's snapshot script runs after its `git add -A`
/// (`crate::snapshot::script`): every ignored file whose [`ATTR`] is set,
/// added to the scratch index. The roots come from each `.gitattributes`
/// the scratch index holds and `info/attributes`; git matches the
/// attribute itself, reading the working tree's attributes for that one
/// listing — the IDE's git otherwise reads none (`GIT_ATTR_SOURCE`, the
/// keeper's), and a listing runs no filter — while the `add` keeps reading
/// none, so no filter the repository names runs on what it adds.
///
/// What syncs is often a program's live data, which changes under the
/// snapshot, and one file that did failed the whole snapshot, so the folder
/// stopped hearing of anything — a backup written into `data/backups/`
/// never arrived while a database beside it kept its write-ahead log
/// coming and going (2026-10-12). So the files go through `update-index
/// --remove`, which passes over one listed and gone before its turn, as
/// `add` would not (a download's `.partial`, that log). One that changes
/// while it is read stops update-index where it stands; then the files go
/// again in batches, a failing batch file by file, and a file that still
/// will not read keeps the previous snapshot's copy (`$prev`, which the
/// script has read by now): left out, it would read as deleted in the
/// checkout, and go from the folder.
///
/// And they go through an index of their own, kept between snapshots
/// (`taste-synced-index` in the git directory), so a file whose stat is
/// what it was is not read again: the scratch index starts from HEAD every
/// time, and every snapshot read and compressed the whole of a project's
/// data, 12 s for 1.7 GB, several times a sync (2026-10-12). What is no
/// longer listed leaves it first. Each run works on its own copy and puts
/// it back, so two at once do not meet on its lock; the last one's stands,
/// which is only ever a cache.
pub fn snapshot_lines() -> String {
    format!(
        r#"taste_sync_roots='{awk}'
roots="$GIT_INDEX_FILE.roots"
{{ git -c core.quotePath=false ls-files -- ':(glob)**/.gitattributes' | while IFS= read -r attrs; do
    base=${{attrs%.gitattributes}}; base=${{base%/}}
    awk -v base="$base" "$taste_sync_roots" "$attrs"
  done
  if [ -f "$gitdir/info/attributes" ]; then awk -v base= "$taste_sync_roots" "$gitdir/info/attributes"; fi
}} | sort -u > "$roots"
set --
while IFS= read -r root; do set -- "$@" ":(attr:{ATTR},literal)${{root:-.}}"; done < "$roots"
rm -f "$roots"
if [ $# -gt 0 ]; then
  synced="$GIT_INDEX_FILE.synced"
  ( unset GIT_ATTR_SOURCE; git ls-files -z -o -i --exclude-standard -- "$@" ) > "$synced"
  own="$GIT_INDEX_FILE.own"
  cp -p "$gitdir/taste-synced-index" "$own" 2>/dev/null || rm -f "$own"
  LC_ALL=C sort -z "$synced" > "$synced.now"
  GIT_INDEX_FILE="$own" git ls-files -z | LC_ALL=C sort -z > "$synced.had"
  LC_ALL=C comm -z -23 "$synced.had" "$synced.now" \
    | GIT_INDEX_FILE="$own" git update-index -z --force-remove --stdin
  if ! GIT_INDEX_FILE="$own" git update-index --add --remove -z --stdin < "$synced" 2>/dev/null; then
    kept="$GIT_INDEX_FILE.kept"
    GIT_INDEX_FILE="$own" xargs -0 -r -n 64 sh -c 'git update-index --add --remove -- "$@" 2>/dev/null && exit 0
      for f; do git update-index --add --remove -- "$f" 2>/dev/null || printf "%s\0" "$f"; done' sh \
      < "$synced" > "$kept" || true
  fi
  GIT_INDEX_FILE="$own" git ls-files -z -s | git update-index -z --index-info
  if [ -n "$prev" ] && [ -s "${{kept:-/dev/null}}" ]; then
    xargs -0 sh -c 'GIT_LITERAL_PATHSPECS=1 git ls-tree -z "$0" -- "$@"' "$prev" < "$kept" \
      | git update-index -z --index-info
  fi
  mv -f "$own" "$gitdir/taste-synced-index"
fi
"#,
        awk = ROOTS_AWK
    )
}

/// One set of `.gitattributes` files, read as git reads them: the deepest
/// file's say first, the last matching line in a file winning.
#[derive(Debug, Default)]
struct Group {
    /// Each file's directory and its lines that mention [`ATTR`], set lines
    /// as patterns and the rest as negations; deepest first.
    files: Vec<(PathBuf, Gitignore)>,
}

impl Group {
    fn syncs(&self, path: &Path) -> bool {
        for (base, matcher) in &self.files {
            let Ok(rel) = path.strip_prefix(base) else {
                continue;
            };
            match matcher.matched(rel, false) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }
}

/// The attribute's lines from one or more sets of `.gitattributes` — the
/// folder's, and a tree's — and which paths they sync: a path any set syncs.
#[derive(Debug, Default)]
pub struct Rules {
    groups: Vec<Group>,
    roots: BTreeSet<PathBuf>,
}

impl Rules {
    /// One set: each file as its directory (relative, the root as the
    /// empty path; `info/attributes` as the root too, and listed first,
    /// since it outranks every `.gitattributes`) and its text.
    pub fn add_set(&mut self, files: impl IntoIterator<Item = (PathBuf, String)>) {
        let mut found: Vec<(usize, PathBuf, Gitignore)> = Vec::new();
        for (order, (base, text)) in files.into_iter().enumerate() {
            let mut builder = GitignoreBuilder::new("");
            let mut any = false;
            for line in text.lines() {
                let Some((raw, attrs)) = split_line(line) else {
                    continue;
                };
                let Some(set) = says(&attrs) else { continue };
                let glob = unquote(raw);
                if set {
                    self.roots.insert(root_of(&base, raw));
                    let _ = builder.add_line(None, &glob);
                } else {
                    let _ = builder.add_line(None, &format!("!{glob}"));
                }
                any = true;
            }
            if !any {
                continue;
            }
            if let Ok(matcher) = builder.build() {
                found.push((order, base, matcher));
            }
        }
        // Deepest first; among equals (only `info/attributes` beside the
        // root's), the order given.
        found.sort_by(|a, b| {
            b.1.components()
                .count()
                .cmp(&a.1.components().count())
                .then(a.0.cmp(&b.0))
        });
        self.groups.push(Group {
            files: found.into_iter().map(|(_, b, m)| (b, m)).collect(),
        });
    }

    /// Whether any set syncs `path` (relative to the root).
    pub fn syncs(&self, path: &Path) -> bool {
        self.groups.iter().any(|group| group.syncs(path))
    }

    /// Where the walks start.
    pub fn roots(&self) -> &BTreeSet<PathBuf> {
        &self.roots
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

/// The `.gitattributes` files in `tree`, as [`Rules::add_set`] takes them.
pub fn attribute_files_in_tree(repo: &git2::Repository, tree: git2::Oid) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let Ok(tree) = repo.find_tree(tree) else {
        return files;
    };
    let _ = tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.name() == Some(".gitattributes") && entry.kind() == Some(git2::ObjectType::Blob) {
            if let Ok(blob) = repo.find_blob(entry.id()) {
                files.push((
                    PathBuf::from(dir.trim_end_matches('/')),
                    String::from_utf8_lossy(blob.content()).into_owned(),
                ));
            }
        }
        git2::TreeWalkResult::Ok
    });
    files
}

impl crate::GitWorkspace {
    /// This working copy's rules: its `info/attributes` and each
    /// `.gitattributes` it has — the index's, and new ones its status shows.
    pub fn sync_rules(&self) -> Rules {
        let mut found: BTreeSet<PathBuf> = BTreeSet::new();
        if let Ok(index) = self.repo.index() {
            for entry in index.iter() {
                let path = PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned());
                if path.file_name().is_some_and(|n| n == ".gitattributes") {
                    found.insert(path);
                }
            }
        }
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .pathspec(".gitattributes")
            .pathspec("*/.gitattributes");
        if let Ok(statuses) = self.repo.statuses(Some(&mut opts)) {
            for entry in statuses.iter() {
                if let Some(path) = entry.path() {
                    if Path::new(path)
                        .file_name()
                        .is_some_and(|n| n == ".gitattributes")
                    {
                        found.insert(PathBuf::from(path));
                    }
                }
            }
        }
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(self.repo.path().join("info/attributes")) {
            files.push((PathBuf::new(), text));
        }
        for path in found {
            if let Ok(text) = std::fs::read_to_string(self.workdir.join(&path)) {
                files.push((
                    path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    text,
                ));
            }
        }
        let mut rules = Rules::default();
        rules.add_set(files);
        rules
    }

    /// The ignored files `rules` sync, relative: what they add to this
    /// working copy. At most [`MAX_SYNCED_FILES`].
    pub(crate) fn synced_ignored_files(&self, rules: &Rules) -> Vec<PathBuf> {
        if rules.is_empty() {
            return Vec::new();
        }
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .include_ignored(true)
            .recurse_ignored_dirs(true)
            .recurse_untracked_dirs(true)
            .exclude_submodules(true);
        if !rules.roots().contains(Path::new("")) {
            for root in rules.roots() {
                opts.pathspec(escape_pathspec(root));
            }
        }
        let Ok(statuses) = self.repo.statuses(Some(&mut opts)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in statuses.iter() {
            if !entry.status().contains(git2::Status::IGNORED) {
                continue;
            }
            let Some(path) = entry.path() else { continue };
            let path = Path::new(path);
            if !rules.syncs(path) {
                continue;
            }
            out.push(path.to_path_buf());
            if out.len() >= MAX_SYNCED_FILES {
                tracing::warn!(
                    "{}: past {MAX_SYNCED_FILES} ignored files set to {ATTR}; the rest is not \
                     synced",
                    self.workdir.display()
                );
                break;
            }
        }
        out
    }

    /// Whether `path` is ignored and not synced anyway: what the mirror
    /// leaves alone.
    pub(crate) fn ignored_unsynced(&self, path: &Path, rules: &Rules) -> bool {
        self.repo.is_path_ignored(path).unwrap_or(true) && !rules.syncs(path)
    }
}

/// A path as a libgit2 pathspec that matches it, and what is under it,
/// literally.
fn escape_pathspec(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(files: &[(&str, &str)]) -> Rules {
        let mut rules = Rules::default();
        rules.add_set(
            files
                .iter()
                .map(|(base, text)| (PathBuf::from(base), text.to_string())),
        );
        rules
    }

    /// The file tree's lines go on under one header and come off again,
    /// leaving the project's own lines as they were; the paths they name
    /// read back as toggled, quoted and escaped ones included.
    #[test]
    fn the_file_trees_lines_come_and_go() {
        let own = "*.png binary";
        let one = with_path(own, Path::new("data/keep"), true);
        assert_eq!(
            one,
            format!("*.png binary\n\n{HEADER}\n/data/keep/** taste-sync\n")
        );
        let two = with_path(&one, Path::new("my dir/a*b.bin"), false);
        assert!(
            two.ends_with("\"/my dir/a\\\\*b.bin\" taste-sync\n"),
            "{two}"
        );
        assert_eq!(with_path(&two, Path::new("data/keep"), true), two);
        let toggled = Toggled::from_text(&two);
        assert!(toggled.has(Path::new("data/keep"), true));
        assert!(toggled.has(Path::new("my dir/a*b.bin"), false));
        assert!(!toggled.has(Path::new("data/keep"), false));
        assert!(toggled.covers(Path::new("data/keep/deep/x")));
        assert!(!toggled.covers(Path::new("data/keep")));
        assert_eq!(without_path(&two, Path::new("my dir/a*b.bin"), false), one);
        assert_eq!(
            without_path(&one, Path::new("data/keep"), true),
            "*.png binary\n"
        );
        assert_eq!(
            without_path(&with_path("", Path::new("x"), false), Path::new("x"), false),
            ""
        );
    }

    #[test]
    fn a_walk_starts_at_a_patterns_literal_directory() {
        let base = Path::new("");
        assert_eq!(root_of(base, "/data/keep/**"), PathBuf::from("data/keep"));
        assert_eq!(root_of(base, "/x.log"), PathBuf::from("x.log"));
        assert_eq!(root_of(base, "/data/a\\*b"), PathBuf::from("data"));
        assert_eq!(root_of(base, "*.onnx"), PathBuf::new());
        assert_eq!(root_of(base, "build/"), PathBuf::new());
        assert_eq!(root_of(base, "**/w.bin"), PathBuf::new());
        assert_eq!(root_of(base, "\"/my dir/**\""), PathBuf::new());
        assert_eq!(
            root_of(Path::new("sub"), "/out/**"),
            PathBuf::from("sub/out")
        );
        assert_eq!(root_of(Path::new("sub"), "*.bin"), PathBuf::from("sub"));
    }

    /// The VM's awk and this host's [`root_of`] start the same walks.
    #[test]
    fn the_shells_roots_are_root_of() {
        if std::process::Command::new("awk")
            .arg("BEGIN{}")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no awk");
            return;
        }
        let text =
            "# comment\n[attr]mine taste-sync\n/data/keep/** taste-sync\n/x.log taste-sync\n\
                    /data/a\\*b taste-sync\n*.onnx taste-sync\nbuild/ taste-sync\n\
                    \"/my dir/**\" taste-sync\n/off/** -taste-sync\n/v/** taste-sync=1\n\
                    /both/** taste-sync -taste-sync taste-sync\n**/w.bin taste-sync\n";
        for base in ["", "sub"] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(".gitattributes");
            std::fs::write(&file, text).unwrap();
            let out = std::process::Command::new("awk")
                .args(["-v", &format!("base={base}"), ROOTS_AWK])
                .arg(&file)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let awk: BTreeSet<PathBuf> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(PathBuf::from)
                .collect();
            let ours = rules(&[(base, text)]);
            assert_eq!(&awk, ours.roots(), "base {base:?}");
        }
    }

    /// [`Rules`] answers as `git check-attr` does: nesting, the last line
    /// winning, unset and valued lines, quoted and escaped patterns, and a
    /// directory pattern that does not reach inside.
    #[test]
    fn the_rules_answer_as_git_does() {
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
        git2::Repository::init(root).unwrap();
        let top = "/data/keep/** taste-sync\n/x.log taste-sync\n/data/a\\*b taste-sync\n\
                   *.onnx taste-sync\n\"/my dir/**\" taste-sync\n/data/keep/no/** -taste-sync\n\
                   /v/** taste-sync=1\nbuild/ taste-sync\n";
        let sub = "*.onnx -taste-sync\n/out/** taste-sync\n";
        std::fs::write(root.join(".gitattributes"), top).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.gitattributes"), sub).unwrap();
        let ours = rules(&[("", top), ("sub", sub)]);
        let paths = [
            "data/keep/a",
            "data/keep/deep/b",
            "data/keep/no/c",
            "data/other",
            "data/a*b",
            "data/aXb",
            "x.log",
            "y/x.log",
            "m.onnx",
            "deep/m.onnx",
            "sub/m.onnx",
            "sub/out/z",
            "my dir/f",
            "v/x",
            "build/x",
        ];
        let out = std::process::Command::new("git")
            .current_dir(root)
            .args(["-c", "core.quotePath=false", "check-attr", ATTR, "--"])
            .args(paths)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        for (path, line) in paths.iter().zip(said.lines()) {
            let git = line.ends_with(": set");
            assert_eq!(ours.syncs(Path::new(path)), git, "{path}: git says {line}");
        }
    }

    /// The working copy's own rules, new `.gitattributes` included, and the
    /// ignored files they add: only those set, and nothing else ignored.
    #[test]
    fn a_working_copy_lists_the_ignored_files_it_syncs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "data/\nsub/out/\n").unwrap();
        std::fs::write(root.join(".gitattributes"), "/data/keep/** taste-sync\n").unwrap();
        std::fs::create_dir_all(root.join("data/keep/deep")).unwrap();
        std::fs::create_dir_all(root.join("sub/out")).unwrap();
        std::fs::write(root.join("sub/.gitattributes"), "/out/*.bin taste-sync\n").unwrap();
        for file in [
            "data/keep/a",
            "data/keep/deep/b",
            "data/other",
            "sub/out/w.bin",
            "sub/out/x.o",
        ] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        let ws = crate::GitWorkspace::discover(root).unwrap();
        let rules = ws.sync_rules();
        let mut files = ws.synced_ignored_files(&rules);
        files.sort();
        assert_eq!(
            files,
            ["data/keep/a", "data/keep/deep/b", "sub/out/w.bin"].map(PathBuf::from)
        );
        assert!(ws.ignored_unsynced(Path::new("data/other"), &rules));
        assert!(!ws.ignored_unsynced(Path::new("data/keep/a"), &rules));
        assert!(!ws.ignored_unsynced(Path::new("sub/out/w.bin"), &rules));
    }
}
