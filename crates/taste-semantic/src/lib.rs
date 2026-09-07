//! Semantic search over a checkout: the meaning of a question against the
//! meaning of the code, for the agents' "where is authentication handled?"
//! — the question `ide_search` cannot answer because no line says
//! "authentication" (docs/spikes/agent-workspace-context.md) — and, behind
//! a toggle, for the person at the box.
//!
//! Local, like everything else the IDE computes: one pinned embedding model
//! ([`EMBEDDING`], fetched once by `taste-models`), run through llama.cpp on
//! the CPU — in a process of its own, `taste-embed`, because the IDE
//! already links whisper.cpp for voice and the two carry ggmls that cannot
//! share a binary.
//!
//! **One store, many checkouts.** A workspace's environments are clones of
//! one repository, each the primary plus a branch's worth of change, so
//! almost every chunk in one is byte-identical to a chunk in another. The
//! vectors therefore live in ONE content-addressed store per workspace,
//! keyed by the hash of the chunk's text, and each checkout has only a
//! manifest: its files, and for each the chunks it is made of by hash.
//! Indexing an environment costs hashing and chunking — seconds — plus
//! embedding the chunks no checkout has seen, which is exactly its own
//! work; a query scans the checkout's manifest against the shared vectors,
//! so every environment sees its own tree and never the primary's answer
//! for a file its agent changed (David, 2026-09-07: "Are efficient
//! derivatives for each env possible, or should the various envs just query
//! the base index?").
//!
//! **Chunks are cut on content, not on line counts.** A blank line or a
//! definition (`taste_core::search::symbols`) starts a new chunk once the
//! current one is big enough, and a chunk is capped in lines; so an edit
//! disturbs the chunk it lands in and not every window after it, and only
//! that chunk is embedded again (David: "Can the index be incrementally
//! freshened?"). Nothing here has a GTK type; the app drives it off the
//! main thread and the MCP server queries it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use taste_core::search::symbols::{definition, language_of, Language};

pub use taste_models::ModelSpec;

/// The embedding model: nomic-embed-text v1.5, the maintainers' own GGUF
/// at Q5_K_M — 100 MB, 768 dimensions, an 8k-token window, Apache-2.0,
/// trained on code as well as prose, and the `search_document:` /
/// `search_query:` prefixes (`taste-embed` adds them) are its documented
/// contract. Pinned by digest like the speech model; a smaller or a
/// code-specialised model is a second constant, not a setting. Digest
/// computed from a real download on 2026-09-07.
pub const EMBEDDING: ModelSpec = ModelSpec {
    name: "nomic-embed-text-v1.5 (Q5_K_M)",
    file: "nomic-embed-text-v1.5.Q5_K_M.gguf",
    url: "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q5_K_M.gguf",
    sha256: "0c7930f6c4f6f29b7da5046e3a2c0832aa3f602db3de5760a95f0582dbd3d6e6",
    bytes: 99_588_928,
};

/// A chunk is cut on content: it ends where a blank line or a definition
/// begins the next, once it has at least `MIN_CHUNK_LINES`, and never runs
/// past `MAX_CHUNK_LINES`. Long enough to carry a function's shape, short
/// enough that a hit points somewhere, and stable under an edit elsewhere
/// in the file.
pub const MAX_CHUNK_LINES: usize = 40;
pub const MIN_CHUNK_LINES: usize = 8;
/// A line longer than this is a minified or generated one; it is cut, not
/// embedded whole.
const MAX_LINE_CHARS: usize = 240;
/// A file bigger than this is a lock file, a fixture or a build product,
/// and its meaning is not what anyone is asking after.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Texts per request to the helper: the helper embeds one at a time
/// either way, this only bounds one round trip's JSON.
const EMBED_BATCH: usize = 16;

/// The on-disk format; a mismatch rebuilds rather than migrates (alpha).
const FORMAT: u32 = 2;
const STORE_MAGIC: &[u8; 4] = b"TSEV";
const MANIFEST_MAGIC: &[u8; 4] = b"TSEM";

