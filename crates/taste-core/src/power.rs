//! Whether this machine is running on its battery.
//!
//! What the IDE would do in the background by its own choice — building
//! the semantic index, minutes of embedding at full tilt — waits while the
//! machine is on battery (David, 2026-10-08: "Pause semantic index building
//! when on battery"). What a person or an agent asks for is not held.
//!
//! The answer is UPower's `OnBattery`, read by the app over the system bus
//! (`taste-app`'s `power.rs`); everything below the app reads this flag.
//! With no UPower to ask, the machine is taken to be on mains power, which
//! pauses nothing.

use std::sync::atomic::{AtomicBool, Ordering};

static ON_BATTERY: AtomicBool = AtomicBool::new(false);

/// Whether the machine is on battery, as UPower last said.
pub fn on_battery() -> bool {
    ON_BATTERY.load(Ordering::Relaxed)
}

/// What UPower says now. True when that changed the answer.
pub fn set_on_battery(on_battery: bool) -> bool {
    ON_BATTERY.swap(on_battery, Ordering::Relaxed) != on_battery
}
