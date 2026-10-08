//! Battery or mains, from UPower (`taste_core::power`).
//!
//! UPower's `OnBattery` on the system bus is the desktop's own answer — the
//! one GNOME's battery indicator reads — and the intended interface for it;
//! GIO's power-profile monitor says only whether power saving is on, which
//! is a different question. Inside the Flatpak the manifest grants the one
//! name (`--system-talk-name=org.freedesktop.UPower`).
//!
//! The proxy is made asynchronously, so the GTK thread never waits on the
//! bus, and its property-change signal keeps the flag current. Whatever
//! wants to act on a change subscribes with [`on_change`], on this thread.

use std::cell::RefCell;

use gtk::gio;
use gtk::prelude::*;

thread_local! {
    /// The proxy, kept for as long as the process, so its signal lives.
    static PROXY: RefCell<Option<gio::DBusProxy>> = const { RefCell::new(None) };
    static SUBSCRIBERS: RefCell<Vec<Box<dyn Fn(bool)>>> = const { RefCell::new(Vec::new()) };
}

/// Call `hook` with the new answer whenever it changes. On the GTK thread.
pub fn on_change(hook: impl Fn(bool) + 'static) {
    SUBSCRIBERS.with(|subscribers| subscribers.borrow_mut().push(Box::new(hook)));
}

/// Start watching UPower. Once per process.
pub fn watch() {
    if PROXY.with(|proxy| proxy.borrow().is_some()) {
        return;
    }
    gio::DBusProxy::for_bus(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
        gio::Cancellable::NONE,
        |result| {
            let proxy = match result {
                Ok(proxy) => proxy,
                Err(e) => {
                    tracing::info!("no UPower to ask about the battery ({e}); taken as mains");
                    return;
                }
            };
            apply(&proxy);
            proxy.connect_g_properties_changed(|proxy, _, _| apply(proxy));
            PROXY.with(|slot| *slot.borrow_mut() = Some(proxy));
        },
    );
}

/// Read `OnBattery` off the proxy's cache and tell the subscribers when it
/// changed. A missing property — no UPower running — is mains.
fn apply(proxy: &gio::DBusProxy) {
    let on_battery = proxy
        .cached_property("OnBattery")
        .and_then(|value| value.get::<bool>())
        .unwrap_or(false);
    if taste_core::power::set_on_battery(on_battery) {
        tracing::info!(
            "power: {}",
            if on_battery {
                "on battery; background indexing waits"
            } else {
                "on mains"
            }
        );
        SUBSCRIBERS.with(|subscribers| {
            for hook in subscribers.borrow().iter() {
                hook(on_battery);
            }
        });
    }
}
