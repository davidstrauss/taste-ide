//! Whether this machine's connection is metered.
//!
//! A metered connection — a phone's hotspot, a capped plan — is one where
//! the IDE waits with what it would download by its own choice: a guest
//! image for a new release, a rebuild it would nudge an agent towards, a
//! package check's metadata (David, 2026-10-06: "I want the IDE to respect
//! metered connections"). What is downloaded out of necessity still is: an
//! agent that rebuilds its environment asked for it, and a VM with no image
//! on this machine at all has nothing else to boot.
//!
//! The answer is the desktop's, through GIO's network monitor (the portal's
//! inside the Flatpak). GIO is the app's to hold, so the app keeps this flag
//! current and everything below it reads the flag.

use std::sync::atomic::{AtomicBool, Ordering};

static METERED: AtomicBool = AtomicBool::new(false);

/// Whether the connection is metered, as the desktop last said.
pub fn metered() -> bool {
    METERED.load(Ordering::Relaxed)
}

/// What the desktop says now. True when that changed the answer.
pub fn set_metered(metered: bool) -> bool {
    METERED.swap(metered, Ordering::Relaxed) != metered
}
