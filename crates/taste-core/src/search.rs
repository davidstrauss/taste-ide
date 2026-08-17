//! Find-in-project: case-insensitive substring search over the workspace,
//! honoring .gitignore, skipping binaries. Deliberately simple — a fast,
//! predictable default rather than a query language.

use std::path::{Path, PathBuf};

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
