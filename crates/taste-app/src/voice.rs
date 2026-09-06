//! The app's side of voice input: whether the speech model is here, the
//! one download when it is not, and a transcriber loaded once and shared
//! by every composer. `taste-voice` does the work; this keeps the state
//! that has to be global, because two composers must not download the
//! same 150 MB twice or hold two copies of the model.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::glib;
use taste_voice::{ModelSpec, Transcriber, BASE_EN};

pub const MODEL: ModelSpec = BASE_EN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Absent,
    Downloading,
}

struct Shared {
    downloading: bool,
    transcriber: Option<Arc<Transcriber>>,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        Mutex::new(Shared {
            downloading: false,
            transcriber: None,
        })
    })
}

pub fn readiness() -> Readiness {
    if shared().lock().is_ok_and(|s| s.downloading) {
        return Readiness::Downloading;
    }
    if taste_voice::model::is_present(&MODEL) {
        Readiness::Ready
    } else {
        Readiness::Absent
    }
}

/// Start the download, once. Progress reaches `notice` as sentences, no
/// more than one a second, and the last one says the microphone is ready.
pub fn fetch_model(notice: impl Fn(String) + 'static) {
    {
        let mut state = shared().lock().expect("voice state");
        if state.downloading {
            return;
        }
        state.downloading = true;
    }
    let (tx, rx) = async_channel::unbounded::<String>();
    glib::spawn_future_local(async move {
        while let Ok(text) = rx.recv().await {
            notice(text);
        }
    });
    let progress_tx = tx.clone();
    let last = Mutex::new(Instant::now() - Duration::from_secs(2));
    crate::runtime::runtime().spawn(async move {
        let _ = progress_tx
            .send(format!(
                "Downloading the speech model ({}, {} MB) — the microphone works once it lands",
                MODEL.name,
                MODEL.bytes / (1024 * 1024)
            ))
            .await;
        let report = progress_tx.clone();
        let result = taste_voice::model::download(&MODEL, move |done, total| {
            let mut last = last.lock().expect("progress clock");
            if last.elapsed() < Duration::from_secs(1) || total == 0 {
                return;
            }
            *last = Instant::now();
            let _ = report.try_send(format!(
                "Downloading the speech model — {}%",
                done * 100 / total
            ));
        })
        .await;
        shared().lock().expect("voice state").downloading = false;
        let _ = progress_tx
            .send(match result {
                Ok(_) => "Speech model ready — hold the microphone to talk".to_string(),
                Err(e) => format!("The speech model could not be fetched: {e:#}"),
            })
            .await;
    });
}

/// Transcribe on the blocking pool — loading the model the first time —
/// and hand the text back on the main thread.
pub fn transcribe(samples: Vec<f32>, done: impl FnOnce(Result<String, String>) + 'static) {
    let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            done(result);
        }
    });
    crate::runtime::runtime().spawn_blocking(move || {
        let result = (|| -> anyhow::Result<String> {
            let transcriber = {
                let existing = shared().lock().expect("voice state").transcriber.clone();
                match existing {
                    Some(t) => t,
                    None => {
                        let loaded =
                            Arc::new(Transcriber::load(&taste_voice::model::model_path(&MODEL))?);
                        shared().lock().expect("voice state").transcriber = Some(loaded.clone());
                        loaded
                    }
                }
            };
            transcriber.transcribe(&samples)
        })();
        let _ = tx.send_blocking(result.map_err(|e| format!("{e:#}")));
    });
}
