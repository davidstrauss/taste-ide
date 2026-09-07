//! Voice input for the IDE: the microphone in, text out, all on this side
//! of the line. `docs/spikes/one-composer-and-evidence.md` → Voice.
//!
//! Three parts, each usable alone: [`model`] fetches and pins the speech
//! model, [`capture`] records from the desktop's audio server while the
//! user holds the button, and [`transcribe`] turns the samples into a line
//! of text with whisper.cpp on the CPU. GTK never appears here; the
//! composer drives it from the main thread and does the slow parts on the
//! blocking pool.

pub mod capture;
/// The pinned-model fetcher, shared with the semantic index
/// (`taste-models`); the speech model's own pin is here beside it.
pub mod model {
    pub use taste_models::*;

    /// `base.en`: English only, ~150 MB, a ten-second utterance in about a
    /// second on a desktop CPU. `small.en` is the upgrade if accuracy
    /// disappoints; it is a second constant, not a setting. The digest was
    /// computed from a real download on 2026-09-06 (its SHA-1 `137c4040…390c`
    /// matches whisper.cpp's own `download-ggml-model.sh`).
    pub const BASE_EN: ModelSpec = ModelSpec {
        name: "base.en",
        file: "ggml-base.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        bytes: 147_964_211,
    };
}
pub mod transcribe;

pub use capture::{has_speech, Recorder};
pub use model::{ModelSpec, BASE_EN};
pub use transcribe::Transcriber;
