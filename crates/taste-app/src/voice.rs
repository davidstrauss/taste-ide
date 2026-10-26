//! The app's side of voice input: whether the speech model is here, the
//! one download when it is not, and one transcriber shared by every
//! composer. `taste-voice` does the work, its model in a helper process
//! that starts when the microphone is pressed and exits when idle
//! (`taste_voice::transcribe`); this keeps the state that has to be
//! global, because two composers must not download the same 150 MB twice
//! or run two copies of the model.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::glib;
use taste_voice::{ModelSpec, Transcriber, BASE_EN};

pub const MODEL: ModelSpec = BASE_EN;

/// What the download reports, for a meter rather than a stream of toasts.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Started,
    Bytes { done: u64, total: u64 },
    Done,
    Failed(String),
}

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

/// The one transcriber, made the first time it is asked for. Making it
/// starts nothing.
fn transcriber() -> Arc<Transcriber> {
    shared()
        .lock()
        .expect("voice state")
        .transcriber
        .get_or_insert_with(|| Arc::new(Transcriber::new(&taste_voice::model::model_path(&MODEL))))
        .clone()
}

/// The microphone was pressed: start the speech helper now, off the main
/// thread, so its model loads while the user speaks rather than after.
pub fn prewarm() {
    if readiness() != Readiness::Ready {
        return;
    }
    crate::runtime::runtime().spawn_blocking(|| {
        if let Err(e) = transcriber().prewarm() {
            tracing::warn!("the speech helper did not start: {e:#}");
        }
    });
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

/// Start the download, once. Progress reaches `on_progress` on the main
/// thread: `Started`, then bytes no more often than every 200 ms, then
/// `Done` or `Failed`. The caller draws a meter; only the ends are words.
pub fn fetch_model(on_progress: impl Fn(Progress) + 'static) {
    {
        let mut state = shared().lock().expect("voice state");
        if state.downloading {
            return;
        }
        state.downloading = true;
    }
    let (tx, rx) = async_channel::unbounded::<Progress>();
    glib::spawn_future_local(async move {
        while let Ok(progress) = rx.recv().await {
            on_progress(progress);
        }
    });
    let progress_tx = tx.clone();
    let last = Mutex::new(Instant::now() - Duration::from_secs(2));
    crate::runtime::runtime().spawn(async move {
        let _ = progress_tx.send(Progress::Started).await;
        let report = progress_tx.clone();
        let result = taste_voice::model::download(&MODEL, move |done, total| {
            let mut last = last.lock().expect("progress clock");
            if last.elapsed() < Duration::from_millis(200) && done < total {
                return;
            }
            *last = Instant::now();
            let _ = report.try_send(Progress::Bytes { done, total });
        })
        .await;
        shared().lock().expect("voice state").downloading = false;
        let _ = progress_tx
            .send(match result {
                Ok(_) => Progress::Done,
                Err(e) => Progress::Failed(format!("{e:#}")),
            })
            .await;
    });
}

/// Transcribe on the blocking pool — starting the helper, and loading its
/// model, if no helper is running — and hand the text back on the main
/// thread.
pub fn transcribe(samples: Vec<f32>, done: impl FnOnce(Result<String, String>) + 'static) {
    let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            done(result);
        }
    });
    crate::runtime::runtime().spawn_blocking(move || {
        let result = transcriber().transcribe(&samples);
        let _ = tx.send_blocking(result.map_err(|e| format!("{e:#}")));
    });
}
