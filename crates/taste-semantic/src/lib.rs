//! Semantic search over a checkout: the meaning of a question against the
//! meaning of the code, for the agents' "where is authentication handled?"
//! — the question `ide_search` cannot answer because no line says
//! "authentication" (docs/spikes/agent-workspace-context.md).
//!
//! Local, like everything else the IDE computes: one pinned embedding model
//! ([`EMBEDDING`], fetched once by `taste-models`), run through llama.cpp on
//! the CPU — in a process of its own, `taste-embed`, because the IDE
//! already links whisper.cpp for voice and the two carry ggmls that cannot
//! share a binary. The index is per checkout, on disk under the workspace's
//! state directory, and incremental: a file is re-embedded only when its
//! content hash changes. Nothing here has a GTK type; the app drives it off
//! the main thread and the MCP server queries it.
//!
//! Cost, honestly: the first build of a 60k-line repository is a few
//! minutes of one CPU's worth of threads in the background; a query is a
//! few milliseconds plus one embedding.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

pub use taste_models::ModelSpec;

/// The embedding model: nomic-embed-text v1.5, the maintainers' own GGUF
/// at Q5_K_M — 100 MB, 768 dimensions, an 8k-token window, Apache-2.0,
/// trained on code as well as prose, and the `search_document:` /
/// `search_query:` prefixes below are its documented contract. Pinned by
/// digest like the speech model; a smaller or a code-specialised model is
/// a second constant, not a setting. Digest computed from a real download
/// on 2026-09-07.
pub const EMBEDDING: ModelSpec = ModelSpec {
    name: "nomic-embed-text-v1.5 (Q5_K_M)",
    file: "nomic-embed-text-v1.5.Q5_K_M.gguf",
    url: "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q5_K_M.gguf",
    sha256: "0c7930f6c4f6f29b7da5046e3a2c0832aa3f602db3de5760a95f0582dbd3d6e6",
    bytes: 99_588_928,
};

