//! Probe GtkSourceMap's internals: child widget tree, CSS names, and the
//! slider's allocation — ground truth for styling the viewport highlight.

use adw::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

fn main() {
    let app = adw::Application::builder()
        .application_id("net.davidstrauss.TasteMapProbe")
        .build();
    app.connect_activate(|app| {
        let buffer = sourceview5::Buffer::new(None);
        let text: String = (1..=300)
            .map(|i| format!("line {i} with some content\n"))
            .collect();
        buffer.set_text(&text);
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_monospace(true);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hexpand(true)
            .vexpand(true)
            .build();
        let map = sourceview5::Map::new();
        map.set_view(&view);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&scroller);
        body.append(&map);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(700)
            .default_height(400)
            .content(&body)
            .build();
        window.present();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            fn walk(widget: &gtk::Widget, depth: usize) {
                println!(
                    "{}{} css_name={} classes={:?} bounds={:?} visible={}",
                    "  ".repeat(depth),
                    widget.type_().name(),
                    widget.css_name(),
                    widget.css_classes(),
                    widget
                        .compute_bounds(&widget.parent().unwrap_or_else(|| widget.clone()))
                        .map(|b| (b.x(), b.y(), b.width(), b.height())),
                    widget.is_visible(),
                );
                let mut child = widget.first_child();
                while let Some(current) = child {
                    walk(&current, depth + 1);
                    child = current.next_sibling();
                }
            }
            walk(map.upcast_ref(), 0);
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[]);
}
