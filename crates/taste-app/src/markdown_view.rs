//! Full-quality markdown rendering: pulldown-cmark events → native GTK
//! widgets (real heading sizes, bulleted lists, code cards, pictures).
//! Inline code copies on click; code blocks carry a copy button.
//! Read-only by design — editing happens in the source view.
//!
//! **Images** (David, 2026-09-21: "Markdown previews should support
//! images") come in two kinds, told apart by their source. A relative
//! path is a file beside the document, read through the same files
//! service the document came through — which is the VM's keeper when the
//! checkout is in a VM — decoded off the main thread, and shown as a
//! picture scaled down to the column. An `http(s)` source is NOT fetched:
//! the host contacting a URL a project chose is a host-side request on
//! project-controlled input, which is the line ENVIRONMENTS.md draws, so
//! it is drawn as a link the reader can open on purpose. A document with
//! no base to resolve against (a chat message) keeps the alt text.

use adw::prelude::*;
use gtk::glib;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::cell::RefCell;
use std::path::{Component, PathBuf};
use std::rc::Rc;

/// Where a document stands: its directory, read through the files
/// service the document itself came through, and the checkout that
/// bounds it. Relative image paths resolve against it, and so do links
/// to files — `[the tree](crates/taste-app/src/filetree.rs)`, or
/// `../README.md#L20` — which open in the editor through `open_file`
/// instead of going nowhere (David, 2026-09-22: "links that map to file
/// paths (relative to project root or current file directly) should open
/// the files rather than copy the string"). A path that climbs out of the
/// checkout is neither an image nor a file to open, whatever it names.
#[derive(Clone)]
pub struct DocumentBase {
    pub files: taste_core::files::Files,
    pub dir: PathBuf,
    pub root: PathBuf,
    /// Open a file of this checkout in the editor, at a line when given.
    pub open_file: Rc<dyn Fn(PathBuf, Option<u32>)>,
}

impl DocumentBase {
    /// A link's target as a file of this checkout and a line, when the
    /// link is a path and not a URL: `path`, `path#L12`, `path#12`, or
    /// `path:12`. Resolved like an image source, against the document's
    /// directory or the root for a leading slash.
    fn resolve_link(&self, href: &str) -> Option<(PathBuf, Option<u32>)> {
        if href.contains("://") || href.starts_with("mailto:") || href.starts_with("copy:") {
            return None;
        }
        if href.starts_with(crate::issue_pill::SCHEME) || href.starts_with('#') {
            return None;
        }
        let (path_part, line) = match href.split_once('#') {
            Some((path, fragment)) => (path, fragment.trim_start_matches('L').parse::<u32>().ok()),
            None => match href.rsplit_once(':') {
                Some((path, digits))
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) =>
                {
                    (path, digits.parse::<u32>().ok())
                }
                _ => (href, None),
            },
        };
        if path_part.is_empty() {
            return None;
        }
        self.resolve(path_part).map(|path| (path, line))
    }

    /// The file a markdown image source names, or `None` when it is not a
    /// file of this checkout: a URL, or a path that leaves the root.
    fn resolve(&self, source: &str) -> Option<PathBuf> {
        if source.contains("://") || source.starts_with("data:") {
            return None;
        }
        let source = percent_decode(source.split(['?', '#']).next().unwrap_or(source));
        let joined = if source.starts_with('/') {
            self.root.join(source.trim_start_matches('/'))
        } else {
            self.dir.join(&source)
        };
        // Lexically, since the path may be in a VM and cannot be
        // canonicalized here; `..` is folded and a climb past the root is
        // refused.
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                other => normalized.push(other),
            }
        }
        normalized.starts_with(&self.root).then_some(normalized)
    }
}

/// `%20` and friends, as a markdown source spells a path with spaces.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && text.is_char_boundary(i + 3) {
            if let Ok(value) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Render `text` to a widget tree. `on_link` receives activated http(s)
/// links (the caller decides how to open them).
pub fn render(text: &str, on_link: Rc<dyn Fn(&str)>) -> gtk::Widget {
    render_full(text, on_link, None, None)
}

/// [`render`] for a document with a place: its relative images are read
/// from beside it and shown (the editor's preview).
pub fn render_document(text: &str, on_link: Rc<dyn Fn(&str)>, images: DocumentBase) -> gtk::Widget {
    render_full(text, on_link, None, Some(images))
}