pub type ChunkHash = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// Whether a line begins something: blank (the paragraph before it ended)
/// or a definition in the file's language.
fn is_boundary(language: Option<Language>, line: &str) -> bool {
    line.trim().is_empty() || language.is_some_and(|language| definition(language, line).is_some())
}

/// Cut a file into chunks on content boundaries.
pub fn chunk(text: &str, language: Option<Language>) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let len = i - start;
        let boundary = len >= MIN_CHUNK_LINES && is_boundary(language, lines[i]);
        if (boundary || len >= MAX_CHUNK_LINES) && len > 0 {
            push_chunk(&mut out, &lines, start, i);
            start = i;
            // A blank boundary belongs to neither chunk.
            if lines[i].trim().is_empty() {
                start = i + 1;
            }
        }
        i += 1;
    }
    if start < lines.len() {
        push_chunk(&mut out, &lines, start, lines.len());
    }
    out
}

fn push_chunk(out: &mut Vec<Chunk>, lines: &[&str], start: usize, end: usize) {
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
    if body.trim().is_empty() {
        return;
    }
    out.push(Chunk {
        start_line: start as u32 + 1,
        end_line: end as u32,
        text: body,
    });
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

pub fn chunk_hash(text: &str) -> ChunkHash {
    Sha256::digest(text.as_bytes()).into()
}

/// Unit length, so similarity is a dot product.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

// --- the embedder ----------------------------------------------------------

/// What turns text into vectors. The helper process in production; a
/// deterministic stand-in in tests, where the model is not the thing under
/// test.
pub trait Embedding: Send + Sync {
    fn dim(&self) -> usize;
    fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn query(&self, text: &str) -> Result<Vec<f32>>;
}

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

impl Embedding for Embedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.ask("document", texts)
    }

    fn query(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.ask("query", &[text.trim().to_string()])?;
        out.pop().context("no embedding came back")
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

// --- the store: every chunk any checkout has, once -------------------------

/// The workspace's vectors, content-addressed: one entry per distinct
/// chunk text, with the text kept beside its vector so a hit can show it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    dim: usize,
    hashes: Vec<ChunkHash>,
    texts: Vec<String>,
    /// `dim` floats per entry, flat.
    vectors: Vec<f32>,
    index: HashMap<ChunkHash, usize>,
}

impl Store {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    fn contains(&self, hash: &ChunkHash) -> bool {
        self.index.contains_key(hash)
    }

    fn insert(&mut self, hash: ChunkHash, text: String, vector: &[f32]) {
        if self.index.contains_key(&hash) {
            return;
        }
        self.index.insert(hash, self.hashes.len());
        self.hashes.push(hash);
        self.texts.push(text);
        self.vectors.extend_from_slice(vector);
    }

    fn vector(&self, hash: &ChunkHash) -> Option<&[f32]> {
        let at = *self.index.get(hash)?;
        Some(&self.vectors[at * self.dim..(at + 1) * self.dim])
    }

    fn text(&self, hash: &ChunkHash) -> Option<&str> {
        self.index.get(hash).map(|at| self.texts[*at].as_str())
    }

    /// The store without the entries nothing references any more.
    fn retain(&self, keep: &HashSet<ChunkHash>) -> Store {
        let mut out = Store::new(self.dim);
        for (at, hash) in self.hashes.iter().enumerate() {
            if keep.contains(hash) {
                out.insert(
                    *hash,
                    self.texts[at].clone(),
                    &self.vectors[at * self.dim..(at + 1) * self.dim],
                );
            }
        }
        out
    }

