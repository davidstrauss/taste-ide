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

/// A run of prose this long ends before its next heading (`render`).
const RUN_AT_HEADING: usize = 1500;

/// A run of prose this long ends after its next paragraph or list.
const RUN_MOST: usize = 4000;

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
    /// Open a file of this checkout in the editor, at a place when given:
    /// a line, a PDF's page (`taste_core::anchor`).
    pub open_file: Rc<dyn Fn(PathBuf, Option<taste_core::anchor::Anchor>)>,
}

impl DocumentBase {
    /// A link's target as a file of this checkout and a place in it, when
    /// the link is a path and not a URL: `path`, `path#L12`, `path#12`,
    /// `path:12`, or a PDF's `path#page=3` (`taste_core::anchor`).
    /// Resolved like an image source, against the document's directory or
    /// the root for a leading slash.
    fn resolve_link(&self, href: &str) -> Option<(PathBuf, Option<taste_core::anchor::Anchor>)> {
        if href.contains("://") || href.starts_with("mailto:") {
            return None;
        }
        if href.starts_with(crate::issue_pill::SCHEME) || href.starts_with('#') {
            return None;
        }
        let (path_part, at) = taste_core::anchor::split_reference(href);
        if path_part.is_empty() {
            return None;
        }
        self.resolve(path_part).map(|path| (path, at))
    }

