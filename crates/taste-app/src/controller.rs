//! A game controller as a way in (David, 2026-09-07: "using a game
//! controller as an accessibility option for driving focus and controlling
//! things"). Read straight off evdev — GTK has no gamepad API — on a thread
//! of its own, and handed to the window as [`Event::Controller`] through
//! the event bus, which is the only road from any other thread to the GTK
//! one.
//!
//! Xbox layout, by the kernel's own names: the `xpad` driver reports A as
//! `BTN_SOUTH`, B as `BTN_EAST`, X as `BTN_NORTH` and Y as `BTN_WEST`. What
//! the buttons DO is the composer's business (compose.rs).
//!
//! A gamepad's device node is readable by the seat's user — systemd's
//! uaccess rules tag joysticks — and inside a sandbox only when it is let
//! in: the Flatpak asks for `--device=input`, the self-hosting run mounts
//! `/dev/input`. Where there is no such node, nothing here logs more than
//! once.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use taste_core::{ControllerButton, Event, EventBus};

/// How often the input directory is looked at again for a pad plugged in
/// after the app started.
const RESCAN: Duration = Duration::from_secs(3);

/// Start listening. Returns at once; the work is on its own thread.
pub fn start(events: EventBus) {
    let spawned = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || watch(events));
    if let Err(e) = spawned {
        tracing::warn!("controller: no thread for it: {e}");
    }
}

fn watch(events: EventBus) {
    let mut open: HashSet<PathBuf> = HashSet::new();
    let (closed_tx, closed_rx) = mpsc::channel::<PathBuf>();
    let mut said_none = false;
    loop {
        while let Ok(path) = closed_rx.try_recv() {
            open.remove(&path);
        }
        let mut seen_any = false;
        for (path, device) in evdev::enumerate() {
            seen_any = true;
            if open.contains(&path) || !is_gamepad(&device) {
                continue;
            }
            tracing::info!(
                "controller: {} at {}",
                device.name().unwrap_or("a gamepad"),
                path.display()
            );
            open.insert(path.clone());
            let events = events.clone();
            let closed = closed_tx.clone();
            let spawned = std::thread::Builder::new()
                .name("controller-read".into())
                .spawn(move || {
                    read(device, &events);
                    let _ = closed.send(path);
                });
            if let Err(e) = spawned {
                tracing::warn!("controller: no reader thread: {e}");
            }
        }
        if !seen_any && !said_none {
            said_none = true;
            tracing::debug!("controller: no input devices are readable from here");
        }
        std::thread::sleep(RESCAN);
    }
}

/// A pad, not a keyboard or a mouse: it has the face buttons and a stick.
fn is_gamepad(device: &evdev::Device) -> bool {
    let keys = device.supported_keys();
    let has_face = keys.is_some_and(|keys| {
        keys.contains(evdev::Key::BTN_SOUTH) && keys.contains(evdev::Key::BTN_EAST)
    });
    let has_stick = device
        .supported_absolute_axes()
        .is_some_and(|axes| axes.contains(evdev::AbsoluteAxisType::ABS_X));
    has_face && has_stick
}

/// The buttons the IDE answers to, by the kernel's names for the Xbox
/// layout; anything else on the pad is not ours yet.
fn button(key: evdev::Key) -> Option<ControllerButton> {
    Some(match key {
        evdev::Key::BTN_SOUTH => ControllerButton::A,
        evdev::Key::BTN_EAST => ControllerButton::B,
        evdev::Key::BTN_NORTH => ControllerButton::X,
        evdev::Key::BTN_WEST => ControllerButton::Y,
        evdev::Key::BTN_TL => ControllerButton::LeftShoulder,
        evdev::Key::BTN_TR => ControllerButton::RightShoulder,
        // Pads that report the triggers as buttons; xpad reports them as
        // axes (`ABS_Z`, `ABS_RZ`), read below.
        evdev::Key::BTN_TL2 => ControllerButton::LeftTrigger,
        evdev::Key::BTN_TR2 => ControllerButton::RightTrigger,
        evdev::Key::BTN_START => ControllerButton::Start,
        evdev::Key::BTN_MODE => ControllerButton::Guide,
        // Some pads report the D-pad as buttons…
        evdev::Key::BTN_DPAD_UP => ControllerButton::Up,
        evdev::Key::BTN_DPAD_DOWN => ControllerButton::Down,
        _ => return None,
    })
}

/// Until the device goes away.
fn read(mut device: evdev::Device, events: &EventBus) {
    // …and the xpad driver reports it as an axis: -1 up, 1 down, 0 for
    // neither, so a release is "whichever was down comes up".
    let mut hat: Option<ControllerButton> = None;
    // The triggers as axes: pressed past halfway is down, back below it is
    // up, and only the crossings are published — an analog value that
    // published every step would be a hundred presses per pull.
    let halfway = |axis: evdev::AbsoluteAxisType| -> i32 {
        device
            .get_abs_state()
            .ok()
            .and_then(|states| states.get(axis.0 as usize).map(|info| info.maximum / 2))
            .filter(|half| *half > 0)
            .unwrap_or(512)
    };
    let (left_half, right_half) = (
        halfway(evdev::AbsoluteAxisType::ABS_Z),
        halfway(evdev::AbsoluteAxisType::ABS_RZ),
    );
    let (mut left_down, mut right_down) = (false, false);
    loop {
        let batch = match device.fetch_events() {
            Ok(batch) => batch,
            Err(e) => {
                tracing::info!("controller: gone ({e})");
                return;
            }
        };
        for event in batch {
            match event.kind() {
                evdev::InputEventKind::Key(key) => {
                    let Some(button) = button(key) else { continue };
                    // 1 is down, 0 up; 2 is the kernel's key repeat, which
                    // a held button must not turn into a second press.
                    let pressed = match event.value() {
                        1 => true,
                        0 => false,
                        _ => continue,
                    };
                    events.publish(Event::Controller { button, pressed });
                }
                evdev::InputEventKind::AbsAxis(
                    axis @ (evdev::AbsoluteAxisType::ABS_Z | evdev::AbsoluteAxisType::ABS_RZ),
                ) => {
                    let (button, half, down) = if axis == evdev::AbsoluteAxisType::ABS_Z {
                        (ControllerButton::LeftTrigger, left_half, &mut left_down)
                    } else {
                        (ControllerButton::RightTrigger, right_half, &mut right_down)
                    };
                    let pressed = event.value() > half;
                    if pressed != *down {
                        *down = pressed;
                        events.publish(Event::Controller { button, pressed });
                    }
                }
                evdev::InputEventKind::AbsAxis(evdev::AbsoluteAxisType::ABS_HAT0Y) => {
                    let now = match event.value() {
                        v if v < 0 => Some(ControllerButton::Up),
                        v if v > 0 => Some(ControllerButton::Down),
                        _ => None,
                    };
                    if now == hat {
                        continue;
                    }
                    if let Some(released) = hat.take() {
                        events.publish(Event::Controller {
                            button: released,
                            pressed: false,
                        });
                    }
                    if let Some(pressed) = now {
                        events.publish(Event::Controller {
                            button: pressed,
                            pressed: true,
                        });
                    }
                    hat = now;
                }
                _ => {}
            }
        }
    }
}
