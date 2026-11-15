//! Whether this machine's connection is metered, what the desktop says it
//! reaches, and whether it has an IPv6 loopback.
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

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static METERED: AtomicBool = AtomicBool::new(false);

/// Whether the connection is metered, as the desktop last said.
pub fn metered() -> bool {
    METERED.load(Ordering::Relaxed)
}

/// What the desktop says now. True when that changed the answer.
pub fn set_metered(metered: bool) -> bool {
    METERED.swap(metered, Ordering::Relaxed) != metered
}

/// What the desktop says the connection reaches: GIO's connectivity
/// levels, read the same way as [`metered`]. What the auth proxy names when
/// the API cannot be reached, since a network held behind a sign-in page
/// (a captive portal) wants the user to sign in, which nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The internet, as far as the desktop can tell.
    Full,
    /// A network that asks for a sign-in before it reaches anything.
    Portal,
    /// A network, but not the internet.
    Limited,
    /// No network but this machine's own.
    Local,
}

static REACH: AtomicU8 = AtomicU8::new(0);

/// What the connection reaches, as the desktop last said.
pub fn reach() -> Reach {
    match REACH.load(Ordering::Relaxed) {
        1 => Reach::Portal,
        2 => Reach::Limited,
        3 => Reach::Local,
        _ => Reach::Full,
    }
}

/// What the desktop says now. True when that changed the answer.
pub fn set_reach(reach: Reach) -> bool {
    let value = match reach {
        Reach::Full => 0,
        Reach::Portal => 1,
        Reach::Limited => 2,
        Reach::Local => 3,
    };
    REACH.swap(value, Ordering::Relaxed) != value
}

/// Whether this machine has an IPv6 loopback to listen on: a bind to
/// `[::1]:0` that succeeds, asked once. A forwarded port listens on both
/// loopbacks where there are two, since it is reached as `localhost`
/// (`taste_devcontainer::PortSpec::url`); a host with IPv6 switched off
/// has one, and a bind to `::1` there fails rather than meaning anything.
pub fn ipv6_loopback() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| std::net::TcpListener::bind(("::1", 0)).is_ok())
}