/// A chunk is a window of lines: long enough to carry a function's shape,
/// short enough that a hit points somewhere. Overlapping by a quarter so a
/// definition straddling a boundary is whole in one of them.
pub const CHUNK_LINES: usize = 40;
pub const CHUNK_STRIDE: usize = 30;
/// A line longer than this is a minified or generated one; it is cut, not
/// embedded whole.
const MAX_LINE_CHARS: usize = 240;
/// A file bigger than this is a lock file, a fixture or a build product,
/// and its meaning is not what anyone is asking after.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// The on-disk format; a mismatch rebuilds rather than migrates (alpha).
const FORMAT: u32 = 1;
const MAGIC: &[u8; 4] = b"TSEM";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// Cut a file into chunks. Windows of `CHUNK_LINES` every `CHUNK_STRIDE`
/// lines; a window that is only whitespace is skipped; the last window
/// always reaches the end.
pub fn chunk(text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    if lines.is_empty() {
        return out;
    }
    let mut start = 0usize;
    loop {
        let end = (start + CHUNK_LINES).min(lines.len());
        let body: String = lines[start..end]
            .iter()
            .map(|line| {
                if line.chars().count() > MAX_LINE_CHARS {
                    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                    format!("{cut}…")
                } else {
                    (*line).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !body.trim().is_empty() {
            out.push(Chunk {
                start_line: start as u32 + 1,
                end_line: end as u32,
                text: body,
            });
        }
        if end >= lines.len() {
            break;
        }
        start += CHUNK_STRIDE;
    }
    out
}

/// Whether a file is worth embedding: text, not too big, valid UTF-8.
pub fn indexable(bytes: &[u8]) -> bool {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return false;
    }
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return false;
    }
    std::str::from_utf8(bytes).is_ok()
}

fn file_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// --- the embedder ----------------------------------------------------------

/// Where the helper is: beside this executable (`target/debug` in
/// development, the install prefix's bin otherwise), or wherever
/// `TASTE_EMBED_BIN` names — which is how the tests find a freshly built
/// one.
pub fn helper_path() -> Result<PathBuf> {
    if let Some(named) = std::env::var_os("TASTE_EMBED_BIN") {
        return Ok(PathBuf::from(named));
    }
    let exe = std::env::current_exe().context("finding this executable")?;
    exe.parent()
        .map(|dir| dir.join("taste-embed"))
        .filter(|path| path.is_file())
        .context("taste-embed is not installed beside the IDE (TASTE_EMBED_BIN names another)")
}

/// The embedding model, behind the `taste-embed` process: one child, kept
/// for the life of this value, asked over stdio one request at a time. A
/// child that dies is started again once; a second failure is the error.
pub struct Embedder {
    helper: Mutex<Helper>,
    model: PathBuf,
    dim: usize,
}

impl Embedder {
    pub fn load(model: &Path) -> Result<Self> {
        let (helper, dim) = Helper::spawn(model)?;
        Ok(Self {
            helper: Mutex::new(helper),
            model: model.to_path_buf(),
            dim,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Documents, with the model's document prefix (the helper adds it).
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.ask("document", texts)
    }

    /// A query, with the model's query prefix.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut out = self.ask("query", &[query.trim().to_string()])?;
        out.pop().context("no embedding came back")
    }

    fn ask(&self, kind: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut helper = self.helper.lock().unwrap_or_else(|e| e.into_inner());
        match helper.ask(kind, texts) {
            Ok(vectors) => Ok(vectors),
            Err(first) => {
                let (fresh, _) = Helper::spawn(&self.model)
                    .with_context(|| format!("restarting the embedding helper after: {first:#}"))?;
                *helper = fresh;
                helper.ask(kind, texts)
            }
        }
    }
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    dim: usize,
    #[serde(default)]
    vectors: Vec<Vec<f32>>,
    #[serde(default)]
    error: Option<String>,
}

impl Helper {
    fn spawn(model: &Path) -> Result<(Self, usize)> {
        let path = helper_path()?;
        let mut child = Command::new(&path)
            .arg(model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // llama.cpp's narration; errors come back as JSON.
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", path.display()))?;
        let stdin = child.stdin.take().context("the helper has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("the helper has no stdout")?);
        let mut helper = Self {
            child,
            stdin,
            stdout,
        };
        let first = helper
            .read_reply()
            .context("the helper did not say it was ready")?;
        if !first.ready || first.dim == 0 {
            bail!(
                "the embedding helper did not come up: {}",
                first.error.unwrap_or_else(|| "no reason given".into())
            );
        }
        Ok((helper, first.dim))
    }

    fn ask(&mut self, kind: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let request = serde_json::json!({ "kind": kind, "texts": texts });
        writeln!(self.stdin, "{request}").context("writing to the embedding helper")?;
        self.stdin
            .flush()
            .context("flushing to the embedding helper")?;
        let reply = self.read_reply()?;
        if let Some(error) = reply.error {
            bail!("embedding: {error}");
        }
        if reply.vectors.len() != texts.len() {
            bail!(
                "the embedding helper answered {} vectors for {} texts",
                reply.vectors.len(),
                texts.len()
            );
        }
        Ok(reply.vectors)
    }

    fn read_reply(&mut self) -> Result<Reply> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .context("reading from the embedding helper")?;
        if n == 0 {
            bail!("the embedding helper closed its output");
        }
        serde_json::from_str(&line).with_context(|| format!("the helper said: {}", line.trim()))
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Unit length, so similarity is a dot product. The helper normalises what
/// it returns; this is here for anything that builds a vector itself.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

// --- the index -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct FileEntry {
    rel: PathBuf,
    hash: String,
    chunks: Vec<(Chunk, Vec<f32>)>,
}

/// One checkout's index: every indexable file, hashed and chunked, with a
/// vector per chunk. Kept whole in memory (a large repository is a few
/// thousand chunks) and written whole to disk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Index {
    dim: usize,
    files: Vec<FileEntry>,
}

impl Index {
    pub fn files(&self) -> usize {
        self.files.len()
    }

    pub fn chunks(&self) -> usize {
        self.files.iter().map(|f| f.chunks.len()).sum()
    }

    /// Top `limit` chunks by cosine similarity (vectors are unit length,
    /// so a dot product). Two overlapping windows of one file that both
    /// hit are reported once, as the better one.
    pub fn search(&self, query: &[f32], limit: usize) -> Vec<Hit> {
        let mut scored: Vec<Hit> = Vec::new();
        for file in &self.files {
            for (chunk, vector) in &file.chunks {
                let score: f32 = vector.iter().zip(query).map(|(a, b)| a * b).sum();
                scored.push(Hit {
                    path: file.rel.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    text: chunk.text.clone(),
                });
            }
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut kept: Vec<Hit> = Vec::new();
        for hit in scored {
            if kept.len() >= limit {
                break;
            }
            let overlaps = kept.iter().any(|k| {
                k.path == hit.path && k.start_line <= hit.end_line && hit.start_line <= k.end_line
            });
            if !overlaps {
                kept.push(hit);
            }
        }
        kept
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.dim as u32).to_le_bytes());
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for file in &self.files {
            write_str(&mut out, &file.rel.to_string_lossy());
            write_str(&mut out, &file.hash);
            out.extend_from_slice(&(file.chunks.len() as u32).to_le_bytes());
            for (chunk, vector) in &file.chunks {
                out.extend_from_slice(&chunk.start_line.to_le_bytes());
                out.extend_from_slice(&chunk.end_line.to_le_bytes());
                write_str(&mut out, &chunk.text);
                for x in vector {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::File::create(&temp)
            .and_then(|mut f| f.write_all(&out))
            .with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
    }

    fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cursor = Cursor {
            bytes: &bytes,
            at: 0,
        };
        if cursor.take(4)? != MAGIC {
            bail!("{} is not a semantic index", path.display());
        }
        if cursor.u32()? != FORMAT {
            bail!("{} is an older index format; it is rebuilt", path.display());
        }
        let dim = cursor.u32()? as usize;
        let n_files = cursor.u32()? as usize;
        let mut files = Vec::with_capacity(n_files);
        for _ in 0..n_files {
            let rel = PathBuf::from(cursor.string()?);
            let hash = cursor.string()?;
            let n_chunks = cursor.u32()? as usize;
            let mut chunks = Vec::with_capacity(n_chunks);
            for _ in 0..n_chunks {
                let start_line = cursor.u32()?;
                let end_line = cursor.u32()?;
                let text = cursor.string()?;
                let mut vector = Vec::with_capacity(dim);
                for _ in 0..dim {
                    vector.push(cursor.f32()?);
                }
                chunks.push((
                    Chunk {
                        start_line,
                        end_line,
                        text,
                    },
                    vector,
                ));
            }
            files.push(FileEntry { rel, hash, chunks });
        }
        Ok(Self { dim, files })
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len());
        let Some(end) = end else {
            bail!("the index file ends early");
        };
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

/// One answer: where, how well, and the chunk itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// Relative to the checkout.
    pub path: PathBuf,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
    pub text: String,
}

/// How a refresh is going. The plan pass hashes every file first, so
/// `chunks_total` — the chunks that actually need embedding — is known
/// before the slow pass begins, and a remaining time can be estimated
/// from `chunks_embedded` of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub files_done: usize,
    pub files_total: usize,
    pub chunks_embedded: usize,
    pub chunks_total: usize,
}

/// What a refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub files: usize,
    pub chunks: usize,
    /// Chunks embedded this time — zero when nothing changed.
    pub embedded: usize,
    pub removed_files: usize,
    pub cancelled: bool,
}

/// What a query can be told when there is no answer yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The model has not been fetched.
    ModelAbsent,
    /// This checkout has no index yet.
    NotIndexed,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::ModelAbsent => write!(f, "the embedding model has not been fetched yet"),
            Unavailable::NotIndexed => write!(f, "this checkout has not been indexed yet"),
        }
    }
}