    fn save(&self, path: &Path) -> Result<()> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(STORE_MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.dim as u32).to_le_bytes());
        out.extend_from_slice(&(self.hashes.len() as u32).to_le_bytes());
        for (at, hash) in self.hashes.iter().enumerate() {
            out.extend_from_slice(hash);
            write_str(&mut out, &self.texts[at]);
            for x in &self.vectors[at * self.dim..(at + 1) * self.dim] {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        write_atomically(path, &out)
    }

    fn load(path: &Path) -> Result<Self> {
        let bytes = read_all(path)?;
        let mut cursor = Cursor {
            bytes: &bytes,
            at: 0,
        };
        if cursor.take(4)? != STORE_MAGIC {
            bail!("{} is not a vector store", path.display());
        }
        if cursor.u32()? != FORMAT {
            bail!("{} is an older store format; it is rebuilt", path.display());
        }
        let dim = cursor.u32()? as usize;
        let n = cursor.u32()? as usize;
        let mut store = Store::new(dim);
        for _ in 0..n {
            let hash: ChunkHash = cursor.take(32)?.try_into().unwrap();
            let text = cursor.string()?;
            let mut vector = Vec::with_capacity(dim);
            for _ in 0..dim {
                vector.push(cursor.f32()?);
            }
            store.insert(hash, text, &vector);
        }
        Ok(store)
    }
}

// --- the manifest: one checkout's files, as chunk hashes -------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChunkRef {
    start_line: u32,
    end_line: u32,
    hash: ChunkHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileEntry {
    rel: PathBuf,
    hash: String,
    chunks: Vec<ChunkRef>,
}

/// One checkout's index: which files, and which chunks each is, by hash
/// into the workspace's [`Store`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    files: Vec<FileEntry>,
}

impl Manifest {
    pub fn files(&self) -> usize {
        self.files.len()
    }

    pub fn chunks(&self) -> usize {
        self.files.iter().map(|f| f.chunks.len()).sum()
    }

    fn hashes(&self) -> impl Iterator<Item = &ChunkHash> {
        self.files
            .iter()
            .flat_map(|f| f.chunks.iter().map(|c| &c.hash))
    }