    /// The file a markdown image source names, or `None` when it is not a
    /// file of this checkout: a URL, or a path that leaves the root.
    fn resolve(&self, source: &str) -> Option<PathBuf> {
        if source.contains("://") || source.starts_with("data:") {
            return None;
        }
        let source = percent_decode(source.split(['?', '#']).next().unwrap_or(source));
        // A leading slash is the checkout's root, as a README spells it —
        // unless what follows already names a path in the checkout, which
        // is how an agent that has just written a file names it: its
        // absolute path where it works. Joined onto the root again, that
        // came out as the root twice over and an image that "could not be
        // read" (2026-10-19).
        let joined = if PathBuf::from(&source).starts_with(&self.root) {
            PathBuf::from(&source)
        } else if source.starts_with('/') {
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
    render_full(text, on_link, None, None, None)
}

/// What a rendering still has on its way: its pictures read and decoded,
/// its diagrams drawn. A caller that keeps the previous rendering on
/// screen until the next is whole — the editor's preview — asks to be
/// told when this is nothing ([`Readiness::when_ready`]); each piece of
/// work holds a [`Hold`] and lets go when it lands, or fails.
#[derive(Clone, Default)]
pub struct Readiness(Rc<ReadinessState>);

#[derive(Default)]
struct ReadinessState {
    pending: std::cell::Cell<usize>,
    on_ready: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Readiness {
    /// One more piece of work on its way.
    pub fn hold(&self) -> Hold {
        self.0.pending.set(self.0.pending.get() + 1);
        Hold(self.clone())
    }

    /// Run `ready` once nothing is on its way: now, if nothing is.
    pub fn when_ready(&self, ready: impl FnOnce() + 'static) {
        if self.0.pending.get() == 0 {
            ready();
        } else {
            *self.0.on_ready.borrow_mut() = Some(Box::new(ready));
        }
    }
}

/// A piece of a rendering's work, outstanding until dropped.
pub struct Hold(Readiness);

impl Drop for Hold {
    fn drop(&mut self) {
        let state = &(self.0).0;
        let left = state.pending.get().saturating_sub(1);
        state.pending.set(left);
        if left == 0 {
            if let Some(ready) = state.on_ready.borrow_mut().take() {
                // Not inside whatever is dropping the hold: a widget's
                // callback, mid-update.
                glib::idle_add_local_once(ready);
            }
        }
    }
}

/// [`render`] for a document with a place: its relative images are read
/// from beside it and shown (the editor's preview).
///
/// Returned with what its pictures and diagrams have still to do: the
/// editor's preview builds the next rendering beside the one on screen
/// and swaps them once this says the next is whole.
pub fn render_document_buffered(
    text: &str,
    on_link: Rc<dyn Fn(&str)>,
    images: DocumentBase,
) -> (gtk::Widget, Readiness) {
    let readiness = Readiness::default();
    let widget = render_full(text, on_link, None, Some(images), Some(&readiness));
    (widget, readiness)
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
    render_full(text, on_link, issues, base, None)
}

/// `text` laid out as the blocks it is drawn as ([`Item`]), with no
/// widget made: what a rendering builds ([`render_full`]), and what an
/// incremental one compares with what it has built ([`Incremental`]).
fn layout(
    text: &str,
    issues: Option<&crate::issue_pill::SharedIssueIndex>,
    images: Option<&DocumentBase>,
) -> Vec<Item> {
    let items: RefCell<Vec<Item>> = RefCell::new(Vec::new());

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
    // The fence's info string: `mermaid` makes the block a diagram.
    let mut code_info = String::new();
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

    // Prose is set as RUNS: consecutive headings, paragraphs, and list
    // items share one label, so a selection can cross from one to the
    // next. GTK cannot carry a selection from one label into another, and
    // a label per paragraph made "select these three bullets" impossible
    // (David, 2026-10-09: "I often can't select text that spans multiple
    // paragraphs (or even bullets) in the chat or Markdown preview"). A
    // block that is a widget of its own — code, a table, a picture, a
    // rule — or a quote's indent ends the run.
    //
    // A block joins the run when it ends (`end_block`); the run becomes a
    // label when something that is not prose interrupts it, or the
    // document ends (`flush`, which ends the open block first).
    //
    // And when it has grown long: before a heading once it holds
    // `RUN_AT_HEADING` bytes of markup, and after a paragraph, a list, or
    // a list's item, none deeper than a top-level item, once it holds
    // `RUN_MOST`. A label draws
    // itself whole for every change of its selection, so a document of one
    // run — ARCHITECTURE.md's prose was a single label of 37,556
    // characters — took 370ms a frame to select in, and the whole window
    // waited on it (David, 2026-10-13: "selecting text in a MD preview
    // seems to be really slow and bog down the entire UI"). A section is
    // where a selection rarely crosses; a short document, a chat answer
    // with headings among them, stays one run.
    let run: RefCell<(String, Option<Block>)> = RefCell::new((String::new(), None));
    let end_block = |markup: &mut String, heading: &mut Option<HeadingLevel>, block: Block| {
        let level = heading.take();
        if markup.trim().is_empty() {
            markup.clear();
            return;
        }
        let block = if level.is_some() {
            Block::Heading
        } else {
            block
        };
        let mut run = run.borrow_mut();
        let (text, last) = &mut *run;
        if let Some(last) = *last {
            text.push('\n');
            text.push_str(gap(last, block));
        }
        match level {
            Some(level) => {
                let (size, weight) = heading_type(level);
                text.push_str(&format!(
                    "<span font_size=\"{size}\" weight=\"{weight}\">{markup}</span>"
                ));
            }
            None => text.push_str(markup),
        }
        markup.clear();
        *last = Some(block);
    };
    let flush = |markup: &mut String,
                 spans: &mut Vec<String>,
                 heading: &mut Option<HeadingLevel>,
                 quote_depth: usize| {
        end_block(markup, heading, Block::Paragraph);
        let (markup, _) = std::mem::take(&mut *run.borrow_mut());
        if markup.trim().is_empty() {
            spans.clear();
            return;
        }
        items.borrow_mut().push(Item::Run {
            markup,
            spans: std::mem::take(spans),
            quote_depth,
        });
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    if run.borrow().0.len() >= RUN_AT_HEADING {
                        flush(&mut markup, &mut spans, &mut heading, quote_depth);
                    }
                    heading = Some(level);
                }
                Tag::CodeBlock(kind) => {
                    flush(&mut markup, &mut spans, &mut heading, quote_depth);
                    code_info = match kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_block = Some(String::new());
                }
                Tag::List(start) => {
                    // The text before a list — a paragraph, or the item a
                    // nested list belongs to — ends here; the list goes on
                    // in the same run.
                    end_block(&mut markup, &mut heading, Block::Item);
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
                    flush(&mut markup, &mut spans, &mut heading, quote_depth);
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
                    flush(&mut markup, &mut spans, &mut heading, quote_depth);
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
                TagEnd::Heading(_) | TagEnd::Paragraph => {
                    let block = if list_stack.is_empty() {
                        Block::Paragraph
                    } else {
                        Block::Item
                    };
                    end_block(&mut markup, &mut heading, block);
                    // Inside a top-level item too: one bullet of many
                    // paragraphs was a label of 14,097 characters.
                    if list_stack.len() <= 1 && run.borrow().0.len() >= RUN_MOST {
                        flush(&mut markup, &mut spans, &mut heading, quote_depth);
                    }
                }
                TagEnd::Item => {
                    end_block(&mut markup, &mut heading, Block::Item);
                    // Between the items of a list that is not inside
                    // another, too: a long list is a run of its own, and
                    // its numbering is the list's, not the label's.
                    if list_stack.len() == 1 && run.borrow().0.len() >= RUN_MOST {
                        flush(&mut markup, &mut spans, &mut heading, quote_depth);
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = code_block.take() {
                        items.borrow_mut().push(Item::Code {
                            code: code.trim_end_matches('\n').to_string(),
                            info: code_info.clone(),
                            open: false,
                        });
                    }
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    if list_stack.is_empty() && run.borrow().0.len() >= RUN_MOST {
                        flush(&mut markup, &mut spans, &mut heading, quote_depth);
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush(&mut markup, &mut spans, &mut heading, quote_depth);
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
                        let file = images.and_then(|base| base.resolve(&source));
                        match file {
                            Some(path) => {
                                flush(&mut markup, &mut spans, &mut heading, quote_depth);
                                items.borrow_mut().push(Item::Picture { path, alt });
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
                        items.borrow_mut().push(Item::Table { alignments, rows });
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
                } else if let Some(index) = issues.filter(|_| !in_link) {
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
                if let Some(index) = issues.filter(|_| !in_link) {
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
                // A URL written as code is the URL: a link to it, which a
                // click opens and the label's menu copies as itself.
                let bare = code.trim();
                if (bare.starts_with("https://") || bare.starts_with("http://"))
                    && !bare.contains(char::is_whitespace)
                    && !in_link
                {
                    markup.push_str(&format!(
                        "<a href=\"{0}\" title=\"{0}\"><tt>{1}</tt></a>",
                        glib::markup_escape_text(bare),
                        glib::markup_escape_text(&code)
                    ));
                    continue;
                }
                // Inline code: click to copy (rendered as a quiet link).
                // The link's address is the code itself, so the label's own
                // "Copy Link Address" copies the code: an index there was
                // what it copied, `copy:0` (2026-10-22). A click finds the
                // address among the run's code spans ([`is_copy_span`]).
                spans.push(code.to_string());
                markup.push_str(&format!(
                    "<a href=\"{}\" title=\"Click to copy\"><tt>{}</tt></a>",
                    glib::markup_escape_text(&code),
                    glib::markup_escape_text(&code)
                ));
            }
            Event::SoftBreak => markup.push(' '),
            Event::HardBreak => markup.push('\n'),
            Event::Rule => {
                flush(&mut markup, &mut spans, &mut heading, quote_depth);
                items.borrow_mut().push(Item::Rule);
            }
            Event::TaskListMarker(done) => {
                markup.push_str(if done { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }
    flush(&mut markup, &mut spans, &mut heading, quote_depth);
    let mut items = items.into_inner();
    // A fence not yet closed — an answer still streaming — is code as it
    // stands, never a diagram drawn again at every line.
    let fences = text
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("```") || line.starts_with("~~~")
        })
        .count();
    if fences % 2 == 1 {
        if let Some(Item::Code { open, .. }) = items.last_mut() {
            *open = true;
        }
    }
    items
}

/// One block of a document as [`layout`] lays it out, before a widget is
/// made of it.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    /// A run of prose: one label's markup, its code spans' texts (the
    /// copy links), and how deep in quotes it stands.
    Run {
        markup: String,
        spans: Vec<String>,
        quote_depth: usize,
    },
    /// A block of code — a diagram when `info` names Mermaid and the
    /// fence has closed (`open` while it has not).
    Code {
        code: String,
        info: String,
        open: bool,
    },
    Table {
        alignments: Vec<pulldown_cmark::Alignment>,
        rows: Vec<Vec<String>>,
    },
    /// A picture of the document's own checkout.
    Picture {
        path: PathBuf,
        alt: String,
    },
    Rule,
}

/// What a document's links and pills answer to, for building its items.
#[derive(Clone)]
struct Context {
    on_link: Rc<dyn Fn(&str)>,
    issues: Option<crate::issue_pill::SharedIssueIndex>,
    images: Option<DocumentBase>,
}

/// An item's widget, and for a run its label and its code spans, which a
/// run grown in place is given anew.
struct Built {
    item: Item,
    widget: gtk::Widget,
    run: Option<(gtk::Label, Rc<RefCell<Vec<String>>>)>,
}

fn build(item: &Item, context: &Context, readiness: Option<&Readiness>) -> Built {
    let mut run = None;
    let widget: gtk::Widget = match item {
        Item::Run {
            markup,
            spans,
            quote_depth,
        } => {
            let label = gtk::Label::builder()
                .use_markup(true)
                .label(markup.as_str())
                .wrap(true)
                .max_width_chars(40)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            if *quote_depth > 0 {
                label.set_margin_start(14 * *quote_depth as i32);
                label.add_css_class("dim-label");
            }
            if let Some(index) = context.issues.as_ref() {
                crate::issue_pill::install_tooltips(&label, index.clone());
            }
            let span_texts = Rc::new(RefCell::new(spans.clone()));
            let on_link = context.on_link.clone();
            let base = context.images.clone();
            {
                let span_texts = span_texts.clone();
                label.connect_activate_link(move |label, href| {
                    if is_copy_span(&span_texts.borrow(), href) {
                        label.clipboard().set_text(href);
                        if let Some(root) = label.root() {
                            if let Some(overlay) = find_toast_overlay(root.upcast_ref()) {
                                overlay.add_toast(adw::Toast::new("Copied"));
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
                        if let Some((path, at)) = base.resolve_link(href) {
                            (base.open_file)(path, at);
                        }
                    }
                    glib::Propagation::Stop // anything else goes nowhere
                });
            }
            run = Some((label.clone(), span_texts));
            // In its frame, which paints the pills' capsules under the text
            // (`issue_pill::PillText`); the label itself is still the thing
            // selected, clicked, and asked for tooltips above.
            crate::issue_pill::PillText::wrap(&label).upcast()
        }
        Item::Code { code, info, open } => {
            let card = code_card(code, true);
            if !*open && crate::mermaid::is_mermaid(info) {
                // The block as code stays the fallback: a diagram that does
                // not parse is shown as written, with the reason under it.
                crate::mermaid::diagram(code, card, readiness.map(Readiness::hold))
            } else {
                card
            }
        }
        Item::Table { alignments, rows } => table_card(alignments, rows),
        Item::Picture { path, alt } => match context.images.as_ref() {
            Some(base) => picture(base, path.clone(), alt, readiness.map(Readiness::hold)),
            None => gtk::Label::new(Some(alt)).upcast(),
        },
        Item::Rule => gtk::Separator::new(gtk::Orientation::Horizontal).upcast(),
    };
    Built {
        item: item.clone(),
        widget,
        run,
    }
}

/// The box a document's blocks stand in.
fn document_box() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root
}

fn render_full(
    text: &str,
    on_link: Rc<dyn Fn(&str)>,
    issues: Option<crate::issue_pill::SharedIssueIndex>,
    images: Option<DocumentBase>,
    readiness: Option<&Readiness>,
) -> gtk::Widget {
    let root = document_box();
    let context = Context {
        on_link,
        issues,
        images,
    };
    for item in layout(text, context.issues.as_ref(), context.images.as_ref()) {
        root.append(&build(&item, &context, readiness).widget);
    }
    root.upcast()
}

/// A document drawn as it grows — an answer as it streams — without what
/// is drawn moving (David, 2026-10-21: "process MD in chat as it comes in,
/// but without jitter from reformatting/reflowing existing chunks"). Each
/// new text is laid out whole ([`layout`]) and compared, block by block,
/// with what is built: a block that has not changed keeps its widget; the
/// first that has, when it is a run of prose — the paragraph being
/// written, joined to the ones before it — is given its new markup in the
/// same label, whose lines above the change wrap as they did; what follows
/// is built. A block that has ended does not change, so only the bottom of
/// the document ever moves, and it is what [`render_in`] draws of the same
/// text, widget for widget.
pub struct Incremental {
    pub root: gtk::Box,
    context: Context,
    built: RefCell<Vec<Built>>,
}

impl Incremental {
    pub fn new(
        on_link: Rc<dyn Fn(&str)>,
        issues: Option<crate::issue_pill::SharedIssueIndex>,
        images: Option<DocumentBase>,
    ) -> Self {
        Self {
            root: document_box(),
            context: Context {
                on_link,
                issues,
                images,
            },
            built: RefCell::new(Vec::new()),
        }
    }

    /// Draw `text`, keeping what is drawn of it already.
    pub fn set_text(&self, text: &str) {
        let items = layout(
            text,
            self.context.issues.as_ref(),
            self.context.images.as_ref(),
        );
        let mut built = self.built.borrow_mut();
        let mut kept = built
            .iter()
            .zip(&items)
            .take_while(|(built, item)| built.item == **item)
            .count();
        // The run being written: its label given the run's new markup.
        if let (Some(old), Some(new)) = (built.get_mut(kept), items.get(kept)) {
            if let (
                Item::Run {
                    quote_depth: was, ..
                },
                Item::Run {
                    markup,
                    spans,
                    quote_depth: is,
                },
                Some((label, span_texts)),
            ) = (&old.item, new, &old.run)
            {
                if was == is {
                    label.set_markup(markup);
                    *span_texts.borrow_mut() = spans.clone();
                    old.item = new.clone();
                    kept += 1;
                }
            }
        }
        for gone in built.drain(kept..) {
            self.root.remove(&gone.widget);
        }
        for item in &items[kept..] {
            let made = build(item, &self.context, None);
            self.root.append(&made.widget);
            built.push(made);
        }
    }
}

/// What a block of prose was, for the space above the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Paragraph,
    Heading,
    Item,
}

/// The space between two blocks of a run, as an empty line of the given
/// size: what the box's spacing was when each block was a label of its
/// own, about ten pixels, and less between the items of one list, which
/// read as one block. A heading takes more above it than below.
fn gap(previous: Block, next: Block) -> &'static str {
    match (previous, next) {
        (Block::Item, Block::Item) => "<span font_size=\"3pt\">\n</span>",
        (_, Block::Heading) => "<span font_size=\"9pt\">\n</span>",
        _ => "<span font_size=\"6pt\">\n</span>",
    }
}

/// A heading's type: libadwaita's `title-1` to `title-4`, which a heading
/// wore as a class when it was a label of its own, as Pango sizes them
/// relative to the body (20, 15, 15, and 13 points over 11).
fn heading_type(level: HeadingLevel) -> (&'static str, u16) {
    match level {
        HeadingLevel::H1 => ("181%", 800),
        HeadingLevel::H2 => ("136%", 800),
        HeadingLevel::H3 => ("136%", 700),
        _ => ("118%", 700),
    }
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
fn picture(base: &DocumentBase, path: PathBuf, alt: &str, hold: Option<Hold>) -> gtk::Widget {
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
        // Outstanding until this task ends, whichever way it ends.
        let _hold = hold;
        let read_path = path.clone();
        let loaded = crate::runtime::runtime()
            .spawn_blocking(move || -> Result<gtk::gdk::Texture, String> {
                let bytes = files
                    .read(&read_path)
                    .map_err(|e| format!("could not be read: {e}"))?;
                crate::glycin::decode(&glib::Bytes::from_owned(bytes))
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
///
/// Laid out by [`crate::table::Table`], not a `GtkGrid`: the grid measured
/// its height as though every column were a word wide, and a transcript row
/// kept the difference as a page of empty space.
fn table_card(alignments: &[pulldown_cmark::Alignment], rows: &[Vec<String>]) -> gtk::Widget {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0) as i32;
    let mut cells: Vec<Vec<gtk::Widget>> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let mut line = Vec::new();
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
            line.push(label.upcast());
        }
        cells.push(line);
    }
    let table = crate::table::Table::new(cells);
    table.add_css_class("markdown-table");
    // As wide as its columns, never stretched: its border and its striped
    // rows are the columns' own width, and a wide pane leaves the rest
    // empty beside it rather than inside it.
    table.set_halign(gtk::Align::Start);
    table.upcast()
}

/// Whether a link's address is one of its run's code spans, which a click
/// copies: their address is their own text ([`layout`]).
fn is_copy_span(spans: &[String], href: &str) -> bool {
    spans.iter().any(|span| span == href)
}

pub(crate) fn find_toast_overlay(widget: &gtk::Widget) -> Option<adw::ToastOverlay> {
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

    /// The labels a rendering set its prose in, in order, by their text.
    fn prose(widget: &gtk::Widget) -> Vec<String> {
        let mut found = Vec::new();
        let mut child = widget.first_child();
        while let Some(current) = child {
            match current.downcast_ref::<gtk::Label>() {
                // A code card's own label is monospace; prose is not.
                Some(label) if label.is_selectable() && !label.has_css_class("monospace") => {
                    found.push(label.text().to_string())
                }
                _ => found.extend(prose(&current)),
            }
            child = current.next_sibling();
        }
        found
    }

    /// A document streamed into [`Incremental`] a few characters at a
    /// time: a block, once another follows it, keeps its widget through
    /// every later update — nothing drawn is drawn again — and what is
    /// drawn at the end is what [`render_in`] draws of the whole text.
    #[test]
    fn a_streamed_document_keeps_what_it_has_drawn() {
        crate::gtk_test::on_gtk_thread("markdown streaming: no display — skipped", || {
            let text = "# Plan\n\nFirst we **read** the `config`.\n\n- one\n- two\n\n\
                        ```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n\
                        ---\n\nAnd a last paragraph, long enough to wrap in the column.";
            let doc = Incremental::new(Rc::new(|_: &str| {}), None, None);
            let children = |root: &gtk::Box| {
                let mut out = Vec::new();
                let mut child = root.first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    out.push(widget);
                }
                out
            };
            let mut seen: Vec<gtk::Widget> = Vec::new();
            let bytes: Vec<usize> = (0..=text.len())
                .step_by(7)
                .chain(std::iter::once(text.len()))
                .filter(|at| text.is_char_boundary(*at))
                .collect();
            for at in bytes {
                doc.set_text(&text[..at]);
                let now = children(&doc.root);
                // Every block but the last one drawn before this update is
                // the same widget now.
                for (index, before) in seen.iter().enumerate().take(seen.len().saturating_sub(1)) {
                    assert_eq!(
                        now.get(index),
                        Some(before),
                        "block {index} was drawn again at byte {at}"
                    );
                }
                seen = now;
            }
            let whole = render_in(text, Rc::new(|_: &str| {}), None, None);
            let drawn = children(&doc.root);
            let rendered = children(whole.downcast_ref::<gtk::Box>().unwrap());
            assert_eq!(drawn.len(), rendered.len());
            for (a, b) in drawn.iter().zip(&rendered) {
                assert_eq!(a.type_(), b.type_());
            }
            assert_eq!(prose(doc.root.upcast_ref()), prose(&whole));
        });
    }

    /// What a streamed answer costs a frame: an answer of about 4 KB —
    /// paragraphs, a list, code — drawn as it grows, 40 bytes an update,
    /// the slowest update and the mean printed. `cargo test -p taste-app
    /// perf_streamed -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perf_streamed_answer() {
        crate::gtk_test::on_gtk_thread("perf: no display — skipped", || {
            let mut text = String::new();
            for n in 0..20 {
                text.push_str(&format!(
                    "Paragraph {n} says something about the change, with **bold** and a \
                     `code span` in it, long enough to wrap twice in a column.\n\n- one \
                     point\n- another point\n\n```rust\nfn f{n}() -> u32 {{ {n} }}\n```\n\n"
                ));
            }
            let doc = Incremental::new(Rc::new(|_: &str| {}), None, None);
            let mut slowest = std::time::Duration::ZERO;
            let mut total = std::time::Duration::ZERO;
            let mut updates = 0u32;
            let mut at = 0;
            while at < text.len() {
                at = (at + 40).min(text.len());
                while !text.is_char_boundary(at) {
                    at += 1;
                }
                let started = std::time::Instant::now();
                doc.set_text(&text[..at]);
                let took = started.elapsed();
                slowest = slowest.max(took);
                total += took;
                updates += 1;
            }
            println!(
                "perf streamed answer: {} bytes in {updates} updates, mean {:.2} ms, slowest {:.2} ms",
                text.len(),
                total.as_secs_f64() * 1000.0 / f64::from(updates),
                slowest.as_secs_f64() * 1000.0
            );
        });
    }

    /// A heading, its paragraphs, and a list are ONE label, so a selection
    /// can run from the heading to the last bullet; a code block is a
    /// widget of its own and starts the next run.
    /// Code spans link to their own text, so the label's "Copy Link
    /// Address" copies the code — it copied `copy:0` — and a URL written as
    /// code links to the URL itself.
    #[test]
    fn a_code_span_links_to_what_it_says() {
        let items = layout(
            "see `https://example.org/a?b=1` then `cargo test`",
            None,
            None,
        );
        let Some(Item::Run { markup, spans, .. }) = items.first() else {
            panic!("a run: {items:?}");
        };
        assert!(
            markup.contains("<a href=\"https://example.org/a?b=1\""),
            "{markup}"
        );
        assert!(markup.contains("<a href=\"cargo test\""), "{markup}");
        assert!(!markup.contains("copy:"), "{markup}");
        assert_eq!(spans, &["cargo test".to_string()]);
        assert!(is_copy_span(spans, "cargo test"));
        assert!(!is_copy_span(spans, "https://example.org/a?b=1"));
    }

    #[test]
    fn prose_runs_are_one_label_so_a_selection_can_cross_them() {
        crate::gtk_test::on_gtk_thread("markdown runs: no display — skipped", || {
            let rendered = render(
                "# Title\n\nFirst paragraph.\n\n- one\n- two\n  - nested\n\nAfter the list.\n\n\
                 ```\ncode\n```\n\nAfter the code.",
                Rc::new(|_: &str| {}),
            );
            let runs = prose(&rendered);
            assert_eq!(runs.len(), 2, "{runs:?}");
            let first = &runs[0];
            for part in [
                "Title",
                "First paragraph.",
                "• one",
                "• two",
                "• nested",
                "After the list.",
            ] {
                assert!(first.contains(part), "{part} missing from {first:?}");
            }
            assert!(!first.contains("code"), "{first:?}");
            assert_eq!(runs[1], "After the code.");
        });
    }

    /// A long document is several runs, each a section or so: a label
    /// draws itself whole for each change of its selection.
    #[test]
    fn a_long_document_is_runs_of_bounded_length() {
        crate::gtk_test::on_gtk_thread("markdown runs: no display — skipped", || {
            let paragraph = "Words in a paragraph that goes on for a while. ".repeat(12);
            let mut doc = String::new();
            for section in 0..20 {
                doc.push_str(&format!("## Section {section}\n\n"));
                for _ in 0..4 {
                    doc.push_str(&paragraph);
                    doc.push_str("\n\n");
                }
            }
            let runs = prose(&render(&doc, Rc::new(|_: &str| {})));
            assert!(runs.len() > 5, "{} runs", runs.len());
            for run in &runs {
                assert!(run.len() < RUN_MOST + 2 * paragraph.len(), "{}", run.len());
            }
            // A short one with headings among it is still one.
            let short = prose(&render(
                "## One\n\nA line.\n\n## Two\n\nAnother.",
                Rc::new(|_: &str| {}),
            ));
            assert_eq!(short.len(), 1, "{short:?}");
        });
    }

    #[test]
    fn a_path_link_names_a_file_and_maybe_a_line_and_a_url_does_not() {
        use taste_core::anchor::Anchor;
        assert_eq!(
            base().resolve_link("../crates/taste-app/src/filetree.rs#L4136"),
            Some((
                PathBuf::from("/checkout/crates/taste-app/src/filetree.rs"),
                Some(Anchor::Line(4136))
            ))
        );
        assert_eq!(
            base().resolve_link("ENVIRONMENTS.md:20"),
            Some((
                PathBuf::from("/checkout/docs/ENVIRONMENTS.md"),
                Some(Anchor::Line(20))
            ))
        );
        assert_eq!(
            base().resolve_link("talk/slides.pdf#page=3"),
            Some((
                PathBuf::from("/checkout/docs/talk/slides.pdf"),
                Some(Anchor::Page(3))
            ))
        );
        assert_eq!(
            base().resolve_link("/README.md"),
            Some((PathBuf::from("/checkout/README.md"), None))
        );
        assert_eq!(base().resolve_link("https://example.org/a.md"), None);
        assert_eq!(base().resolve_link("#heading"), None);
        assert_eq!(base().resolve_link("../../etc/passwd"), None);
    }

    /// The decode the preview relies on, glycin's, against a real
    /// screenshot of the docs set: a PNG the size of the window comes back
    /// as a texture of that size, off any thread.
    #[test]
    fn a_docs_screenshot_decodes_to_a_texture() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots/hero.png");
        let bytes = std::fs::read(&path).expect("the docs set has a hero shot");
        let texture = crate::glycin::tests::decode_here(&glib::Bytes::from_owned(bytes))
            .expect("a PNG decodes");
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
        // An absolute path in the checkout, as an agent names a file it
        // just wrote, is that file.
        assert_eq!(
            base().resolve("/checkout/target/shots/start-3.png"),
            Some(PathBuf::from("/checkout/target/shots/start-3.png"))
        );
        assert_eq!(base().resolve("../../etc/passwd"), None);
        assert_eq!(base().resolve("https://example.org/a.png"), None);
        assert_eq!(base().resolve("data:image/png;base64,AAAA"), None);
    }
}
