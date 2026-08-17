//! The tokio runtime backing all non-UI work.
//!
//! GTK owns the main thread; everything async (ACP, MCP, podman, watchers)
//! runs here and communicates back through `taste_core::EventBus` or
//! channels drained with `glib::spawn_future_local`.

use std::sync::OnceLock;

use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("tokio runtime"))
}
