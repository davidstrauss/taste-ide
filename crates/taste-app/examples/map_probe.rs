//! Probe GtkSourceMap ground truth: the slider's child node, its
//! allocation while scrolled, and — via pixel sampling of a rendered
//! frame — whether it actually PAINTS. This last check is what caught the
//! opaque map text layer hiding a perfectly allocated slider (the CSS
//! under test mirrors main.rs). Needs a display; not part of the app.

use adw::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

fn main() {
    let app = adw::Application::builder()
        .application_id("dev.taste.MapProbe")
        .build();
    app.connect_activate(|app| {
        // Same CSS the app installs (the map rules under test).
        if let Some(display) = gtk::gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_string(
                "textview.GtkSourceMap text { background: transparent; }\n\
                 textview.GtkSourceMap > slider { \
                   background-color: alpha(@accent_bg_color, 0.25); \
                   border-radius: 2px; }\n\
                 textview.GtkSourceMap > slider:hover { \
                   background-color: alpha(@accent_bg_color, 0.4); }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let content: String = (1..=800)
            .map(|i| format!("fn line_{i}() {{ let value = {i}; }}\n"))
            .collect();
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_text(&content);
        if let Some(scheme) =
            sourceview5::StyleSchemeManager::default().scheme("Adwaita-dark")
        {
            buffer.set_style_scheme(Some(&scheme));
        }
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_monospace(true);
        view.set_hexpand(true);
        view.set_vexpand(true);

        // The app's (fixed) order: scroller first, then map.set_view —
        // GtkSourceMap binds the view's vadjustment exactly once, there.
        let scroller = gtk::ScrolledWindow::builder().child(&view).build();
        let map = sourceview5::Map::new();
        map.set_view(&view);
        map.set_highlight_current_line(false);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&scroller);
        body.append(&map);
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .default_width(600)
            .default_height(400)
            .child(&body)
            .build();
        window.present();

        let map_for_probe = map.clone();
        let view_for_probe = view.clone();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            // Scroll to the middle so the slider sits mid-map.
            let vadj = view_for_probe.vadjustment().unwrap();
            vadj.set_value((vadj.upper() - vadj.page_size()) * 0.5);
            let map = map_for_probe.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                let mut child = map.first_child();
                let mut slider = None;
                while let Some(widget) = child {
                    println!(
                        "map child: {} css_name={}",
                        widget.type_(),
                        widget.css_name()
                    );
                    if widget.css_name() == "slider" {
                        slider = Some(widget.clone());
                    }
                    child = widget.next_sibling();
                }
                match slider {
                    Some(slider) => {
                        println!("slider visible={}", slider.is_visible());
                        match slider.compute_bounds(&map) {
                            Some(b) => println!(
                                "slider bounds in map: x={} y={} w={} h={}",
                                b.x(),
                                b.y(),
                                b.width(),
                                b.height()
                            ),
                            None => println!("slider bounds: NONE"),
                        }
                        println!("map size: {}x{}", map.width(), map.height());
                    }
                    None => println!("NO SLIDER CHILD FOUND"),
                }
                // Render the map subtree and sample pixels inside/outside
                // the slider region. NOTE: download() is BGRA.
                let paintable = gtk::WidgetPaintable::new(Some(&map));
                let snapshot = gtk::Snapshot::new();
                paintable.snapshot(
                    &snapshot,
                    f64::from(map.width()),
                    f64::from(map.height()),
                );
                if let Some(node) = snapshot.to_node() {
                    let renderer = map.native().unwrap().renderer().unwrap();
                    let texture = renderer.render_texture(&node, None);
                    let w = texture.width();
                    let h = texture.height();
                    let mut data = vec![0u8; (w * h * 4) as usize];
                    texture.download(&mut data, (w * 4) as usize);
                    let sample = |x: i32, y: i32| {
                        let o = ((y * w + x) * 4) as usize;
                        (data[o], data[o + 1], data[o + 2], data[o + 3])
                    };
                    // mid falls inside the slider (we scrolled to 50%);
                    // top/bot are plain map. Differing mid = slider paints.
                    println!("pixel mid   {:?}", sample(w / 2, h / 2));
                    println!("pixel top   {:?}", sample(w / 2, 5));
                    println!("pixel bot   {:?}", sample(w / 2, h - 5));
                } else {
                    println!("empty snapshot node");
                }
                app.quit();
            });
        });
    });
    app.run_with_args::<&str>(&[]);
}
