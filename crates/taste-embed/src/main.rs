//! `taste-embed`: the embedding model, in a process of its own.
//!
//! The IDE links whisper.cpp for voice, and whisper.cpp and llama.cpp each
//! carry their own ggml; two ggmls cannot share one binary (the link fails
//! on duplicate symbols), so the embedder lives here and `taste-semantic`
//! talks to it over stdio. That also keeps a native library's crash out of
//! the IDE's process, which is where a crash in a 100 MB model would
//! otherwise land.
//!
//! Protocol, one JSON object per line, request then reply:
//!   → {"kind": "document" | "query", "texts": ["…", …]}
//!   ← {"vectors": [[f32, …], …]}   or   {"error": "…"}
//! `argv[1]` is the GGUF model path; the model loads once; the first reply
//! is `{"ready": true, "dim": 768}`.

use std::io::{BufRead, Write};
use std::num::NonZeroU32;
use std::path::Path;

use anyhow::{Context, Result};
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use serde::Deserialize;

/// nomic-embed-text's documented prefixes: a document and a question are
/// embedded differently on purpose.
const DOCUMENT_PREFIX: &str = "search_document: ";
const QUERY_PREFIX: &str = "search_query: ";
/// Tokens per text, at most; the window is larger, this keeps a
/// pathological chunk cheap.
const MAX_TOKENS: usize = 1024;
const CONTEXT_TOKENS: u32 = 2048;

#[derive(Deserialize)]
struct Request {
    kind: String,
    texts: Vec<String>,
}

struct Embedder {
    backend: LlamaBackend,
    model: LlamaModel,
    threads: i32,
}

impl Embedder {
    fn load(path: &Path) -> Result<Self> {
        // llama.cpp narrates every load to stderr; the IDE reads this
        // process's stderr into its log, so leave it on but let it be.
        let backend = LlamaBackend::init().context("initialising llama.cpp")?;
        let model = LlamaModel::load_from_file(&backend, path, &LlamaModelParams::default())
            .with_context(|| format!("loading {}", path.display()))?;
        // Half the machine: this is background work beside an IDE that
        // must stay snappy, and embedding gains little past a few threads.
        let threads = (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            / 2)
        .clamp(1, 16) as i32;
        Ok(Self {
            backend,
            model,
            threads,
        })
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(CONTEXT_TOKENS))
            .with_n_batch(CONTEXT_TOKENS)
            .with_n_ubatch(CONTEXT_TOKENS)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads)
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Mean);
        let mut ctx = self
            .model
            .new_context(&self.backend, params)
            .context("creating a context")?;
        let mut batch = LlamaBatch::new(CONTEXT_TOKENS as usize, 1);
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let mut tokens = self
                .model
                .str_to_token(text, AddBos::Always)
                .context("tokenising")?;
            tokens.truncate(MAX_TOKENS);
            batch.clear();
            // Every token is an output: mean pooling reads them all.
            batch
                .add_sequence(&tokens, 0, true)
                .context("filling the batch")?;
            ctx.clear_kv_cache();
            ctx.decode(&mut batch).context("embedding")?;
            let embedding = ctx.embeddings_seq_ith(0).context("reading the embedding")?;
            out.push(normalize(embedding));
        }
        Ok(out)
    }
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: taste-embed <model.gguf>")?;
    let embedder = Embedder::load(Path::new(&path))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{}",
        serde_json::json!({ "ready": true, "dim": embedder.model.n_embd() })
    )?;
    out.flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let prefix = match request.kind.as_str() {
                    "query" => QUERY_PREFIX,
                    _ => DOCUMENT_PREFIX,
                };
                let prefixed: Vec<String> = request
                    .texts
                    .iter()
                    .map(|t| format!("{prefix}{t}"))
                    .collect();
                match embedder.embed(&prefixed) {
                    Ok(vectors) => serde_json::json!({ "vectors": vectors }),
                    Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
                }
            }
            Err(e) => serde_json::json!({ "error": format!("bad request: {e}") }),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}
