//! Local speech-to-text: whisper.cpp through `whisper-rs`, on the CPU.
//!
//! Nothing leaves the machine. The auth proxy holds the one credential the
//! IDE has and Anthropic has no speech endpoint, so a cloud transcriber
//! would be a new credential, a new destination for the user's voice, and
//! a new hole in the line CLAUDE.md draws. Not worth what it buys.

use std::path::Path;

use anyhow::{Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// A loaded model. Loading takes a second or two and a few hundred MB, so
/// the app keeps one and reuses it; transcribing is a `&self` call.
pub struct Transcriber {
    context: WhisperContext,
}

impl Transcriber {
    pub fn load(model: &Path) -> Result<Self> {
        whisper_rs::install_logging_hooks();
        let path = model.to_str().context("the model path is not UTF-8")?;
        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .with_context(|| format!("loading the speech model at {}", model.display()))?;
        Ok(Self { context })
    }

    /// 16 kHz mono samples in, one line of text out. Runs to completion on
    /// the calling thread: callers put it on the blocking pool.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let mut state = self
            .context
            .create_state()
            .context("preparing to transcribe")?;
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
}

/// Whisper's text comes with leading spaces per segment and, on silence,
/// bracketed non-speech markers. One line, one space between words.
pub fn tidy(text: &str) -> String {
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
}
