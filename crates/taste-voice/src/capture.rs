//! The microphone, through GStreamer: 16 kHz mono float samples, which is
//! what whisper wants, collected for as long as the button is held.
//!
//! `autoaudiosrc` picks the desktop's source — PipeWire directly, or its
//! PulseAudio compatibility socket — so the same pipeline runs on the
//! host, in the self-hosting container (bootstrap.sh mounts the sockets)
//! and in the Flatpak (`--socket=pulseaudio`). Nothing is captured while
//! no recorder exists: there is no always-on stream to mute.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

pub const SAMPLE_RATE: u32 = 16_000;

/// One recording, from `start` to `stop`.
pub struct Recorder {
    pipeline: gst::Pipeline,
    samples: Arc<Mutex<Vec<f32>>>,
    /// RMS of the most recent buffer, as `f32` bits — the level the button
    /// draws while held.
    level: Arc<AtomicU32>,
}

impl Recorder {
    /// Open the default source and start collecting. Fails — with the
    /// pipeline's own message — when there is no microphone to open, so a
    /// dead input is visible before any silence is transcribed.
    pub fn start() -> Result<Self> {
        gst::init().context("initialising GStreamer")?;
        let pipeline = gst::parse::launch(&format!(
            "autoaudiosrc ! audioconvert ! audioresample ! \
             audio/x-raw,format=F32LE,rate={SAMPLE_RATE},channels=1,layout=interleaved ! \
             appsink name=sink sync=false"
        ))
        .context("building the capture pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("the capture pipeline is not a pipeline"))?;

        let sink = pipeline
            .by_name("sink")
            .context("the capture pipeline has no sink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("the sink is not an appsink"))?;

        let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
        let level = Arc::new(AtomicU32::new(0));
        {
            let samples = samples.clone();
            let level = level.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let bytes = map.as_slice();
                        let chunk: Vec<f32> = bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|frame| f32::from_le_bytes(*frame))
                            .collect();
                        level.store(rms(&chunk).to_bits(), Ordering::Relaxed);
                        if let Ok(mut all) = samples.lock() {
                            all.extend_from_slice(&chunk);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        pipeline
            .set_state(gst::State::Playing)
            .context("starting the microphone")?;
        // Wait for the pipeline to actually reach Playing, and read the
        // bus for the reason when it does not: "no such device" is a
        // sentence the user can act on, "state change failed" is not.
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(2));
        if result.is_err() {
            let why = pipeline
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                .and_then(|message| match message.view() {
                    gst::MessageView::Error(err) => Some(err.error().to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "the audio source did not start".to_string());
            let _ = pipeline.set_state(gst::State::Null);
            bail!("no microphone: {why}");
        }
        Ok(Self {
            pipeline,
            samples,
            level,
        })
    }

    /// The most recent buffer's RMS, 0.0 to about 1.0.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// How long has been captured so far.
    pub fn seconds(&self) -> f32 {
        self.samples
            .lock()
            .map(|s| s.len() as f32 / SAMPLE_RATE as f32)
            .unwrap_or(0.0)
    }

    /// Stop the source and hand over everything it heard.
    /// Everything captured so far, without ending the recording.
    ///
    /// For transcribing while the microphone is still open: the caller
    /// re-runs the model over a growing clip and shows what it hears, the
    /// way whisper.cpp's own stream example does. A clone rather than a
    /// borrow, so the capture callback is never blocked behind whoever is
    /// reading.
    pub fn samples_so_far(&self) -> Vec<f32> {
        self.samples.lock().expect("captured samples").clone()
    }

    pub fn stop(self) -> Vec<f32> {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.samples.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Whether a recording is worth transcribing at all: whisper hallucinates
/// prose on pure silence, so a held button with nothing said produces
/// nothing rather than "Thank you for watching".
pub fn has_speech(samples: &[f32]) -> bool {
    const FLOOR: f32 = 0.004;
    const MIN_SECONDS: f32 = 0.3;
    samples.len() as f32 / SAMPLE_RATE as f32 >= MIN_SECONDS && rms(samples) > FLOOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_blips_are_not_speech() {
        assert!(!has_speech(&[]));
        assert!(!has_speech(&vec![0.0; SAMPLE_RATE as usize]));
        let blip: Vec<f32> = (0..2_000).map(|i| ((i as f32) * 0.3).sin() * 0.5).collect();
        assert!(!has_speech(&blip), "a tenth of a second is a click");
        let tone: Vec<f32> = (0..SAMPLE_RATE)
            .map(|i| ((i as f32) * 0.3).sin() * 0.2)
            .collect();
        assert!(has_speech(&tone));
        assert!((rms(&[0.5, -0.5]) - 0.5).abs() < 1e-6);
    }
}