impl std::error::Error for Unavailable {}

/// Where a checkout's index lives.
pub fn index_path(root: &Path) -> PathBuf {
    taste_core::state::workspace_state_dir(root)
        .join("semantic")
        .join("index.bin")
}

/// The service: one embedder, an index per checkout, refreshes that never
/// overlap on the same checkout.
pub struct Semantic {
    /// The helper that embeds documents — busy for seconds at a time while
    /// an index builds.
    embedder: Mutex<Option<Arc<Embedder>>>,
    /// A second helper for questions, so a person's query is answered in
    /// milliseconds while a refresh is in the middle of a batch. Started
    /// on the first question, at the cost of a second copy of the model
    /// in memory while both live.
    query_embedder: Mutex<Option<Arc<Embedder>>>,
    indexes: Mutex<HashMap<PathBuf, Arc<Index>>>,
    refreshing: Mutex<HashSet<PathBuf>>,
}

impl Default for Semantic {
    fn default() -> Self {
        Self {
            embedder: Mutex::new(None),
            query_embedder: Mutex::new(None),
            indexes: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
        }
    }
}

impl Semantic {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn model_present() -> bool {
        taste_models::is_present(&EMBEDDING)
    }

    fn embedder(&self) -> Result<Arc<Embedder>> {
        Self::embedder_in(&self.embedder)
    }

