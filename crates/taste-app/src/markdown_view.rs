//! Full-quality markdown rendering: pulldown-cmark events → native GTK
//! widgets (real heading sizes, bulleted lists, code cards). Inline code
//! copies on click; code blocks carry a copy button. Read-only by design —
//! editing happens in the source view.

use adw::prelude::*;
use gtk::glib;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cell::RefCell;
use std::rc::Rc;

/// Render `text` to a widget tree. `on_link` receives activated http(s)
/// links (the caller decides how to open them).
pub fn render(text: &str, on_link: Rc<dyn Fn(&str)>) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(text, options);

    // Inline accumulation: Pango markup plus the code-span texts backing
    // the clickable copy links.
    let mut markup = String::new();
    let mut spans: Vec<String> = Vec::new();
    let mut heading: Option<HeadingLevel> = None;
    let mut code_block: Option<String> = None;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth: usize = 0;
    // Tables render as a monospace grid (plain but faithful).
    let mut table: Option<Vec<Vec<String>>> = None;

    let flush = |markup: &mut String,
                 spans: &mut Vec<String>,
                 heading: &mut Option<HeadingLevel>,
                 quote_depth: usize,
                 root: &gtk::Box,
                 on_link: &Rc<dyn Fn(&str)>| {
        if markup.trim().is_empty() {
            markup.clear();
            spans.clear();
            return;
        }
        let label = gtk::Label::builder()
            .use_markup(true)
            .label(markup.as_str())
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .selectable(true)
            .build();
        match heading.take() {
            Some(HeadingLevel::H1) => label.add_css_class("title-1"),
            Some(HeadingLevel::H2) => label.add_css_class("title-2"),
            Some(HeadingLevel::H3) => label.add_css_class("title-3"),
            Some(_) => label.add_css_class("title-4"),
            None => {}
        }
        if quote_depth > 0 {
            label.set_margin_start(14 * quote_depth as i32);
            label.add_css_class("dim-label");
        }
        let span_texts = std::mem::take(spans);
        let on_link = on_link.clone();
        label.connect_activate_link(move |label, href| {
            if let Some(index) = href.strip_prefix("copy:") {
                if let Some(text) = index.parse::<usize>().ok().and_then(|i| span_texts.get(i)) {
                    label.clipboard().set_text(text);
                    if let Some(root) = label.root() {
                        if let Some(overlay) = find_toast_overlay(root.upcast_ref()) {
                            overlay.add_toast(adw::Toast::new("Copied"));
                        }
                    }
                }
                return glib::Propagation::Stop;
            }
            if href.starts_with("http://") || href.starts_with("https://") {
                on_link(href);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Stop // unknown schemes go nowhere
        });
        root.append(&label);
        markup.clear();
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => heading = Some(level),
                Tag::CodeBlock(kind) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    let _lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_block = Some(String::new());
                }
                Tag::List(start) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    list_stack.push(start);
                }
                Tag::Item => {
                    let depth = list_stack.len().saturating_sub(1);
                    markup.push_str(&"    ".repeat(depth));
                    match list_stack.last_mut() {
                        Some(Some(n)) => {
                            markup.push_str(&format!("{n}. "));
                            *n += 1;
                        }
                        _ => markup.push_str("• "),
                    }
                }
                Tag::BlockQuote(_) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    quote_depth += 1;
                }
                Tag::Emphasis => markup.push_str("<i>"),
                Tag::Strong => markup.push_str("<b>"),
                Tag::Strikethrough => markup.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    markup.push_str(&format!(
                        "<a href=\"{}\">",
                        glib::markup_escape_text(&dest_url)
                    ));
                }
                Tag::Image { .. } => markup.push_str("<i>[image: "),
                Tag::Table(_) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    table = Some(Vec::new());
                }
                Tag::TableRow | Tag::TableHead => {
                    if let Some(rows) = table.as_mut() {
                        rows.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some(row) = table.as_mut().and_then(|r| r.last_mut()) {
                        row.push(String::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::Item => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = code_block.take() {
                        root.append(&code_card(code.trim_end_matches('\n')));
                    }
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::BlockQuote(_) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Emphasis => markup.push_str("</i>"),
                TagEnd::Strong => markup.push_str("</b>"),
                TagEnd::Strikethrough => markup.push_str("</s>"),
                TagEnd::Link => markup.push_str("</a>"),
                TagEnd::Image => markup.push_str("]</i>"),
                TagEnd::Table => {
                    if let Some(rows) = table.take() {
                        root.append(&table_card(&rows));
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some(code) = code_block.as_mut() {
                    code.push_str(&text);
                } else if let Some(cell) = table
                    .as_mut()
                    .and_then(|r| r.last_mut())
                    .and_then(|r| r.last_mut())
                {
                    cell.push_str(&text);
                } else {
                    markup.push_str(&glib::markup_escape_text(&text));
                }
            }
            Event::Code(code) => {
                // Inline code: click to copy (rendered as a quiet link).
                let index = spans.len();
                spans.push(code.to_string());
                markup.push_str(&format!(
                    "<a href=\"copy:{index}\" title=\"Click to copy\"><tt>{}</tt></a>",
                    glib::markup_escape_text(&code)
                ));
            }
            Event::SoftBreak => markup.push(' '),
            Event::HardBreak => markup.push('\n'),
            Event::Rule => {
                flush(
                    &mut markup,
                    &mut spans,
                    &mut heading,
                    quote_depth,
                    &root,
                    &on_link,
                );
                root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            Event::TaskListMarker(done) => {
                markup.push_str(if done { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }
    flush(
        &mut markup,
        &mut spans,
        &mut heading,
        quote_depth,
        &root,
        &on_link,
    );
    root.upcast()
}

/// A code block: monospace card with a copy button that confirms itself.
fn code_card(code: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(code)
        .xalign(0.0)
        .selectable(true)
        .wrap(false)
        .css_classes(["monospace"])
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&label)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy code block")
        .css_classes(["flat", "circular"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(4)
        .margin_end(4)
        .build();
    let code = code.to_string();
    let reset: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&code);
        // The button confirms itself; no toast needed this close by.
        button.set_icon_name("object-select-symbolic");
        if let Some(previous) = reset.borrow_mut().take() {
            previous.remove();
        }
        let button = button.clone();
        let reset_slot = reset.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
            button.set_icon_name("edit-copy-symbolic");
            reset_slot.borrow_mut().take();
        });
        *reset.borrow_mut() = Some(id);
    });
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&scroller));
    overlay.add_overlay(&copy);
    let frame = gtk::Frame::builder().child(&overlay).build();
    frame.add_css_class("view");
    frame.upcast()
}

/// Tables: a faithful monospace grid (native GtkGrid styling can come
/// later; alignment correctness comes first).
fn table_card(rows: &[Vec<String>]) -> gtk::Widget {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let len = cell.chars().count();
            if index >= widths.len() {
                widths.push(len);
            } else if widths[index] < len {
                widths[index] = len;
            }
        }
    }
    let mut text = String::new();
    for (row_index, row) in rows.iter().enumerate() {
        for (index, cell) in row.iter().enumerate() {
            let pad = widths.get(index).copied().unwrap_or(0);
            text.push_str(&format!("{cell:<pad$}  "));
        }
        text.push('\n');
        if row_index == 0 {
            for width in &widths {
                text.push_str(&"─".repeat(*width));
                text.push_str("  ");
            }
            text.push('\n');
        }
    }
    code_card(text.trim_end())
}

fn find_toast_overlay(widget: &gtk::Widget) -> Option<adw::ToastOverlay> {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Ok(overlay) = current.clone().downcast::<adw::ToastOverlay>() {
            return Some(overlay);
        }
        child = current.first_child();
    }
    None
}
