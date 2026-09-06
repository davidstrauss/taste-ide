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
pub mod model;
pub mod transcribe;

pub use capture::{has_speech, Recorder};
pub use model::{ModelSpec, BASE_EN};
pub use transcribe::Transcriber;
