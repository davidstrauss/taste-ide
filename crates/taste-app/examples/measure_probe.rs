//! Measures transcript-row allocations with the exact widget shapes the
//! chat pane uses. Prints heights and exits; diagnostic only.

use adw::prelude::*;
use gtk::glib;

fn main() {
    let app = adw::Application::builder()
        .application_id("net.davidstrauss.TasteMeasure")
        .build();
    app.connect_activate(|app| {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        // user card shape
        let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card.add_css_class("card");
        card.append(&gtk::Label::builder().label("Hello").xalign(0.0).build());
        list.append(&card);
        // agent message shape (chat.rs agent_buffer), wrapped in a
        // ListBoxRow exactly like append_row does.
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(6)
            .margin_end(24)
            .build();
        view.buffer().set_text("No response requested.");
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .child(&view)
            .build();
        list.append(&row);
        let card2 = gtk::Box::new(gtk::Orientation::Vertical, 4);
        card2.add_css_class("card");
        card2.append(&gtk::Label::builder().label("Hello 2").xalign(0.0).build());
        list.append(&card2);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build();
        // Replicate the options shade: the transcript page starts HIDDEN
        // (rows created while unallocated), then becomes visible.
        let shade = gtk::Label::new(Some("options shade"));
        shade.add_css_class("background");
        let stack = gtk::Overlay::new();
        stack.set_vexpand(true);
        stack.set_child(Some(&scroller));
        stack.add_overlay(&shade);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(380)
            .default_height(900)
            .content(&stack)
            .build();
        window.present();
        {
            let shade = shade.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                shade.set_visible(false);
            });
        }
        let list = list.clone();
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            let mut index = 0;
            let mut child = list.first_child();
            while let Some(row) = child {
                println!("row {index}: height={} width={}", row.height(), row.width());
                let (min, nat, _, _) = row.measure(gtk::Orientation::Vertical, 340);
                println!("  measured(min={min} nat={nat}) at width 340");
                child = row.next_sibling();
                index += 1;
            }
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[]);
}