    fn query_embedder(&self) -> Result<Arc<Embedder>> {
        Self::embedder_in(&self.query_embedder)
    }

    fn embedder_in(slot: &Mutex<Option<Arc<Embedder>>>) -> Result<Arc<Embedder>> {
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(embedder) = slot.as_ref() {
            return Ok(embedder.clone());
        }
        if !Self::model_present() {
            bail!(Unavailable::ModelAbsent);
        }
        let embedder = Arc::new(Embedder::load(&taste_models::model_path(&EMBEDDING))?);
        *slot = Some(embedder.clone());
        Ok(embedder)
    }

    /// The index for a checkout, from memory or from disk.
    fn index(&self, root: &Path) -> Option<Arc<Index>> {
        if let Some(index) = self
            .indexes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(root)
        {
            return Some(index.clone());
        }
        let loaded = Arc::new(Index::load(&index_path(root)).ok()?);
        self.indexes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.to_path_buf(), loaded.clone());
        Some(loaded)
    }

    /// Whether a checkout has an index, and how big.
    pub fn status(&self, root: &Path) -> Option<(usize, usize)> {
        self.index(root)
            .map(|index| (index.files(), index.chunks()))
    }

    /// Whether a refresh of this checkout is running.
    pub fn refreshing(&self, root: &Path) -> bool {
        self.refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(root)
    }

    /// Bring a checkout's index up to date. Blocking: call it from a
    /// blocking pool. Files whose hash is unchanged keep their vectors;
    /// changed and new files are re-chunked and embedded; files gone from
    /// the checkout leave the index. `cancel` is checked between files, and
    /// a cancelled refresh keeps what it had (the next one finishes the
    /// job). A refresh already running on the same checkout makes this one
    /// return at once with `cancelled: true`.
    pub fn refresh(
        &self,
        root: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> Result<Report> {
        {
            let mut running = self.refreshing.lock().unwrap_or_else(|e| e.into_inner());
            if !running.insert(root.to_path_buf()) {
                return Ok(Report {
                    cancelled: true,
                    ..Report::default()
                });
            }
        }
        let result = self.refresh_inner(root, cancel, &mut progress);
        self.refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(root);
        result
    }

    fn refresh_inner(
        &self,
        root: &Path,
        cancel: &AtomicBool,
        progress: &mut impl FnMut(Progress),
    ) -> Result<Report> {
        let embedder = self.embedder()?;
        let previous = self.index(root).unwrap_or_default();
        let known: HashMap<PathBuf, &FileEntry> =
            previous.files.iter().map(|f| (f.rel.clone(), f)).collect();
        let paths = taste_core::search::collect_files(root, |_| {});

        // Pass one, the plan: hash every file, keep the unchanged ones'
        // vectors, chunk the rest. Cheap, and it makes the slow pass's size
        // known before it starts — which is what a remaining-time estimate
        // is made of.
        let mut files: Vec<FileEntry> = Vec::with_capacity(paths.len());
        let mut pending: Vec<(PathBuf, String, Vec<Chunk>)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut status = Progress {
            files_done: 0,
            files_total: paths.len(),
            chunks_embedded: 0,
            chunks_total: 0,
        };
        for path in &paths {
            status.files_done += 1;
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if !indexable(&bytes) {
                continue;
            }
            let hash = file_hash(&bytes);
            seen.insert(rel.to_path_buf());
            if let Some(entry) = known.get(rel) {
                if entry.hash == hash {
                    files.push((*entry).clone());
                    continue;
                }
            }
            let chunks = chunk(&String::from_utf8_lossy(&bytes));
            if chunks.is_empty() {
                continue;
            }
            status.chunks_total += chunks.len();
            pending.push((rel.to_path_buf(), hash, chunks));
        }
        progress(status);

        // Pass two: embed what changed, a file at a time, checking the stop
        // flag between files.
        let mut report = Report {
            embedded: 0,
            ..Report::default()
        };
        let mut unreached: HashSet<PathBuf> = HashSet::new();
        for (rel, hash, chunks) in pending {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                unreached.insert(rel);
                continue;
            }
            let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
            let vectors = embedder
                .embed_documents(&texts)
                .with_context(|| format!("embedding {}", rel.display()))?;
            report.embedded += vectors.len();
            status.chunks_embedded += vectors.len();
            files.push(FileEntry {
                rel,
                hash,
                chunks: chunks.into_iter().zip(vectors).collect(),
            });
            progress(status);
        }
        if report.cancelled {
            // Keep what the previous index had for the files not reached.
            for entry in &previous.files {
                if unreached.contains(&entry.rel) {
                    files.push(entry.clone());
                }
            }
        } else {
            report.removed_files = previous
                .files
                .iter()
                .filter(|f| !seen.contains(&f.rel))
                .count();
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        let index = Index {
            dim: embedder.dim(),
            files,
        };
        report.files = index.files();
        report.chunks = index.chunks();
        index.save(&index_path(root))?;
        self.indexes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.to_path_buf(), Arc::new(index));
        Ok(report)
    }

    /// Ask a checkout's index. `Unavailable` when there is nothing to ask
    /// yet — the caller decides whether to start a refresh.
    pub fn search(&self, root: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(index) = self.index(root) else {
            bail!(Unavailable::NotIndexed);
        };
        let embedder = self.query_embedder()?;
        let vector = embedder.embed_query(query)?;
        Ok(index.search(&vector, limit.clamp(1, 50)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_overlapping_windows_that_reach_the_end() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let chunks = chunk(&text);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 40);
        assert_eq!(chunks[1].start_line, 31);
        assert_eq!(chunks.last().unwrap().end_line, 100);
        assert!(chunks.last().unwrap().text.ends_with("line 100"));
        assert!(chunk("").is_empty());
        assert!(chunk("\n\n  \n").is_empty(), "whitespace is not a chunk");
    }

    #[test]
    fn a_long_line_is_cut_not_embedded_whole() {
        let long = "x".repeat(1000);
        let chunks = chunk(&long);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.chars().count() < 260);
        assert!(chunks[0].text.ends_with('…'));
    }

    #[test]
    fn binaries_and_huge_files_are_not_indexable() {
        assert!(indexable(b"fn main() {}"));
        assert!(!indexable(b"PNG\0\0\0"));
        assert!(!indexable(&vec![b'a'; (MAX_FILE_BYTES + 1) as usize]));
        assert!(!indexable(&[0xff, 0xfe, 0x00]));
    }

    fn sample() -> Index {
        Index {
            dim: 3,
            files: vec![
                FileEntry {
                    rel: PathBuf::from("a.rs"),
                    hash: "h1".into(),
                    chunks: vec![
                        (
                            Chunk {
                                start_line: 1,
                                end_line: 40,
                                text: "alpha".into(),
                            },
                            normalize(&[1.0, 0.0, 0.0]),
                        ),
                        (
                            Chunk {
                                start_line: 31,
                                end_line: 70,
                                text: "alpha too".into(),
                            },
                            normalize(&[0.9, 0.1, 0.0]),
                        ),
                    ],
                },
                FileEntry {
                    rel: PathBuf::from("b.rs"),
                    hash: "h2".into(),
                    chunks: vec![(
                        Chunk {
                            start_line: 1,
                            end_line: 10,
                            text: "beta".into(),
                        },
                        normalize(&[0.0, 1.0, 0.0]),
                    )],
                },
            ],
        }
    }

    #[test]
    fn the_index_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic").join("index.bin");
        let index = sample();
        index.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        assert_eq!(back, index);
        assert_eq!(back.files(), 2);
        assert_eq!(back.chunks(), 3);
    }

    #[test]
    fn search_ranks_by_similarity_and_reports_overlapping_windows_once() {
        let index = sample();
        let hits = index.search(&normalize(&[1.0, 0.05, 0.0]), 10);
        assert_eq!(hits[0].path, PathBuf::from("a.rs"));
        assert_eq!(
            hits[0].start_line, 1,
            "the better of two overlapping windows"
        );
        assert_eq!(hits.len(), 2, "its overlapping neighbour is folded into it");
        assert_eq!(hits[1].path, PathBuf::from("b.rs"));
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn a_wrong_magic_or_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        std::fs::write(&path, b"NOPE").unwrap();
        assert!(Index::load(&path).is_err());
    }

    /// Needs the pinned model, the helper and a checkout to index:
    /// `cargo build -p taste-embed && TASTE_EMBED_BIN=target/debug/taste-embed
    /// TASTE_SEMANTIC_MODEL=/path/to.gguf TASTE_SEMANTIC_REPO=/path/to/repo
    /// cargo test -p taste-semantic -- --ignored --nocapture`. Builds a
    /// whole index into a temporary state directory, prints how long it
    /// took, and asks it one question — the numbers the design document
    /// quotes come from here.
    #[test]
    #[ignore]
    fn indexes_a_checkout_and_answers_a_question() {
        let model = std::env::var("TASTE_SEMANTIC_MODEL").expect("TASTE_SEMANTIC_MODEL");
        let repo = std::env::var("TASTE_SEMANTIC_REPO").expect("TASTE_SEMANTIC_REPO");
        let state = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", state.path());
        std::env::set_var(
            "XDG_DATA_HOME",
            Path::new(&model).parent().unwrap().parent().unwrap(),
        );
        let semantic = Semantic::default();
        // The model where `taste_models` looks, by the pin's file name.
        let dir = taste_models::models_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(EMBEDDING.file);
        if !target.exists() {
            std::fs::hard_link(&model, &target)
                .or_else(|_| std::fs::copy(&model, &target).map(|_| ()))
                .unwrap();
        }
        let started = std::time::Instant::now();
        let cancel = AtomicBool::new(false);
        let mut last = 0;
        let report = semantic
            .refresh(Path::new(&repo), &cancel, |p| {
                if p.chunks_embedded / 200 > last {
                    last = p.chunks_embedded / 200;
                    eprintln!(
                        "  {} files, {} chunks, {:.0}s",
                        p.files_done,
                        p.chunks_embedded,
                        started.elapsed().as_secs_f64()
                    );
                }
            })
            .unwrap();
        eprintln!(
            "indexed {} files into {} chunks ({} embedded) in {:.1}s",
            report.files,
            report.chunks,
            report.embedded,
            started.elapsed().as_secs_f64()
        );
        let asked = std::time::Instant::now();
        let hits = semantic
            .search(
                Path::new(&repo),
                "where is it decided whether the agent may write to a file?",
                5,
            )
            .unwrap();
        eprintln!("asked in {:.0} ms", asked.elapsed().as_secs_f64() * 1000.0);
        for hit in &hits {
            eprintln!(
                "  {:.3} {}:{}-{}",
                hit.score,
                hit.path.display(),
                hit.start_line,
                hit.end_line
            );
        }
        assert!(!hits.is_empty());
        // The second refresh finds nothing changed.
        let again = semantic.refresh(Path::new(&repo), &cancel, |_| {}).unwrap();
        assert_eq!(again.embedded, 0);
        assert_eq!(again.files, report.files);
    }

    /// Needs the pinned model and the helper (see above). Asserts what an embedding
    /// model is for: a question lands nearer the code that answers it than
    /// near unrelated code.
    #[test]
    #[ignore]
    fn the_model_puts_a_question_beside_its_answer() {
        let path = std::env::var("TASTE_SEMANTIC_MODEL").expect("TASTE_SEMANTIC_MODEL");
        let embedder = Embedder::load(Path::new(&path)).unwrap();
        assert_eq!(embedder.dim(), 768);
        let docs = embedder
            .embed_documents(&[
                "fn verify_password(user: &User, candidate: &str) -> bool { argon2::verify(...) }"
                    .into(),
                "fn draw_sparkline(cr: &cairo::Context, samples: &[u16]) { ... }".into(),
            ])
            .unwrap();
        let query = embedder
            .embed_query("where is authentication handled?")
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&docs[0], &query) > dot(&docs[1], &query));
    }
}
