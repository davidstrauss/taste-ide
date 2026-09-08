//! A crowded strip's way to any page: a menu of them, at the strip's end.
//!
//! This is what GNOME Builder does. Its frames (libpanel's
//! `PanelFrameTabBar`) put a down-arrow menu button at the end of the tab
//! bar, and the menu lists the frame's pages; a tab that scrolled out of
//! view is one click away and nothing on screen changes place to get there.
//! The strips here used `AdwTabOverview` instead — a grid of thumbnails
//! with a search box, opened from a count badge at the strip's start — and
//! two things were wrong with it in a pane: its header carried the
//! window's close control (fixed once, by bending the widget), and its way
//! back sat in a different corner from its way in (David: "I don't like
//! the widget for opening the tab overview moving to the right hand side
//! after opening it"). A popover opens over the strip and closes back into
//! the same button. Nothing moves.
//!
//! The list is the view's own `pages()` selection model, so it is always
//! current and the selected page is the selected row without any
//! bookkeeping here; titles and icons are read when the popover opens,
//! which is when they are looked at.

use crate::hover::FullTextOnHover;
use adw::prelude::*;
use gtk::glib;

/// The menu button for `view`'s pages, ready to be placed in a tab bar's
/// end action slot.
pub fn pages_menu(view: &adw::TabView) -> gtk::MenuButton {
    let popover = gtk::Popover::builder().build();
    let button = gtk::MenuButton::builder()
        .icon_name("pan-down-symbolic")
        .tooltip_text("Open pages")
        .css_classes(["flat"])
        .popover(&popover)
        .valign(gtk::Align::Center)
        .build();
    // Built on every open: the pages a strip has, and what they are
    // called, change between one look and the next, and a popover is
    // looked at for a second at a time.
    let view = view.clone();
    popover.connect_show(move |popover| {
        popover.set_child(Some(&build_list(&view, popover)));
    });
    button
}

fn build_list(view: &adw::TabView, popover: &gtk::Popover) -> gtk::Widget {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::new();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build()
            .full_text_on_hover();
        row.append(&icon);
        row.append(&title);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(page) = item.item().and_downcast::<adw::TabPage>() else {
            return;
        };
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let icon = row.first_child().and_downcast::<gtk::Image>().unwrap();
        let title = row.last_child().and_downcast::<gtk::Label>().unwrap();
        match page.icon() {
            Some(gicon) => icon.set_from_gicon(&gicon),
            None => icon.set_icon_name(None),
        }
        icon.set_visible(page.icon().is_some());
        let query = crate::search::current_query();
        if query.is_empty() {
            title.set_label(&page.title());
        } else {
            title.set_markup(&query.highlight_markup(&page.title()));
            if !query.matches(&page.title()) {
                row.add_css_class("search-dim");
            } else {
                row.remove_css_class("search-dim");
            }
        }
        // A page asking for attention says so here too, where the strip's
        // own mark is not on screen for a tab that scrolled off.
        if page.needs_attention() {
            row.add_css_class("accent");
        } else {
            row.remove_css_class("accent");
        }
    });

    // Under a query the menu lists the matching pages — all of them, with
    // the matches marked, when the ghost is on. Tabs cannot hide in the
    // strip (docs/SEARCH.md → Known limits), so this is where a tab set
    // filters.
    let query = crate::search::current_query();
    let filtered: gtk::gio::ListModel = if query.is_empty() || query.ghost {
        view.pages().upcast()
    } else {
        let filter = gtk::CustomFilter::new(move |item| {
            item.downcast_ref::<adw::TabPage>()
                .is_some_and(|page| query.matches(&page.title()))
        });
        gtk::FilterListModel::new(Some(view.pages()), Some(filter)).upcast()
    };
    let list = gtk::ListView::builder()
        .model(&gtk::NoSelection::new(Some(filtered)))
        .factory(&factory)
        .single_click_activate(true)
        .css_classes(["navigation-sidebar"])
        .build();
    let view = view.clone();
    let popover = popover.clone();
    list.connect_activate(move |list, position| {
        if let Some(page) = list
            .model()
            .and_then(|model| model.item(position))
            .and_downcast::<adw::TabPage>()
        {
            view.set_selected_page(&page);
        }
        popover.popdown();
    });

    let heading = gtk::Label::builder()
        .label("Open pages")
        .css_classes(["caption-heading", "dim-label"])
        .xalign(0.0)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .width_request(280)
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.append(&heading);
    column.append(&scroller);
    // The list's own focus, not the heading's: Down then Enter picks the
    // second page without a pointer.
    glib::idle_add_local_once(move || {
        list.grab_focus();
    });
    column.upcast()
}
