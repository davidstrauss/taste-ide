//! The semantic index's keeper: the GTK side of `taste-semantic`.
//!
//! Fetches the embedding model once (100 MB, pinned — `taste_models`),
//! builds the primary checkout's index when the window opens, and rebuilds
//! it — debounced — whenever git says the tree changed. Every heavy step is
//! off the main thread; what reaches the GTK thread is a toast the first
//! time the index is ready and a line in the IDE log for every refresh
//! after that (docs/spikes/agent-workspace-context.md).
//!
//! Only the primary checkout is kept here. An environment's clone is
//! indexed when an agent first asks (`ide_semantic_search` starts it), and
//! goes with the clone.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::glib;
use taste_core::{Event, Workspace};

use crate::search::{Indexing, Search};

/// How long the tree has to be quiet after a git change before the index
/// follows: a `git checkout` touches hundreds of files in a second, and
/// each one should not start a refresh.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Keeper {
    semantic: Arc<taste_semantic::Semantic>,
    workspace: Workspace,
    root: PathBuf,
    /// The box in the title bar, where the build's progress and the time
    /// left are shown (`Search::set_indexing`).
    search: std::rc::Weak<Search>,
    /// The refresh in flight, so a newer one can stop it between files.
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Called on the GTK thread when a refresh has landed: the window
    /// re-asks the index for the query on screen, which may have been typed
    /// while there was nothing to ask.
    on_indexed: RefCell<Option<Box<dyn Fn()>>>,
}

impl Keeper {
    /// Start keeping: fetch the model if it is not here, then index.
    ///
    /// Not under the probe (`TASTE_PROBE_CHECK`): a screenshot run must not
    /// fetch a model or start minutes of embedding, and the frames that
    /// show the index at work are fixtures (`window.rs`, the `search`
    /// view).
    pub fn start(
        semantic: Arc<taste_semantic::Semantic>,
        workspace: Workspace,
        search: std::rc::Weak<Search>,
    ) -> Rc<Self> {
        let keeper = Rc::new(Self {
            semantic,
            root: workspace.root().to_path_buf(),
            workspace,
            search,
            cancel: RefCell::new(None),
            timer: RefCell::new(None),
            on_indexed: RefCell::new(None),
        });
        if std::env::var_os("TASTE_PROBE_CHECK").is_some() {
            return keeper;
        }
        if taste_semantic::Semantic::model_present() {
            keeper.refresh_now();
        } else {
            keeper.fetch_model_then_index();
        }
        keeper
    }

    /// The model, once. The download is the IDE's own process talking to
    /// one host over TLS with the file's digest pinned; it is announced,
    /// because 100 MB is a thing a person on a metered link wants to know
    /// about, and it happens exactly once per machine.
    fn fetch_model_then_index(self: &Rc<Self>) {
        self.workspace.events.publish(Event::Toast(
            "Fetching the semantic search model (100 MB, once): agents will be able to \
             search this checkout by meaning"
                .into(),
        ));
        let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
        crate::runtime::runtime().spawn(async move {
            let result = taste_models::download(&taste_semantic::EMBEDDING, |_, _| {})
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result).await;
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(keeper) = weak.upgrade() else { return };
            match result {
                Ok(()) => keeper.refresh_now(),
                Err(e) => keeper.workspace.events.publish(Event::Toast(format!(
                    "The semantic search model could not be fetched: {e}"
                ))),
            }
        });
    }

    /// What to do when a refresh has landed.
    pub fn set_on_indexed(&self, hook: impl Fn() + 'static) {
        *self.on_indexed.borrow_mut() = Some(Box::new(hook));
    }

    /// The tree changed: refresh once it has been quiet for a moment.
    pub fn schedule_refresh(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(SETTLE, move || {
            if let Some(keeper) = weak.upgrade() {
                keeper.timer.borrow_mut().take();
                keeper.refresh_now();
            }
        });
        *self.timer.borrow_mut() = Some(id);
    }

    /// Refresh now, stopping a refresh already in flight between files —
    /// what it finished is kept and the new one carries on from there.
    fn refresh_now(self: &Rc<Self>) {
        if !taste_semantic::Semantic::model_present() {
            return;
        }
        if let Some(previous) = self.cancel.borrow_mut().take() {
            previous.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel.borrow_mut() = Some(cancel.clone());
        let semantic = self.semantic.clone();
        let root = self.root.clone();
        let (tx, rx) = async_channel::bounded::<anyhow::Result<taste_semantic::Report>>(1);
        // Progress, as it happens, to the box: the fraction and the time
        // left, estimated from the rate so far — after the plan pass has
        // said how much there is to do. The channel is drained to its
        // latest value per wakeup, so a fast helper is not a thousand
        // label updates.
        let (progress_tx, progress_rx) = async_channel::unbounded::<taste_semantic::Progress>();
        {
            let search = self.search.clone();
            glib::spawn_future_local(async move {
                let started = std::time::Instant::now();
                while let Ok(mut latest) = progress_rx.recv().await {
                    while let Ok(newer) = progress_rx.try_recv() {
                        latest = newer;
                    }
                    let Some(search) = search.upgrade() else {
                        return;
                    };
                    if latest.chunks_total == 0 {
                        continue;
                    }
                    let eta = if latest.chunks_embedded > 0 {
                        let per_chunk =
                            started.elapsed().as_secs_f64() / latest.chunks_embedded as f64;
                        let left = latest.chunks_total.saturating_sub(latest.chunks_embedded);
                        Some(std::time::Duration::from_secs_f64(per_chunk * left as f64))
                    } else {
                        None
                    };
                    search.set_indexing(Some(Indexing {
                        done: latest.chunks_embedded,
                        total: latest.chunks_total,
                        eta,
                    }));
                }
                if let Some(search) = search.upgrade() {
                    search.set_indexing(None);
                }
            });
        }
        crate::runtime::runtime().spawn_blocking(move || {
            let started = std::time::Instant::now();
            let result = semantic.refresh(&root, &cancel, |progress| {
                let _ = progress_tx.try_send(progress);
            });
            drop(progress_tx);
            if let Ok(report) = &result {
                tracing::info!(
                    "semantic index: {} files, {} chunks, {} embedded, {} removed, {:.1}s{}",
                    report.files,
                    report.chunks,
                    report.embedded,
                    report.removed_files,
                    started.elapsed().as_secs_f64(),
                    if report.cancelled {
                        " (stopped early)"
                    } else {
                        ""
                    }
                );
            }
            let _ = tx.send_blocking(result);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(keeper) = weak.upgrade() else { return };
            match result {
                Ok(report) if report.cancelled => {}
                Ok(report) => {
                    // No toast: the meaning button's pill going away IS the
                    // announcement (David, 2026-09-07: "Don't show the toast
                    // when indexing finishes"). The log keeps the numbers.
                    tracing::info!(
                        "semantic index current: {} files in {} chunks",
                        report.files,
                        report.chunks
                    );
                    if let Some(hook) = keeper.on_indexed.borrow().as_ref() {
                        hook();
                    }
                }
                Err(e) => tracing::warn!("semantic index: {e:#}"),
            }
        });
    }
}