/// [`render`] with issue references drawn as pills when an index is
/// given (`crate::issue_pill`) — the chat's transcript passes its window's
/// index, and a pill's click reaches `on_link` as `taste-issue:<id>` —
/// and with a place when the document has one, so its paths open and its
/// images show.
pub fn render_in(
    text: &str,
    on_link: Rc<dyn Fn(&str)>,
    issues: Option<crate::issue_pill::SharedIssueIndex>,
    base: Option<DocumentBase>,
) -> gtk::Widget {
    render_full(text, on_link, issues, base)
}

fn render_full(
    text: &str,
    on_link: Rc<dyn Fn(&str)>,
    issues: Option<crate::issue_pill::SharedIssueIndex>,
    images: Option<DocumentBase>,
) -> gtk::Widget {
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
    // Tables: the column alignments and the rows, the first row the head,
    // rendered as a grid of labels (`table_card`).
    let mut table: Option<(Vec<pulldown_cmark::Alignment>, Vec<Vec<String>>)> = None;
    // Inside a markdown link the text is the link's, and a pill there
    // would nest one <a> in another.
    let mut in_link = false;
    // Inside an image: its source and the alt text accumulating, until
    // the end decides what the image becomes.
    let mut image: Option<(String, String)> = None;

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
            .max_width_chars(40)
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
        if let Some(index) = issues.as_ref() {
            crate::issue_pill::install_tooltips(&label, index.clone());
        }
        let span_texts = std::mem::take(spans);
        let on_link = on_link.clone();
        let base = images.clone();
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
            if href.starts_with("http://")
                || href.starts_with("https://")
                || href.starts_with(crate::issue_pill::SCHEME)
            {
                on_link(href);
                return glib::Propagation::Stop;
            }
            // A path of this checkout: the file, in the editor.
            if let Some(base) = &base {
                if let Some((path, line)) = base.resolve_link(href) {
                    (base.open_file)(path, line);
                }
            }
            glib::Propagation::Stop // anything else goes nowhere
        });
        // In its frame, which paints the pills' capsules under the text
        // (`issue_pill::PillText`); the label itself is still the thing
        // selected, clicked, and asked for tooltips above.
        root.append(&crate::issue_pill::PillText::wrap(&label));
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
                    in_link = true;
                    markup.push_str(&format!(
                        "<a href=\"{}\">",
                        glib::markup_escape_text(&dest_url)
                    ));
                }
                Tag::Image { dest_url, .. } => image = Some((dest_url.to_string(), String::new())),
                Tag::Table(alignments) => {
                    flush(
                        &mut markup,
                        &mut spans,
                        &mut heading,
                        quote_depth,
                        &root,
                        &on_link,
                    );
                    table = Some((alignments, Vec::new()));
                }
                Tag::TableRow | Tag::TableHead => {
                    if let Some((_, rows)) = table.as_mut() {
                        rows.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some(row) = table.as_mut().and_then(|(_, r)| r.last_mut()) {
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
                        root.append(&code_card(code.trim_end_matches('\n'), true));
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
                TagEnd::Link => {
                    in_link = false;
                    markup.push_str("</a>");
                }
                TagEnd::Image => {
                    if let Some((source, alt)) = image.take() {
                        let file = images.as_ref().and_then(|base| base.resolve(&source));
                        match (file, images.as_ref()) {
                            (Some(path), Some(base)) => {
                                flush(
                                    &mut markup,
                                    &mut spans,
                                    &mut heading,
                                    quote_depth,
                                    &root,
                                    &on_link,
                                );
                                root.append(&picture(base, path, &alt));
                            }
                            _ if source.starts_with("http://")
                                || source.starts_with("https://") =>
                            {
                                // A link the reader opens on purpose, never
                                // a fetch the document makes for them.
                                markup.push_str(&format!(
                                    "<i>[image: <a href=\"{}\">{}</a>]</i>",
                                    glib::markup_escape_text(&source),
                                    glib::markup_escape_text(if alt.is_empty() {
                                        &source
                                    } else {
                                        &alt
                                    })
                                ));
                            }
                            _ => {
                                markup.push_str(&format!(
                                    "<i>[image: {}]</i>",
                                    glib::markup_escape_text(&alt)
                                ));
                            }
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some((alignments, rows)) = table.take() {
                        root.append(&table_card(&alignments, &rows));
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some((_, alt)) = image.as_mut() {
                    alt.push_str(&text);
                } else if let Some(code) = code_block.as_mut() {
                    code.push_str(&text);
                } else if let Some(cell) = table
                    .as_mut()
                    .and_then(|(_, r)| r.last_mut())
                    .and_then(|r| r.last_mut())
                {
                    cell.push_str(&text);
                } else if let Some(index) = issues.as_ref().filter(|_| !in_link) {
                    markup.push_str(&crate::issue_pill::pillify(&text, index));
                } else {
                    markup.push_str(&glib::markup_escape_text(&text));
                }
            }
            Event::Code(code) => {
                // A table cell's code is the cell's, as its text is: a
                // code span that went to the markup instead leaked every
                // `id` and `path` of a backlog table out of the grid and
                // into the paragraph after it, as one run of copy links,
                // while the cells lost them.
                if let Some(cell) = table
                    .as_mut()
                    .and_then(|(_, r)| r.last_mut())
                    .and_then(|r| r.last_mut())
                {
                    cell.push_str(&code);
                    continue;
                }
                // An issue id in backticks is the issue, not six characters
                // to copy: agents write ids as code as often as as prose,
                // and a reader should not get a copy link for one and a
                // pill for the other (David, 2026-09-16: "These issues
                // aren't pills").
                if let Some(index) = issues.as_ref().filter(|_| !in_link) {
                    let bare = code.trim();
                    let whole = matches!(
                        crate::issue_pill::find_refs(bare).as_slice(),
                        [range] if *range == (0..bare.len())
                    );
                    if whole {
                        markup.push_str(&crate::issue_pill::pillify(bare, index));
                        continue;
                    }
                }
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
///
/// `reflow` is whether long lines may FOLD. A fenced code block may: at the
/// chat column's 320px minimum a line of code is wider than the pane, and
/// the alternative was a line cut off mid-glyph with a hairline overlay
/// scrollbar as the only sign there was more of it — a clipping bug's
/// silhouette, and it hid content in the one pane held to the highest bar.
/// A table may NOT: its columns ARE its meaning, and folding them makes
/// rubble of the grid, so a table keeps its horizontal scroller and gets an
/// honest one instead (see the caller).
/// A document's image, as a picture that fills in: the file is read
/// through the files service and decoded off the main thread, and the
/// widget takes the texture when it lands. Scaled down to the column,
/// never up; the alt text is the tooltip, and the whole caption when the
/// file cannot be read or is not an image.
fn picture(base: &DocumentBase, path: PathBuf, alt: &str) -> gtk::Widget {
    // The picture fills the column and scales down to it, never up: a
    // 1440px screenshot in a 600px column is the column wide and keeps
    // its aspect. Its minimum height is zero (it can shrink), so the
    // preview's viewport has to allocate natural heights, and does
    // (editor.rs, `preview_viewport`).
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 4);
    holder.set_hexpand(true);
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();
    if !alt.is_empty() {
        picture.set_tooltip_text(Some(alt));
        picture.update_property(&[gtk::accessible::Property::Label(alt)]);
    }
    holder.append(&picture);
    let files = base.files.clone();
    let alt = alt.to_string();
    let weak = holder.downgrade();
    let picture_weak = picture.downgrade();
    glib::spawn_future_local(async move {
        let read_path = path.clone();
        let loaded = crate::runtime::runtime()
            .spawn_blocking(move || -> Result<gtk::gdk::Texture, String> {
                let bytes = files
                    .read(&read_path)
                    .map_err(|e| format!("could not be read: {e}"))?;
                gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                    .map_err(|e| format!("is not an image this build can decode: {e}"))
            })
            .await;
        let (Some(holder), Some(picture)) = (weak.upgrade(), picture_weak.upgrade()) else {
            return;
        };
        let loaded = match loaded {
            Ok(inner) => inner,
            Err(e) => Err(format!("could not be decoded: {e}")),
        };
        match loaded {
            Ok(texture) => {
                tracing::debug!(
                    "markdown image {} loaded: {}x{}",
                    path.display(),
                    texture.width(),
                    texture.height()
                );
                picture.set_paintable(Some(&texture));
            }
            Err(why) => {
                tracing::info!("markdown image {} {why}", path.display());
                holder.remove(&picture);
                holder.append(
                    &gtk::Label::builder()
                        .label(format!(
                            "[image: {}] — {} {why}",
                            if alt.is_empty() { "untitled" } else { &alt },
                            path.display()
                        ))
                        .css_classes(["dim-label"])
                        .wrap(true)
                        .xalign(0.0)
                        .build(),
                );
            }
        }
    });
    holder.upcast()
}

fn code_card(code: &str, reflow: bool) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(code)
        .xalign(0.0)
        .selectable(true)
        .wrap(reflow)
        // Code has runs with no space to break at — a path, a long
        // identifier — so word wrapping alone would overflow the pane
        // rather than fold.
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["monospace"])
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&label)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(if reflow {
            // Folded: there is nothing to the right, so a bar there could
            // only lie about it.
            gtk::PolicyType::Never
        } else {
            gtk::PolicyType::Automatic
        })
        .build();
    // And when there IS something to the right, the bar that says so takes
    // room of its own. An overlay scrollbar is a hairline that fades out
    // over a surface nobody has hovered — the right treatment for a long
    // document, and the wrong one for the single fact that a table
    // continues past the edge of a narrow pane.
    scroller.set_overlay_scrolling(reflow);
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
/// A table as a table: a grid of labels, the head row bold over a rule,
/// each column aligned as the markdown said, cells wrapping in place of
/// scrolling (David, 2026-09-22: "use a real table" — it was a monospace
/// grid in a code card). Plain text in the cells, as the parser hands
/// them over.
fn table_card(alignments: &[pulldown_cmark::Alignment], rows: &[Vec<String>]) -> gtk::Widget {
    let grid = gtk::Grid::builder()
        .css_classes(["markdown-table"])
        .column_spacing(0)
        .row_spacing(0)
        .build();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0) as i32;
    let mut grid_row = 0;
    for (index, row) in rows.iter().enumerate() {
        for column in 0..columns {
            let text = row.get(column as usize).map(String::as_str).unwrap_or("");
            let xalign = match alignments.get(column as usize) {
                Some(pulldown_cmark::Alignment::Center) => 0.5,
                Some(pulldown_cmark::Alignment::Right) => 1.0,
                _ => 0.0,
            };
            let label = gtk::Label::builder()
                .label(text)
                .xalign(xalign)
                .yalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(32)
                .selectable(true)
                .css_classes(if index == 0 {
                    vec!["markdown-table-cell", "markdown-table-head"]
                } else if index % 2 == 0 {
                    vec!["markdown-table-cell", "markdown-table-alt"]
                } else {
                    vec!["markdown-table-cell"]
                })
                .build();
            grid.attach(&label, column, grid_row, 1, 1);
        }
        grid_row += 1;
        if index == 0 {
            let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
            grid.attach(&rule, 0, grid_row, columns.max(1), 1);
            grid_row += 1;
        }
    }
    grid.upcast()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DocumentBase {
        DocumentBase {
            files: taste_core::files::Files::Local,
            dir: PathBuf::from("/checkout/docs"),
            root: PathBuf::from("/checkout"),
            open_file: Rc::new(|_, _| {}),
        }
    }

    #[test]
    fn a_path_link_names_a_file_and_maybe_a_line_and_a_url_does_not() {
        assert_eq!(
            base().resolve_link("../crates/taste-app/src/filetree.rs#L4136"),
            Some((
                PathBuf::from("/checkout/crates/taste-app/src/filetree.rs"),
                Some(4136)
            ))
        );
        assert_eq!(
            base().resolve_link("ENVIRONMENTS.md:20"),
            Some((PathBuf::from("/checkout/docs/ENVIRONMENTS.md"), Some(20)))
        );
        assert_eq!(
            base().resolve_link("/README.md"),
            Some((PathBuf::from("/checkout/README.md"), None))
        );
        assert_eq!(base().resolve_link("https://example.org/a.md"), None);
        assert_eq!(base().resolve_link("#heading"), None);
        assert_eq!(base().resolve_link("copy:3"), None);
        assert_eq!(base().resolve_link("../../etc/passwd"), None);
    }

    /// The decode the preview relies on, against a real screenshot of the
    /// docs set: a PNG the size of the window comes back as a texture of
    /// that size, off any thread.
    #[test]
    fn a_docs_screenshot_decodes_to_a_texture() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots/hero.png");
        let bytes = std::fs::read(&path).expect("the docs set has a hero shot");
        let texture =
            gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).expect("a PNG decodes");
        assert_eq!((texture.width(), texture.height()), (1440, 900));
    }

    #[test]
    fn a_relative_source_resolves_beside_the_document_and_never_above_the_root() {
        assert_eq!(
            base().resolve("screenshots/hero.png"),
            Some(PathBuf::from("/checkout/docs/screenshots/hero.png"))
        );
        assert_eq!(
            base().resolve("../README%20art.png?raw=1"),
            Some(PathBuf::from("/checkout/README art.png"))
        );
        assert_eq!(
            base().resolve("/assets/logo.svg"),
            Some(PathBuf::from("/checkout/assets/logo.svg"))
        );
        assert_eq!(base().resolve("../../etc/passwd"), None);
        assert_eq!(base().resolve("https://example.org/a.png"), None);
        assert_eq!(base().resolve("data:image/png;base64,AAAA"), None);
    }
}
