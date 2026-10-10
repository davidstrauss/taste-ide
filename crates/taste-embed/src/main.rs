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
/// One text per forward pass. Packing sixteen into one pass was tried and
/// measured on this repository — 1,006 s against 846 s for one at a time,
/// on twelve threads — so the cost is the model's arithmetic, not the
/// pass count, and one at a time keeps the compute buffer small.
const CONTEXT_TOKENS: u32 = 2048;
const MAX_SEQUENCES: usize = 1;

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
        // Half the machine's compute: this is background work beside an
        // IDE that must stay snappy, and embedding gains little past a few
        // threads (`compute_threads`).
        let threads = compute_threads();
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
            .with_n_seq_max(MAX_SEQUENCES as u32)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads)
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Mean);
        let mut ctx = self
            .model
            .new_context(&self.backend, params)
            .context("creating a context")?;
        let mut batch = LlamaBatch::new(CONTEXT_TOKENS as usize, MAX_SEQUENCES as i32);
        let tokenized = texts
            .iter()
            .map(|text| {
                let mut tokens = self
                    .model
                    .str_to_token(text, AddBos::Always)
                    .context("tokenising")?;
                tokens.truncate(MAX_TOKENS);
                Ok(tokens)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(texts.len());
        let mut next = 0;
        while next < tokenized.len() {
            batch.clear();
            let mut sequences = 0usize;
            let mut used = 0usize;
            while next < tokenized.len()
                && sequences < MAX_SEQUENCES
                && used + tokenized[next].len() <= CONTEXT_TOKENS as usize
            {
                // Every token is an output: mean pooling reads them all.
                batch
                    .add_sequence(&tokenized[next], sequences as i32, true)
                    .context("filling the batch")?;
                used += tokenized[next].len();
                sequences += 1;
                next += 1;
            }
            ctx.clear_kv_cache();
            ctx.decode(&mut batch).context("embedding")?;
            for seq in 0..sequences {
                let embedding = ctx
                    .embeddings_seq_ith(seq as i32)
                    .context("reading an embedding")?;
                out.push(normalize(embedding));
            }
        }
        Ok(out)
    }
}

/// Half the machine's PHYSICAL cores. Half its logical CPUs was the rule,
/// and on a machine whose cores each run two hardware threads it is the
/// whole of it: llama.cpp's vector arithmetic on two threads of one core
/// shares that core's units, so eight threads on sixteen logical CPUs read
/// as 44% in a process monitor and kept every core busy (David,
/// 2026-10-02: "Cap it at 50%"). Cores are counted from the kernel's
/// topology — one per distinct set of sibling threads — and fall back to
/// the logical count where it is not exposed.
fn compute_threads() -> i32 {
    (physical_cores() / 2).clamp(1, 16) as i32
}

fn physical_cores() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut cores = std::collections::BTreeSet::new();
    for cpu in 0..logical {
        let siblings = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list");
        match std::fs::read_to_string(&siblings) {
            Ok(list) => {
                cores.insert(list.trim().to_string());
            }
            Err(_) => return logical,
        }
    }
    cores.len().max(1)
}

/// The share of the machine indexing may average: half of every logical
/// CPU's time. The thread count keeps the arithmetic under it on its own;
/// this is the ceiling that holds whatever the hardware or the backend
/// does with threads — tokenising, a backend that spins — measured rather
/// than assumed.
const CPU_SHARE: f64 = 0.5;

