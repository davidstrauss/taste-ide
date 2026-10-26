//! `taste-transcribe`: the speech model, in a process of its own.
//!
//! whisper.cpp was linked into the IDE, so the model's few hundred
//! megabytes stayed in the IDE from the first dictation to the last
//! window closed, and an abort in ggml — its answer to some internal
//! errors, and to running out of memory partway through — took the whole
//! window with it. Here the model lives as long as this process, which the
//! IDE starts when the microphone is pressed (so the model loads while
//! the user speaks) and which exits on its own after [`IDLE`] without a
//! request, giving the memory back; a crash ends one dictation, not the
//! window (David, 2026-10-26).
//!
//! Protocol, over stdio, request then reply:
//!   → a little-endian u32, the number of samples, then that many
//!     little-endian f32s: 16 kHz mono audio
//!   ← one JSON object per line: `{"text": "…"}` or `{"error": "…"}`
//! `argv[1]` is the model path; the model loads once; the first line out
//! is `{"ready": true}`, or `{"error": "…"}` and an exit. The samples are
//! binary because half a minute of speech is half a million floats, which
//! as JSON would be several megabytes of text to write and parse for every
//! partial transcription.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// How long the helper waits for a request before it exits, freeing the
/// model's memory; the next dictation starts another.
const IDLE: Duration = Duration::from_secs(300);

/// The most samples one request may carry: ten minutes at 16 kHz, well
/// past what the composer records, so a corrupt length is refused rather
/// than allocated.
const MAX_SAMPLES: usize = 16_000 * 60 * 10;

fn main() {
    let Some(model) = std::env::args_os().nth(1) else {
        say(&serde_json::json!({ "error": "no model path given" }));
        std::process::exit(2);
    };
    let context = match load(Path::new(&model)) {
        Ok(context) => context,
        Err(e) => {
            say(&serde_json::json!({ "error": format!("{e:#}") }));
            std::process::exit(1);
        }
    };
    say(&serde_json::json!({ "ready": true }));

    // Requests are read on a thread of their own so the wait for the next
    // one can time out: a blocking read on stdin cannot.
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(Some(samples)) = read_request(&mut stdin) {
            if tx.send(samples).is_err() {
                return;
            }
        }
    });
    // A request, or nothing for IDLE, or the IDE gone (stdin closed, and
    // with it the reading thread's sender).
    while let Ok(samples) = rx.recv_timeout(IDLE) {
        match transcribe(&context, &samples) {
            Ok(text) => say(&serde_json::json!({ "text": text })),
            Err(e) => say(&serde_json::json!({ "error": format!("{e:#}") })),
        }
    }
}

/// One line to the IDE, flushed at once: it waits on every reply.
fn say(value: &serde_json::Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}

fn load(model: &Path) -> Result<WhisperContext> {
    whisper_rs::install_logging_hooks();
    let path = model.to_str().context("the model path is not UTF-8")?;
    WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .with_context(|| format!("loading the speech model at {}", model.display()))
}

/// One request's samples, or `None` at the end of the input.
fn read_request(input: &mut impl Read) -> Result<Option<Vec<f32>>> {
    let mut count = [0u8; 4];
    match input.read_exact(&mut count) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let count = u32::from_le_bytes(count) as usize;
    if count > MAX_SAMPLES {
        bail!("{count} samples is more than one request may carry");
    }
    let mut bytes = vec![0u8; count * 4];
    input.read_exact(&mut bytes)?;
    Ok(Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
    ))
}

/// 16 kHz mono samples in, one line of text out.
fn transcribe(context: &WhisperContext, samples: &[f32]) -> Result<String> {
    let mut state = context.create_state().context("preparing to transcribe")?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_no_context(true);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    params.set_n_threads(threads as i32);
    state.full(params, samples).context("transcribing")?;

    let mut text = String::new();
    for segment in state.as_iter() {
        if let Ok(piece) = segment.to_str_lossy() {
            text.push_str(&piece);
        }
    }
    Ok(tidy(&text))
}

/// Whisper's text comes with leading spaces per segment and, on silence,
/// bracketed non-speech markers. One line, one space between words.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_join_with_single_spaces_and_markers_go() {
        assert_eq!(tidy(" Hello,  world. [BLANK_AUDIO]"), "Hello, world.");
        assert_eq!(tidy("(wind blowing) it works"), "it works");
        assert_eq!(tidy("   "), "");
    }

    /// A request is its length and its samples; the input's end is no
    /// request, and a length past the ceiling is refused, not allocated.
    #[test]
    fn a_request_is_read_whole_and_a_wild_length_refused() {
        let mut input = Vec::new();
        input.extend_from_slice(&3u32.to_le_bytes());
        for sample in [0.5f32, -0.25, 1.0] {
            input.extend_from_slice(&sample.to_le_bytes());
        }
        let mut reader = input.as_slice();
        assert_eq!(
            read_request(&mut reader).unwrap(),
            Some(vec![0.5, -0.25, 1.0])
        );
        assert_eq!(read_request(&mut reader).unwrap(), None);

        let wild = (MAX_SAMPLES as u32 + 1).to_le_bytes();
        assert!(read_request(&mut wild.as_slice()).is_err());
    }
}
