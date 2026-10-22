//! Probe whether this app's `GtkApplication::inhibit` reaches the desktop:
//! the same flags the app runs with (`NON_UNIQUE`), a window, an inhibit
//! of suspend and idle, and the cookie it got, printed. Run it on the
//! desktop and list the session's inhibitors while it waits. Needs a
//! display; not part of the app.

use adw::prelude::*;
use gtk::glib;

fn main() {
    let app = adw::Application::builder()
        .application_id("dev.taste.InhibitProbe")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(200)
            .default_height(100)
            .build();
        window.present();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            let window = app.active_window();
            let cookie = app.inhibit(
                window.as_ref(),
                gtk::ApplicationInhibitFlags::SUSPEND | gtk::ApplicationInhibitFlags::IDLE,
                Some("Presenting"),
            );
            println!("inhibit cookie: {cookie}");
            glib::timeout_add_local_once(std::time::Duration::from_secs(6), move || {
                app.quit();
            });
        });
    });
    app.run_with_args::<&str>(&[]);
}
