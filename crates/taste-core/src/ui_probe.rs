//! On-demand questions for the GTK main thread.
//!
//! The MCP server answers agents from tokio; a few answers (a rendered
//! screenshot, computed widget geometry) only exist on the GTK side. This
//! is the request/reply seam between them: tokio sends a [`UiRequest`]
//! with a reply channel, the GTK side drains [`UiProbe::requests`] with
//! `glib::spawn_future_local` and answers with GTK-free types. Nothing
//! GTK-shaped crosses; the reply is bytes and JSON.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

/// What the MCP server can ask of the UI. `target` names a pane from the
/// window's registry ("chat", "editor", …), optionally dotted with a
/// descendant widget name ("chat.composer").
#[derive(Debug, Clone)]
pub enum UiRequest {
    /// Render the target as it is on screen right now.
    Screenshot { target: String },
    /// Dump the target's widget subtree with computed geometry.
    Geometry { target: String },
}

#[derive(Debug, Clone)]
pub enum UiReply {
    Screenshot {
        png: Vec<u8>,
        width: i32,
        height: i32,
    },
    Geometry(serde_json::Value),
    /// The UI could not answer (unknown target, widget not rendered).
    Error(String),
}

type Envelope = (UiRequest, async_channel::Sender<UiReply>);

/// Cloneable handle carried on the [`crate::Workspace`].
#[derive(Clone)]
pub struct UiProbe {
    tx: async_channel::Sender<Envelope>,
    rx: async_channel::Receiver<Envelope>,
    /// Set once the GTK side starts draining. Requests before that fail
    /// fast instead of hanging a tool call against a headless workspace.
    attached: Arc<AtomicBool>,
}

impl Default for UiProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl UiProbe {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            tx,
            rx,
            attached: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The GTK side's end. Calling this declares "a UI is listening".
    pub fn requests(&self) -> async_channel::Receiver<Envelope> {
        self.attached.store(true, Ordering::Release);
        self.rx.clone()
    }

    /// Ask the UI and await its answer. Callers add their own timeout —
    /// a wedged main thread must show up as a tool error, not a hang.
    pub async fn request(&self, request: UiRequest) -> Result<UiReply> {
        if !self.attached.load(Ordering::Acquire) {
            anyhow::bail!("no UI is attached to this workspace");
        }
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.tx
            .send((request, reply_tx))
            .await
            .map_err(|_| anyhow::anyhow!("UI probe channel closed"))?;
        reply_rx
            .recv()
            .await
            .map_err(|_| anyhow::anyhow!("UI dropped the request without answering"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattached_probe_fails_fast() {
        let probe = UiProbe::new();
        let result = block_on(probe.request(UiRequest::Screenshot {
            target: "chat".into(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn attached_probe_roundtrips() {
        let probe = UiProbe::new();
        let requests = probe.requests();
        let responder = std::thread::spawn(move || {
            let (request, reply) = requests.recv_blocking().unwrap();
            assert!(matches!(request, UiRequest::Geometry { .. }));
            reply
                .send_blocking(UiReply::Geometry(serde_json::json!({"ok": true})))
                .unwrap();
        });
        let reply = block_on(probe.request(UiRequest::Geometry {
            target: "editor".into(),
        }))
        .unwrap();
        assert!(matches!(reply, UiReply::Geometry(_)));
        responder.join().unwrap();
    }

    /// A minimal block_on (park/unpark): these futures only await channels.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct Unparker(std::thread::Thread);
        impl std::task::Wake for Unparker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::from(Arc::new(Unparker(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }
}