    /// Top `limit` chunks by cosine similarity (unit vectors, so a dot
    /// product). Two overlapping chunks of one file that both hit are
    /// reported once, as the better one.
    fn search(&self, store: &Store, query: &[f32], limit: usize) -> Vec<Hit> {
        let mut scored: Vec<Hit> = Vec::new();
        for file in &self.files {
            for chunk in &file.chunks {
                let Some(vector) = store.vector(&chunk.hash) else {
                    continue;
                };
                let score: f32 = vector.iter().zip(query).map(|(a, b)| a * b).sum();
                scored.push(Hit {
                    path: file.rel.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    text: store.text(&chunk.hash).unwrap_or_default().to_string(),
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
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(MANIFEST_MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for file in &self.files {
            write_str(&mut out, &file.rel.to_string_lossy());
            write_str(&mut out, &file.hash);
            out.extend_from_slice(&(file.chunks.len() as u32).to_le_bytes());
            for chunk in &file.chunks {
                out.extend_from_slice(&chunk.start_line.to_le_bytes());
                out.extend_from_slice(&chunk.end_line.to_le_bytes());
                out.extend_from_slice(&chunk.hash);
            }
        }
        write_atomically(path, &out)
    }

    fn load(path: &Path) -> Result<Self> {
        let bytes = read_all(path)?;
        let mut cursor = Cursor {
            bytes: &bytes,
            at: 0,
        };
        if cursor.take(4)? != MANIFEST_MAGIC {
            bail!("{} is not a manifest", path.display());
        }
        if cursor.u32()? != FORMAT {
            bail!(
                "{} is an older manifest format; it is rebuilt",
                path.display()
            );
        }
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
                let hash: ChunkHash = cursor.take(32)?.try_into().unwrap();
                chunks.push(ChunkRef {
                    start_line,
                    end_line,
                    hash,
                });
            }
            files.push(FileEntry { rel, hash, chunks });
        }
        Ok(Self { files })
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::File::create(&temp)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
}

fn read_all(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(bytes)
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
            bail!("the file ends early");
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

// --- the service -----------------------------------------------------------

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
/// `chunks_total` — the chunks no checkout has embedded yet — is known
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
    /// Chunks embedded this time — zero when nothing new was seen.
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

/// The service: the workspace's one store, a manifest per checkout, one
/// embedder for documents and one for questions, refreshes that never
/// overlap on the same checkout.
pub struct Semantic {
    /// `<workspace state dir>/semantic`: `vectors.bin` and `manifests/`.
    dir: PathBuf,
    /// The helper that embeds documents — busy for seconds at a time while
    /// an index builds.
    embedder: Mutex<Option<Arc<dyn Embedding>>>,
    /// A second helper for questions, so a person's query is answered in
    /// milliseconds while a refresh is in the middle of a batch. Started
    /// on the first question, at the cost of a second copy of the model
    /// in memory while both live.
    query_embedder: Mutex<Option<Arc<dyn Embedding>>>,
    /// A stand-in for both, in tests.
    provided: Option<Arc<dyn Embedding>>,
    store: Mutex<Option<Arc<Store>>>,
    manifests: Mutex<HashMap<PathBuf, Arc<Manifest>>>,
    refreshing: Mutex<HashSet<PathBuf>>,
}

impl Semantic {
    /// The workspace's service: its store lives under the workspace's
    /// state directory, whichever of its checkouts is asked about.
    pub fn new(workspace_root: &Path) -> Arc<Self> {
        Arc::new(Self::at(
            taste_core::state::workspace_state_dir(workspace_root).join("semantic"),
            None,
        ))
    }

    /// A service at a directory of the caller's choosing, with the caller's
    /// embedding — for tests, and for anything that is not the IDE.
    pub fn with_embedding(dir: PathBuf, embedding: Arc<dyn Embedding>) -> Arc<Self> {
        Arc::new(Self::at(dir, Some(embedding)))
    }

    fn at(dir: PathBuf, provided: Option<Arc<dyn Embedding>>) -> Self {
        Self {
            dir,
            embedder: Mutex::new(None),
            query_embedder: Mutex::new(None),
            provided,
            store: Mutex::new(None),
            manifests: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
        }
    }

    pub fn model_present() -> bool {
        taste_models::is_present(&EMBEDDING)
    }

    fn embedder(&self) -> Result<Arc<dyn Embedding>> {
        self.embedder_in(&self.embedder)
    }

    fn query_embedder(&self) -> Result<Arc<dyn Embedding>> {
        self.embedder_in(&self.query_embedder)
    }

    fn embedder_in(&self, slot: &Mutex<Option<Arc<dyn Embedding>>>) -> Result<Arc<dyn Embedding>> {
        if let Some(provided) = &self.provided {
            return Ok(provided.clone());
        }
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(embedder) = slot.as_ref() {
            return Ok(embedder.clone());
        }
        if !Self::model_present() {
            bail!(Unavailable::ModelAbsent);
        }
        let embedder: Arc<dyn Embedding> =
            Arc::new(Embedder::load(&taste_models::model_path(&EMBEDDING))?);
        *slot = Some(embedder.clone());
        Ok(embedder)
    }

    fn store_path(&self) -> PathBuf {
        self.dir.join("vectors.bin")
    }

    fn manifests_dir(&self) -> PathBuf {
        self.dir.join("manifests")
    }

    fn manifest_path(&self, root: &Path) -> PathBuf {
        let digest = Sha256::digest(root.to_string_lossy().as_bytes());
        let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "checkout".into());
        self.manifests_dir().join(format!("{name}-{short}.bin"))
    }

    /// The store, from memory or disk; empty with the embedder's dimension
    /// when there is none yet.
    fn store(&self, dim: usize) -> Arc<Store> {
        let mut slot = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = slot.as_ref() {
            return store.clone();
        }
        let loaded = Store::load(&self.store_path())
            .ok()
            .filter(|store| store.dim == dim)
            .unwrap_or_else(|| Store::new(dim));
        let loaded = Arc::new(loaded);
        *slot = Some(loaded.clone());
        loaded
    }

    fn loaded_store(&self) -> Option<Arc<Store>> {
        if let Some(store) = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Some(store.clone());
        }
        let loaded = Arc::new(Store::load(&self.store_path()).ok()?);
        *self.store.lock().unwrap_or_else(|e| e.into_inner()) = Some(loaded.clone());
        Some(loaded)
    }

    /// A checkout's manifest, from memory or disk.
    fn manifest(&self, root: &Path) -> Option<Arc<Manifest>> {
        if let Some(manifest) = self
            .manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(root)
        {
            return Some(manifest.clone());
        }
        let loaded = Arc::new(Manifest::load(&self.manifest_path(root)).ok()?);
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.to_path_buf(), loaded.clone());
        Some(loaded)
    }

    /// Every chunk hash any manifest on disk still refers to — what the
    /// store keeps when it is written.
    fn referenced(&self) -> HashSet<ChunkHash> {
        let mut keep = HashSet::new();
        for manifest in self
            .manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            keep.extend(manifest.hashes().copied());
        }
        if let Ok(entries) = std::fs::read_dir(self.manifests_dir()) {
            for entry in entries.flatten() {
                if let Ok(manifest) = Manifest::load(&entry.path()) {
                    keep.extend(manifest.hashes().copied());
                }
            }
        }
        keep
    }

    /// Whether a checkout has an index, and how big: files and chunks.
    pub fn status(&self, root: &Path) -> Option<(usize, usize)> {
        self.manifest(root)
            .map(|manifest| (manifest.files(), manifest.chunks()))
    }

    /// How many distinct chunks the workspace's store holds.
    pub fn stored_chunks(&self) -> usize {
        self.loaded_store().map(|store| store.len()).unwrap_or(0)
    }

    /// Whether a refresh of this checkout is running.
    pub fn refreshing(&self, root: &Path) -> bool {
        self.refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(root)
    }

    /// Bring a checkout's index up to date. Blocking: call it from a
    /// blocking pool. A file whose hash is unchanged keeps its chunks; a
    /// changed or new file is re-chunked, and only the chunks whose text no
    /// checkout has embedded before are embedded; files gone from the
    /// checkout leave the manifest. `cancel` is checked between batches,
    /// and a cancelled refresh keeps what it had (the next one finishes the
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
        let store = self.store(embedder.dim());
        let previous = self.manifest(root).unwrap_or_default();
        let known: HashMap<&Path, &FileEntry> = previous
            .files
            .iter()
            .map(|f| (f.rel.as_path(), f))
            .collect();
        let paths = taste_core::search::collect_files(root, |_| {});

        // Pass one, the plan: hash every file; keep an unchanged file's
        // chunks, cut a changed one anew; note every chunk text the store
        // has never seen. Cheap, and it makes the slow pass's size known
        // before it starts — which is what a remaining-time estimate is
        // made of.
        let mut files: Vec<FileEntry> = Vec::with_capacity(paths.len());
        let mut missing: Vec<(ChunkHash, String)> = Vec::new();
        let mut missing_seen: HashSet<ChunkHash> = HashSet::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut status = Progress {
            files_total: paths.len(),
            ..Progress::default()
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
                if entry.hash == hash && entry.chunks.iter().all(|c| store.contains(&c.hash)) {
                    files.push((*entry).clone());
                    continue;
                }
            }
            let text = String::from_utf8_lossy(&bytes);
            let chunks = chunk(&text, language_of(rel));
            if chunks.is_empty() {
                continue;
            }
            let refs: Vec<ChunkRef> = chunks
                .iter()
                .map(|c| {
                    let hash = chunk_hash(&c.text);
                    if !store.contains(&hash) && missing_seen.insert(hash) {
                        missing.push((hash, c.text.clone()));
                    }
                    ChunkRef {
                        start_line: c.start_line,
                        end_line: c.end_line,
                        hash,
                    }
                })
                .collect();
            files.push(FileEntry {
                rel: rel.to_path_buf(),
                hash,
                chunks: refs,
            });
        }
        status.chunks_total = missing.len();
        progress(status);