/// This process's CPU time so far, every thread of it.
fn cpu_time() -> std::time::Duration {
    // SAFETY: getrusage writes the struct it is handed and reads nothing.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return std::time::Duration::ZERO;
    }
    let seconds = |t: libc::timeval| {
        std::time::Duration::from_secs(t.tv_sec as u64)
            + std::time::Duration::from_micros(t.tv_usec as u64)
    };
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// How long to rest after a stretch of work so it averages `share` of the
/// machine: `cpu` spent over `wall` on `cpus` logical CPUs. Nothing when
/// it is already under.
fn rest_for(
    cpu: std::time::Duration,
    wall: std::time::Duration,
    cpus: usize,
    share: f64,
) -> std::time::Duration {
    let allowed = share * cpus as f64;
    let needed = cpu.as_secs_f64() / allowed;
    std::time::Duration::from_secs_f64((needed - wall.as_secs_f64()).max(0.0))
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

/// Run only on time nothing else on the machine wants. A niceness of ten
/// was a smaller share, not a lower place: under a build, a browser, or a
/// VM, indexing still took its weight of every contended core (David,
/// 2026-10-09: "Is it possible to deprioritize the semantic indexing versus
/// other activity on my machine?"). So, for this process and the threads it
/// starts after:
/// - `SCHED_IDLE`, the scheduler's lowest class: below every ordinary
///   process whatever its niceness, run when a core would otherwise idle;
/// - niceness 19, the floor of the ordinary scale, for a kernel that
///   refuses the class;
/// - the idle I/O class: its reads of the model wait for every other
///   process's disk work.
///
/// Every one of these is a process lowering itself, which needs no
/// privilege and works inside the Flatpak. A query still runs here, and on
/// an idle machine at full speed; on a saturated one it waits its turn,
/// which the search shows as its own progress.
fn yield_to_everything() {
    // SAFETY: each call changes this thread's own scheduling and reads
    // nothing but the struct handed to it.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        // ioprio_set(IOPRIO_WHO_PROCESS, self, IOPRIO_CLASS_IDLE << 13).
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
        );
    }
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: taste-embed <model.gguf>")?;
    // Before anything else starts a thread, so every thread inherits it.
    yield_to_everything();
    let embedder = Embedder::load(Path::new(&path))?;
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{}",
        serde_json::json!({ "ready": true, "dim": embedder.model.n_embd() })
    )?;
    out.flush()?;
    let mut rest_until: Option<std::time::Instant> = None;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut indexing = false;
        let mut began = (std::time::Instant::now(), cpu_time());
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                indexing = request.kind != "query";
                // The rest the last indexing stretch earned is taken before
                // the next one, not after it: a query that arrives in the
                // meantime is answered at once.
                if indexing {
                    if let Some(rest) = rest_until.take() {
                        std::thread::sleep(
                            rest.saturating_duration_since(std::time::Instant::now()),
                        );
                    }
                    began = (std::time::Instant::now(), cpu_time());
                }
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
        // Indexing rests until it averages its share; a query, which
        // someone is waiting on, does not.
        if indexing {
            rest_until = Some(
                std::time::Instant::now()
                    + rest_for(
                        cpu_time().saturating_sub(began.1),
                        began.0.elapsed(),
                        cpus,
                        CPU_SHARE,
                    ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helper lowers itself to the bottom of every scale it can, and a
    /// thread it starts after inherits all of it.
    #[test]
    fn the_helper_runs_only_on_time_nothing_else_wants() {
        std::thread::spawn(|| {
            yield_to_everything();
            let check = || unsafe {
                (
                    libc::sched_getscheduler(0),
                    libc::getpriority(libc::PRIO_PROCESS, 0),
                    libc::syscall(libc::SYS_ioprio_get, 1, 0) >> 13,
                )
            };
            assert_eq!(check(), (libc::SCHED_IDLE, 19, 3));
            let inherited = std::thread::spawn(check).join().unwrap();
            assert_eq!(inherited, (libc::SCHED_IDLE, 19, 3));
        })
        .join()
        .unwrap();
    }
    use std::time::Duration;

    #[test]
    fn indexing_rests_until_it_averages_half_the_machine() {
        // Eight CPUs' worth for a second, on sixteen: exactly half, no rest.
        assert_eq!(
            rest_for(Duration::from_secs(8), Duration::from_secs(1), 16, 0.5),
            Duration::ZERO
        );
        // Twelve CPU-seconds in one second on sixteen: half allows eight a
        // second, so the stretch must last 1.5 s — half a second of rest.
        assert_eq!(
            rest_for(Duration::from_secs(12), Duration::from_secs(1), 16, 0.5),
            Duration::from_millis(500)
        );
        // Under the share: nothing.
        assert_eq!(
            rest_for(Duration::from_secs(1), Duration::from_secs(1), 16, 0.5),
            Duration::ZERO
        );
    }

    #[test]
    fn threads_are_half_the_physical_cores() {
        let threads = compute_threads();
        assert!(threads >= 1);
        assert!(threads as usize <= physical_cores().max(2));
    }
}
