//! Ground truth for the composer-vs-search parity work: builds the exact
//! widget shapes with the exact app CSS and prints allocated heights.

use adw::prelude::*;
use gtk::glib;

fn main() {
    let app = adw::Application::builder()
        .application_id("net.davidstrauss.TasteMeasure")
        .build();
    app.connect_activate(|app| {
        if let Some(display) = gtk::gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_string(
                ".prompt-entry { background-color: @view_bg_color; \
                   border: 1px solid @borders; border-radius: 6px; \
                   padding: 0 4px; min-height: 34px; }\n\
                 .prompt-entry textview, .prompt-entry textview > text { \
                   background: transparent; }\n\
                 .prompt-entry entry.flat-entry { background: transparent; \
                   border: none; box-shadow: none; outline: none; \
                   min-height: 32px; }\n\
                 .prompt-entry:focus-within { \
                   border-color: @accent_bg_color; }\n\
                 .composer-action, .composer-action > button, \
                 button.composer-action, button.composer-action.circular { \
                   min-width: 26px; min-height: 26px; padding: 2px; \
                   margin: 0; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_margin_top(12);
        column.set_margin_start(12);
        column.set_margin_end(12);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Find in project")
            .build();
        column.append(&search);

        // chat-style composer (attach MenuButton, multiline field, send)
        let attach = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["flat"])
            .build();
        let entry = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&entry)
            .min_content_height(0)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let send = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .css_classes(["flat", "circular"])
            .build();
        let field = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field.set_hexpand(true);
        field.append(&scroller);
        let capsule = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        capsule.add_css_class("prompt-entry");
        for widget in [
            attach.clone().upcast::<gtk::Widget>(),
            send.clone().upcast(),
        ] {
            widget.add_css_class("composer-action");
            widget.set_valign(gtk::Align::End);
            widget.set_margin_top(4);
            widget.set_margin_bottom(4);
        }
        capsule.append(&attach);
        capsule.append(&field);
        capsule.append(&send);
        column.append(&capsule);

        // Variant B: bare TextView, no scroller.
        let entry_b = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .hexpand(true)
            .build();
        let capsule_b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        capsule_b.add_css_class("prompt-entry");
        capsule_b.append(&entry_b);
        column.append(&capsule_b);

        // Variant C: scroller with min_content_height(-1).
        let entry_c = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        let scroller_c = gtk::ScrolledWindow::builder()
            .child(&entry_c)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .hexpand(true)
            .build();
        let capsule_c = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        capsule_c.add_css_class("prompt-entry");
        capsule_c.append(&scroller_c);
        column.append(&capsule_c);

        // Variant D: chat.rs EXACTLY as of today.
        let entry_d = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(7)
            .bottom_margin(7)
            .left_margin(8)
            .right_margin(8)
            .build();
        let scroller_d = gtk::ScrolledWindow::builder()
            .child(&entry_d)
            .vscrollbar_policy(gtk::PolicyType::External)
            .min_content_height(0)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let field_d = gtk::Box::new(gtk::Orientation::Vertical, 0);
        field_d.add_css_class("prompt-entry");
        field_d.append(&scroller_d);
        column.append(&field_d);

        // Wrapped-text matrix: which policy measures BOTH states right?
        let wrapped =
            "dffffffffffffffffffffffffffffffffffffffffffffffff ffffffffffffffffffffffffffffffffff";
        let mut matrix: Vec<(String, gtk::Box)> = Vec::new();
        for (name, policy, with_text) in [
            ("external+text", gtk::PolicyType::External, true),
            ("automatic+text", gtk::PolicyType::Automatic, true),
            ("always+empty", gtk::PolicyType::Always, false),
            ("always+text", gtk::PolicyType::Always, true),
        ] {
            let tv = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::WordChar)
                .top_margin(7)
                .bottom_margin(7)
                .left_margin(8)
                .right_margin(8)
                .build();
            if with_text {
                tv.buffer().set_text(wrapped);
            }
            let sc = gtk::ScrolledWindow::builder()
                .child(&tv)
                .vscrollbar_policy(policy)
                .min_content_height(0)
                .max_content_height(120)
                .propagate_natural_height(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .hexpand(true)
                .build();
            let boxx = gtk::Box::new(gtk::Orientation::Vertical, 0);
            boxx.add_css_class("prompt-entry");
            boxx.append(&sc);
            column.append(&boxx);
            matrix.push((name.to_string(), boxx));
        }

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(380)
            .default_height(300)
            .content(&column)
            .build();
        window.present();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            println!("search:  h={}", search.height());
            println!("capsule: h={}", capsule.height());
            println!(
                "  field: h={}  scroller: h={}  entry: h={}",
                field.height(),
                scroller.height(),
                entry.height()
            );
            println!("  attach: h={}  send: h={}", attach.height(), send.height());
            let describe = |ctx: gtk::pango::Context| {
                ctx.font_description()
                    .map(|d| {
                        format!(
                            "{} @ {}pt",
                            d.family().unwrap_or_default(),
                            d.size() as f64 / gtk::pango::SCALE as f64
                        )
                    })
                    .unwrap_or_default()
            };
            println!("search font: {}", describe(search.pango_context()));
            println!("textview font: {}", describe(entry.pango_context()));
            println!("bare-textview capsule: h={}", capsule_b.height());
            println!("external-scrollbar capsule: h={}", capsule_c.height());
            println!(
                "chat-exact field: h={} scroller: h={} entry: h={}",
                field_d.height(),
                scroller_d.height(),
                entry_d.height()
            );
            for (name, boxx) in &matrix {
                println!("matrix {name}: h={}", boxx.height());
            }
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[]);
}