        // Pass two: embed what no checkout has, checking the stop flag
        // between batches, into a copy of the store that replaces it.
        let mut next_store = (*store).clone();
        let mut report = Report::default();
        let mut embedded: HashSet<ChunkHash> = HashSet::new();
        for batch in missing.chunks(EMBED_BATCH) {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            let vectors = embedder.documents(&texts).context("embedding")?;
            for ((hash, text), vector) in batch.iter().zip(vectors) {
                next_store.insert(*hash, text.clone(), &normalize(&vector));
                embedded.insert(*hash);
            }
            report.embedded += batch.len();
            status.chunks_embedded += batch.len();
            progress(status);
        }
        if report.cancelled {
            // A file with a chunk still unembedded keeps its previous entry
            // (or none): the manifest never names a vector the store lacks.
            files.retain(|f| f.chunks.iter().all(|c| next_store.contains(&c.hash)));
            for entry in &previous.files {
                if !files.iter().any(|f| f.rel == entry.rel)
                    && entry.chunks.iter().all(|c| next_store.contains(&c.hash))
                {
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
        let manifest = Manifest { files };
        report.files = manifest.files();
        report.chunks = manifest.chunks();
        manifest.save(&self.manifest_path(root))?;
        let manifest = Arc::new(manifest);
        self.manifests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.to_path_buf(), manifest);
        // The store keeps what any checkout still refers to, and drops the
        // chunks every edit has left behind.
        let compacted = Arc::new(next_store.retain(&self.referenced()));
        compacted.save(&self.store_path())?;
        *self.store.lock().unwrap_or_else(|e| e.into_inner()) = Some(compacted);
        Ok(report)
    }

    /// Ask a checkout's index. `Unavailable` when there is nothing to ask
    /// yet — the caller decides whether to start a refresh.
    pub fn search(&self, root: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(manifest) = self.manifest(root) else {
            bail!(Unavailable::NotIndexed);
        };
        let Some(store) = self.loaded_store() else {
            bail!(Unavailable::NotIndexed);
        };
        let embedder = self.query_embedder()?;
        let vector = normalize(&embedder.query(query)?);
        Ok(manifest.search(&store, &vector, limit.clamp(1, 50)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_break_on_blank_lines_and_definitions_once_big_enough() {
        let mut text = String::new();
        for n in 1..=12 {
            text.push_str(&format!("    let a{n} = {n};\n"));
        }
        text.push('\n');
        text.push_str("fn second() {\n");
        for n in 1..=5 {
            text.push_str(&format!("    let b{n} = {n};\n"));
        }
        text.push_str("}\n");
        text.push_str("pub fn third() {}\n");
        let chunks = chunk(&text, Some(Language::Rust));
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 12, "the blank line ends the first");
        assert_eq!(chunks[1].start_line, 14, "and belongs to neither");
        assert!(chunks[1].text.starts_with("fn second"));
        // `pub fn third` is a definition but the chunk before it has only 7
        // lines: too small to break.
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[1].text.ends_with("pub fn third() {}"));
    }

    #[test]
    fn a_chunk_never_runs_past_the_cap_and_the_tail_is_kept() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let chunks = chunk(&text, None);
        assert!(chunks
            .iter()
            .all(|c| (c.end_line - c.start_line + 1) as usize <= MAX_CHUNK_LINES));
        assert_eq!(chunks.first().unwrap().start_line, 1);
        assert_eq!(chunks.last().unwrap().end_line, 100);
        assert!(chunk("", None).is_empty());
        assert!(
            chunk("\n\n  \n", None).is_empty(),
            "whitespace is not a chunk"
        );
    }

    #[test]
    fn an_edit_disturbs_only_its_own_chunk() {
        let mut text = String::new();
        for para in 0..4 {
            for n in 0..10 {
                text.push_str(&format!("paragraph {para} line {n}\n"));
            }
            text.push('\n');
        }
        let before = chunk(&text, None);
        let after = chunk(
            &text.replace("paragraph 2 line 3", "paragraph 2 CHANGED"),
            None,
        );
        assert_eq!(before.len(), after.len());
        let changed: Vec<usize> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (a, b))| a.text != b.text)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(changed, vec![2], "one chunk's hash moves, the rest stand");
    }

    #[test]
    fn a_long_line_is_cut_not_embedded_whole() {
        let long = "x".repeat(1000);
        let chunks = chunk(&long, None);
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

    /// A stand-in embedding: a text's vector is a function of its words, so
    /// similar texts land near each other and the arithmetic is real.
    struct Fake;

    impl Fake {
        fn vector(text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32; 16];
            for word in text.split(|c: char| !c.is_alphanumeric()) {
                if word.is_empty() {
                    continue;
                }
                let h = Sha256::digest(word.to_lowercase().as_bytes());
                v[(h[0] % 16) as usize] += 1.0;
            }
            normalize(&v)
        }
    }

    impl Embedding for Fake {
        fn dim(&self) -> usize {
            16
        }
        fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| Self::vector(t)).collect())
        }
        fn query(&self, text: &str) -> Result<Vec<f32>> {
            Ok(Self::vector(text))
        }
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn checkout(dir: &Path) {
        write(
            dir,
            "auth.rs",
            "fn verify_password(user: &User, candidate: &str) -> bool {\n    argon2::verify(user.hash, candidate)\n}\n",
        );
        write(
            dir,
            "draw.rs",
            "fn draw_sparkline(cr: &cairo::Context, samples: &[u16]) {\n    cr.move_to(0.0, 0.0);\n}\n",
        );
        write(dir, "notes.md", "Passwords are verified with argon2.\n");
    }

    #[test]
    fn two_checkouts_share_one_store_and_the_second_costs_only_its_difference() {
        let state = tempfile::tempdir().unwrap();
        let semantic = Semantic::with_embedding(state.path().join("semantic"), Arc::new(Fake));
        let primary = tempfile::tempdir().unwrap();
        checkout(primary.path());
        let cancel = AtomicBool::new(false);
        let first = semantic.refresh(primary.path(), &cancel, |_| {}).unwrap();
        assert_eq!(first.files, 3);
        assert_eq!(first.embedded, first.chunks, "everything was new");
        assert_eq!(semantic.stored_chunks(), first.chunks);

        // A clone with one file changed: only that file's chunk is new.
        let clone = tempfile::tempdir().unwrap();
        checkout(clone.path());
        write(
            clone.path(),
            "draw.rs",
            "fn draw_sparkline(cr: &cairo::Context, samples: &[u16]) {\n    cr.move_to(1.0, 1.0);\n}\n",
        );
        let second = semantic.refresh(clone.path(), &cancel, |_| {}).unwrap();
        assert_eq!(second.files, 3);
        assert_eq!(second.embedded, 1, "the changed chunk, and nothing shared");
        assert_eq!(semantic.stored_chunks(), first.chunks + 1);

        // Nothing changed: nothing embedded, on either.
        let again = semantic.refresh(primary.path(), &cancel, |_| {}).unwrap();
        assert_eq!(again.embedded, 0);
        assert_eq!(again.files, 3);

        // Each checkout answers from its own tree.
        let hits = semantic
            .search(primary.path(), "how are passwords verified", 3)
            .unwrap();
        assert_eq!(hits[0].path, PathBuf::from("notes.md"), "{hits:?}");
        assert!(hits
            .iter()
            .any(|h| h.path.as_path() == Path::new("auth.rs")));
        let clone_hits = semantic.search(clone.path(), "move_to 1.0", 1).unwrap();
        assert!(
            clone_hits[0].text.contains("1.0, 1.0"),
            "the clone's own draw.rs"
        );

        // A file gone from the checkout leaves the manifest, and a chunk no
        // checkout refers to leaves the store.
        std::fs::remove_file(primary.path().join("notes.md")).unwrap();
        let removed = semantic.refresh(primary.path(), &cancel, |_| {}).unwrap();
        assert_eq!(removed.removed_files, 1);
        assert_eq!(removed.files, 2);
        assert_eq!(
            semantic.stored_chunks(),
            first.chunks + 1,
            "notes.md's chunk is still the clone's"
        );
        std::fs::remove_file(clone.path().join("notes.md")).unwrap();
        semantic.refresh(clone.path(), &cancel, |_| {}).unwrap();
        assert_eq!(semantic.stored_chunks(), first.chunks, "now it is nobody's");

        // Everything survives a fresh service reading the same directory.
        let reopened = Semantic::with_embedding(state.path().join("semantic"), Arc::new(Fake));
        assert_eq!(reopened.status(primary.path()), Some((2, 2)));
        assert_eq!(reopened.stored_chunks(), first.chunks);
        assert!(reopened.search(clone.path(), "sparkline", 1).is_ok());
    }

    #[test]
    fn a_cancelled_refresh_keeps_what_it_finished_and_names_no_missing_vector() {
        let state = tempfile::tempdir().unwrap();
        let semantic = Semantic::with_embedding(state.path().join("semantic"), Arc::new(Fake));
        let dir = tempfile::tempdir().unwrap();
        for n in 0..40 {
            write(
                dir.path(),
                &format!("f{n}.txt"),
                &format!("file number {n}\n"),
            );
        }
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let report = semantic
            .refresh(dir.path(), &cancel, |_| {
                calls += 1;
                if calls == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert!(report.cancelled);
        assert!(report.files < 40 && report.files > 0, "{report:?}");
        let store = semantic.loaded_store().unwrap();
        let manifest = semantic.manifest(dir.path()).unwrap();
        assert!(manifest.hashes().all(|h| store.contains(h)));
        cancel.store(false, Ordering::Relaxed);
        let finished = semantic.refresh(dir.path(), &cancel, |_| {}).unwrap();
        assert_eq!(finished.files, 40);
        assert!(!finished.cancelled);
    }

    #[test]
    fn a_wrong_magic_or_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        std::fs::write(&path, b"NOPE").unwrap();
        assert!(Store::load(&path).is_err());
        assert!(Manifest::load(&path).is_err());
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
        let embedder: Arc<dyn Embedding> = Arc::new(Embedder::load(Path::new(&model)).unwrap());
        let semantic = Semantic::with_embedding(state.path().join("semantic"), embedder);
        let started = std::time::Instant::now();
        let cancel = AtomicBool::new(false);
        let mut last = 0;
        let report = semantic
            .refresh(Path::new(&repo), &cancel, |p| {
                if p.chunks_embedded / 200 > last {
                    last = p.chunks_embedded / 200;
                    eprintln!(
                        "  {} of {} chunks, {:.0}s",
                        p.chunks_embedded,
                        p.chunks_total,
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
    }

    /// Needs the pinned model and the helper (see above). Asserts what an
    /// embedding model is for: a question lands nearer the code that
    /// answers it than near unrelated code.
    #[test]
    #[ignore]
    fn the_model_puts_a_question_beside_its_answer() {
        let path = std::env::var("TASTE_SEMANTIC_MODEL").expect("TASTE_SEMANTIC_MODEL");
        let embedder = Embedder::load(Path::new(&path)).unwrap();
        assert_eq!(embedder.dim(), 768);
        let docs = embedder
            .documents(&[
                "fn verify_password(user: &User, candidate: &str) -> bool { argon2::verify(...) }"
                    .into(),
                "fn draw_sparkline(cr: &cairo::Context, samples: &[u16]) { ... }".into(),
            ])
            .unwrap();
        let query = embedder.query("where is authentication handled?").unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&docs[0], &query) > dot(&docs[1], &query));
    }
}
