//! What a transcript step shows in brief, and what it opens as, whole.
//!
//! David, 2026-09-07: the chat "needs to be much more dense and truncate
//! (while allowing opening the full versions in the editor area) huge
//! requests and responses"; diffs "should be side-by-side unless the chat
//! is too narrow to render that without wrapping. Then, it can be stacked:
//! removed, then added"; commands "should use the IN/OUT format of Claude
//! Code chat. Truncate huge commands and replies, but allow the pair to be
//! opened as a full editor pane."
//!
//! So this module is two things. The BLOCKS a step in the transcript shows
//! — a command with its output, an edit as a diff, a run of terminal
//! output with its colours read — each able to stop after so many lines
//! and say how many it left out. And the DOCUMENT those blocks open as: a
//! read-only page in the editor's strip beside the files
//! (`editor.rs::open_document`), where the whole of a prompt, a response, a
//! command's output or an edit is read at the editor's width rather than
//! the chat's. Nothing here knows about a chat or a session: it is handed
//! text and renders it, which is what lets the chat and the editor draw the
//! same edit the same way.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use similar::{DiffOp, TextDiff};

/// A whole thing the transcript showed in brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    /// A prompt, a response, or a tool's result. `markdown` says whether it
    /// is rendered (a response) or shown as typed; `role` is who wrote it,
    /// which is the tab's glyph.
    Text {
        title: String,
        body: String,
        markdown: bool,
        role: TextRole,
    },
    /// A command and what it printed: the IN/OUT pair.
    Command { command: String, output: String },
    /// A proposed or applied edit to one file.
    Edit(Edit),
}

/// Whose words a text document holds: the tab wears the role's glyph — the
/// human's for a prompt, the agent's for a response — and the title is the
/// moment it was said (David, 2026-09-07: "some sort of prompt/AI icon and
/// the timestamp of the content").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    Prompt,
    Response,
    Result,
}

/// One file's before and after. Our own type rather than the protocol's
/// `Diff`, so the editor page depends on text, not on the agent wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
}

impl Document {
    /// The tab's title: what this is, in a few words.
    pub fn title(&self) -> String {
        match self {
            Document::Text { title, .. } => title.clone(),
            Document::Command { command, .. } => crate::chat::single_line(command, 40),
            // "Edit filetree.rs", not "filetree.rs": the file's own tab
            // may be open beside it, and the two must not read alike.
            Document::Edit(edit) => format!(
                "Edit {}",
                edit.path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| edit.path.display().to_string())
            ),
        }
    }

    /// The tab's icon: the same glyph the permission card types the ask
    /// with, so a command opened from the chat wears the terminal and an
    /// edit the pencil.
    pub fn icon(&self) -> &'static str {
        match self {
            Document::Text {
                role: TextRole::Prompt,
                ..
            } => "taste-human-symbolic",
            Document::Text {
                role: TextRole::Response,
                ..
            } => "taste-agent-symbolic",
            Document::Text { .. } => "text-x-generic-symbolic",
            Document::Command { .. } => "utilities-terminal-symbolic",
            Document::Edit(_) => "document-edit-symbolic",
        }
    }

    /// The tab's tooltip: the whole title where the tab ellipsized it.
    pub fn tooltip(&self) -> String {
        match self {
            Document::Text {
                title,
                role: TextRole::Prompt,
                ..
            } => format!("Prompt sent {title}"),
            Document::Text {
                title,
                role: TextRole::Response,
                ..
            } => format!("Response from {title}"),
            Document::Text { title, .. } => title.clone(),
            Document::Command { command, .. } => crate::chat::single_line(command, 400),
            Document::Edit(edit) => edit.path.display().to_string(),
        }
    }
}

/// The document as an editor page: read-only, scrolled, at full width.
pub struct DocPage {
    pub widget: gtk::Widget,
    /// The tab's glyph (`Document::icon`), kept for the editor's badge
    /// pass, which draws every surface's icon from the surface itself.
    pub icon: &'static str,
}

impl DocPage {
    pub fn new(doc: &Document, on_link: Rc<dyn Fn(&str)>) -> Rc<Self> {
        let content: gtk::Widget = match doc {
            Document::Text {
                body,
                markdown: true,
                ..
            } => crate::markdown_view::render(body, on_link),
            Document::Text { body, .. } => {
                let view = gtk::TextView::builder()
                    .editable(false)
                    .cursor_visible(false)
                    .wrap_mode(gtk::WrapMode::WordChar)
                    .top_margin(PAGE_INSET)
                    .bottom_margin(PAGE_INSET)
                    .left_margin(PAGE_INSET)
                    .right_margin(PAGE_INSET)
                    .build();
                view.buffer().set_text(body);
                crate::chat::suppress_hyphens(&view.buffer());
                view.upcast()
            }
            Document::Command { command, output } => {
                inset(&command_block(command, output, None, None))
            }
            Document::Edit(edit) => {
                let view = diff_view(edit, None, Layout::SideBySide);
                let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
                column.append(&diff_header(edit, view.added, view.removed, None));
                column.append(&view.widget);
                inset(&column)
            }
        };
        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .hexpand(true)
            .vexpand(true)
            // Prose folds; a side-by-side diff's lines do not, so the page
            // scrolls sideways for the one and never for the others.
            .hscrollbar_policy(if matches!(doc, Document::Edit(_)) {
                gtk::PolicyType::Automatic
            } else {
                gtk::PolicyType::Never
            })
            .build();
        Rc::new(Self {
            widget: scroller.upcast(),
            icon: doc.icon(),
        })
    }
}

/// A document page's inset — the markdown preview's own figure, so a
/// response rendered by it and a command beside it stand on one column.
const PAGE_INSET: i32 = 16;

fn inset(widget: &impl IsA<gtk::Widget>) -> gtk::Widget {
    widget.set_margin_top(PAGE_INSET);
    widget.set_margin_bottom(PAGE_INSET);
    widget.set_margin_start(PAGE_INSET);
    widget.set_margin_end(PAGE_INSET);
    widget.clone().upcast()
}

// --- text, clipped ------------------------------------------------------

/// The first `max` lines of `text`, and how many it left out.
pub fn clip_lines(text: &str, max: usize) -> (String, usize) {
    let total = text.lines().count();
    if total <= max {
        return (text.to_string(), 0);
    }
    let head: Vec<&str> = text.lines().take(max).collect();
    (head.join("\n"), total - max)
}

/// The head of a response, cut where a reader can stand: after `max_lines`
/// lines or `max_chars` characters, at the last blank line before the
/// bound when one falls in the second half of the head (so a cut lands
/// between paragraphs rather than mid-sentence), and with an open code
/// fence closed, so the head renders as the whole would have. Returns the
/// head and how many lines were left out.
pub fn clip_prose(text: &str, max_lines: usize, max_chars: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut cut = 0;
    let mut chars = 0;
    while cut < total && cut < max_lines {
        chars += lines[cut].chars().count() + 1;
        if chars > max_chars && cut > 0 {
            break;
        }
        cut += 1;
    }
    if cut >= total {
        return (text.to_string(), 0);
    }
    // Prefer a paragraph boundary in the second half of what fits.
    if let Some(blank) = (cut / 2..cut).rev().find(|&i| lines[i].trim().is_empty()) {
        cut = blank;
    }
    let mut head: Vec<&str> = lines[..cut].to_vec();
    // Trailing blank lines carry nothing into the head.
    while head.last().is_some_and(|line| line.trim().is_empty()) {
        head.pop();
    }
    let fences = head
        .iter()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    let mut out = head.join("\n");
    if fences % 2 == 1 {
        out.push_str("\n```");
    }
    (out, total - head.len())
}

/// The one line a collapsed command step shows under the command: the
/// last line the command printed, which is where a build says `ok` or
/// `FAILED`. None when it printed nothing. A line that is only a marker in
/// angle brackets — Copilot's `<shellId: 1 completed with exit code 0>` —
/// is the tool's, not the command's, and is skipped.
pub fn digest(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .map(|line| crate::chat::single_line(&strip_ansi(line), 160))
        .find(|line| !line.is_empty() && !(line.starts_with('<') && line.ends_with('>')))
}

/// `text` with every escape sequence dropped — for a digest, where colour
/// is not going to be rendered.
fn strip_ansi(text: &str) -> String {
    crate::chat::ansi_spans(text)
        .into_iter()
        .map(|span| span.text)
        .collect()
}

/// A dim "… N more lines" — the truncation saying so, in the block's own
/// last line rather than a footnote.
fn more_text(hidden: usize) -> String {
    if hidden == 1 {
        "… 1 more line".to_string()
    } else {
        format!("… {hidden} more lines")
    }
}

// --- terminal output, coloured ------------------------------------------

/// `text` as a label, its SGR colour and weight read into Pango markup and
/// everything else it carried as escapes discarded (`chat::ansi_spans`).
/// With `clip`, the first so many lines and a last dim line saying how many
/// more there were.
///
/// A label, not a text view, for the block a step shows: a `GtkTextView`
/// reports the height of its LAST layout, which on first measure is the
/// height at its minimum width — wrapped hard, and so far too tall — and
/// in a plain container that answer sticks (the OUT block was allocated a
/// screen of blank under ten lines of text). A label measures
/// height-for-width honestly.
pub fn ansi_label(text: &str, clip: Option<usize>) -> gtk::Label {
    let (shown, hidden) = match clip {
        Some(max) => clip_lines(text, max),
        None => (text.to_string(), 0),
    };
    let mut markup = ansi_markup(&shown);
    if hidden > 0 {
        if !markup.is_empty() {
            markup.push('\n');
        }
        markup.push_str(&format!(
            "<span foreground=\"{}\">{}</span>",
            crate::palette::MUTED,
            glib::markup_escape_text(&more_text(hidden))
        ));
    }
    // Lines do not fold: a test name or a path broken at an arbitrary
    // character is unreadable, and Claude Code's OUT cuts each line at
    // the box instead. Without wrapping, Pango ellipsizes every line at the
    // width on its own — and the whole of every line is in the editor.
    gtk::Label::builder()
        .label(markup)
        .use_markup(true)
        .attributes(&crate::chat::no_hyphens())
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .focusable(false)
        .css_classes(["monospace"])
        .build()
}

/// The ANSI runs of `text` as Pango markup: colour and weight, in the
/// palette's text colours; everything else escaped.
pub fn ansi_markup(text: &str) -> String {
    let mut out = String::new();
    for span in crate::chat::ansi_spans(text) {
        let escaped = glib::markup_escape_text(&span.text);
        if span.color.is_none() && !span.bold && !span.dim {
            out.push_str(&escaped);
            continue;
        }
        out.push_str("<span");
        if span.color.is_some() || span.dim {
            let color = span.color.unwrap_or(8).min(15);
            out.push_str(&format!(
                " foreground=\"{}\"",
                crate::palette::ANSI_TEXT[color as usize]
            ));
        }
        if span.bold {
            out.push_str(" weight=\"bold\"");
        }
        out.push('>');
        out.push_str(&escaped);
        out.push_str("</span>");
    }
    out
}

/// `text` in a read-only monospace view, coloured the same way: the editor
/// page's block, where a scroller gives the view its width before it is
/// asked for its height.
pub fn ansi_view(text: &str) -> gtk::TextView {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    let buffer = view.buffer();
    fill_ansi(&buffer, text);
    crate::chat::suppress_hyphens(&buffer);
    view
}

/// Insert `text` into `buffer` with its ANSI runs as tags — one tag per
/// distinct style, reused: a long log is thousands of spans and a tag each
/// would be thousands of objects.
fn fill_ansi(buffer: &gtk::TextBuffer, text: &str) {
    let table = buffer.tag_table();
    for span in crate::chat::ansi_spans(text) {
        let mut end = buffer.end_iter();
        let start_offset = end.offset();
        buffer.insert(&mut end, &span.text);
        if span.color.is_none() && !span.bold && !span.dim {
            continue;
        }
        let name = format!(
            "ansi-{}-{}-{}",
            span.color.map_or(-1, i16::from),
            span.bold,
            span.dim
        );
        let tag = table.lookup(&name).unwrap_or_else(|| {
            let tag = gtk::TextTag::builder().name(&name).build();
            // Dim with no colour of its own is the palette's bright black,
            // which is what a terminal renders SGR 2 as: a grey that stays
            // legible against either background.
            let color = span.color.unwrap_or(8).min(15);
            if span.color.is_some() || span.dim {
                if let Ok(rgba) =
                    crate::palette::ANSI_TEXT[color as usize].parse::<gtk::gdk::RGBA>()
                {
                    tag.set_foreground_rgba(Some(&rgba));
                }
            }
            if span.bold {
                tag.set_weight(700);
            }
            table.add(&tag);
            tag
        });
        let start = buffer.iter_at_offset(start_offset);
        buffer.apply_tag(&tag, &start, &end);
    }
}

// --- a command and its output: IN / OUT ---------------------------------

/// How many lines of a command itself a step shows before it ellipsizes —
/// a heredoc or a pasted script is the IN, but not the point.
const COMMAND_CLIP_LINES: usize = 6;

/// Claude Code's shape for a command: `IN` and the command, `OUT` and what
/// it printed, the tags in a narrow dim column so the eye reads down the
/// text. With `clip`, both halves stop after so many lines; `open`, when
/// given, is the button at the pair's corner that opens the whole of it.
///
/// Boxes, not a grid, though a grid is the obvious shape: a `GtkGrid`
/// measured its wrapping labels at their minimum width, and a step's row
/// reserved ten thousand pixels for ten lines of output. A `GtkBox` asks
/// height-for-width, and the two tags are the same width, so the text
/// column lines up anyway.
pub fn command_block(
    command: &str,
    output: &str,
    clip: Option<usize>,
    open: Option<Rc<dyn Fn()>>,
) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);
    let in_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    in_row.append(&io_tag("IN"));
    // The command, one line per line of it — each cut at the box like the
    // output's, and a script cut to its first lines (in the text: a
    // label's line limit is per paragraph and would not cut it).
    let (head, hidden) = match clip {
        Some(_) => clip_lines(command.trim_end(), COMMAND_CLIP_LINES),
        None => (command.trim_end().to_string(), 0),
    };
    let mut markup = glib::markup_escape_text(&head).to_string();
    if hidden > 0 {
        markup.push_str(&format!(
            "\n<span foreground=\"{}\">{}</span>",
            crate::palette::MUTED,
            glib::markup_escape_text(&more_text(hidden))
        ));
    }
    let command_label = gtk::Label::builder()
        .label(markup)
        .use_markup(true)
        .attributes(&crate::chat::no_hyphens())
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .focusable(false)
        .css_classes(["monospace"])
        .build();
    in_row.append(&command_label);
    if let Some(open) = open {
        in_row.append(&open_button(
            "Open the command and its output in the editor",
            open,
        ));
    }
    column.append(&in_row);
    if !output.trim().is_empty() {
        let out_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        out_row.append(&io_tag("OUT"));
        let out: gtk::Widget = match clip {
            Some(_) => ansi_label(output, clip).upcast(),
            None => {
                let view = ansi_view(output);
                view.set_hexpand(true);
                view.upcast()
            }
        };
        out_row.append(&out);
        column.append(&out_row);
    }
    column.upcast()
}

/// `IN` / `OUT`: caption, dim, monospace so the two are one width, top-
/// aligned against the first line of what they tag.
fn io_tag(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .width_chars(3)
        .css_classes(["caption", "dim-label", "monospace", "io-tag"])
        .build()
}

/// The one gesture every truncated block offers: open the whole thing in
/// the editor. A flat icon at the block's corner, no label — it sits beside
/// text that already says what it is.
pub fn open_button(tooltip: &str, open: Rc<dyn Fn()>) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text(tooltip)
        .css_classes(["flat", "circular", "open-whole"])
        .valign(gtk::Align::Start)
        .build();
    button.connect_clicked(move |_| open());
    button
}

// --- an edit, side by side or stacked -----------------------------------

/// How a diff lays itself out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Side by side when both columns fit unwrapped in the width the
    /// block is given, else stacked — one unified block, each change's
    /// removed lines then its added ones. Decided from the block's own
    /// allocation and redecided as it changes.
    Auto,
    /// Side by side whatever the width, scrolling sideways when a line
    /// is longer than half of it: the editor page.
    SideBySide,
}

/// One line's part in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Context,
    Removed,
    Added,
    /// The blank a side shows opposite a line the other side has and it
    /// does not, so the two columns stay in step.
    Pad,
}

/// One row of the side-by-side view: what the old side shows and what the
/// new side shows on the same line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub left: (Mark, String),
    pub right: (Mark, String),
}

/// A run of changes with three lines of context around it, the way a
/// unified diff groups them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first line of the hunk on each side, and how many lines of
    /// each side it covers.
    pub old_from: usize,
    pub old_len: usize,
    pub new_from: usize,
    pub new_len: usize,
    pub rows: Vec<Row>,
}

impl Hunk {
    fn changed(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.left.0 == Mark::Removed || row.right.0 == Mark::Added)
    }

    /// The hunk as unified lines: `(mark, text)` in the order a unified
    /// diff prints them — context as it comes, and within each run of
    /// changes every removed line before every added one.
    pub fn unified(&self) -> Vec<(Mark, String)> {
        let mut out = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        let mut added: Vec<String> = Vec::new();
        let flush =
            |out: &mut Vec<(Mark, String)>, removed: &mut Vec<String>, added: &mut Vec<String>| {
                out.extend(removed.drain(..).map(|line| (Mark::Removed, line)));
                out.extend(added.drain(..).map(|line| (Mark::Added, line)));
            };
        for row in &self.rows {
            if row.left.0 == Mark::Context {
                flush(&mut out, &mut removed, &mut added);
                out.push((Mark::Context, row.left.1.clone()));
                continue;
            }
            if row.left.0 == Mark::Removed {
                removed.push(row.left.1.clone());
            }
            if row.right.0 == Mark::Added {
                added.push(row.right.1.clone());
            }
        }
        flush(&mut out, &mut removed, &mut added);
        out
    }
}

/// The hunks of `old` → `new`, each row of each hunk paired across the two
/// sides: a replaced run pairs its removed lines with its added ones line
/// for line and pads the shorter side, so the columns stay in step.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let olds = diff.old_slices();
    let news = diff.new_slices();
    let line = |text: &str| text.trim_end_matches(['\n', '\r']).to_string();
    let mut out = Vec::new();
    for group in diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_from = first.old_range().start;
        let new_from = first.new_range().start;
        let mut hunk = Hunk {
            old_from: old_from + 1,
            old_len: last.old_range().end - old_from,
            new_from: new_from + 1,
            new_len: last.new_range().end - new_from,
            rows: Vec::new(),
        };
        for op in &group {
            match *op {
                DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                } => {
                    for i in 0..len {
                        hunk.rows.push(Row {
                            left: (Mark::Context, line(olds[old_index + i])),
                            right: (Mark::Context, line(news[new_index + i])),
                        });
                    }
                }
                DiffOp::Delete {
                    old_index, old_len, ..
                } => {
                    for i in 0..old_len {
                        hunk.rows.push(Row {
                            left: (Mark::Removed, line(olds[old_index + i])),
                            right: (Mark::Pad, String::new()),
                        });
                    }
                }
                DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    for i in 0..new_len {
                        hunk.rows.push(Row {
                            left: (Mark::Pad, String::new()),
                            right: (Mark::Added, line(news[new_index + i])),
                        });
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    for i in 0..old_len.max(new_len) {
                        hunk.rows.push(Row {
                            left: if i < old_len {
                                (Mark::Removed, line(olds[old_index + i]))
                            } else {
                                (Mark::Pad, String::new())
                            },
                            right: if i < new_len {
                                (Mark::Added, line(news[new_index + i]))
                            } else {
                                (Mark::Pad, String::new())
                            },
                        });
                    }
                }
            }
        }
        out.push(hunk);
    }
    out
}

/// Where two paired lines differ, word by word: the char ranges of `old`
/// that were removed and of `new` that were added. Empty when so much of a
/// line changed that emphasis would cover it — then the line's own wash is
/// the honest signal, as VS Code's diff also decides.
pub fn changed_spans(old: &str, new: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    use similar::ChangeTag;
    let diff = TextDiff::from_words(old, new);
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let (mut at_old, mut at_new) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        let len = change.value().chars().count();
        match change.tag() {
            ChangeTag::Equal => {
                at_old += len;
                at_new += len;
            }
            ChangeTag::Delete => {
                removed.push((at_old, at_old + len));
                at_old += len;
            }
            ChangeTag::Insert => {
                added.push((at_new, at_new + len));
                at_new += len;
            }
        }
    }
    let changed: usize = removed.iter().chain(&added).map(|(a, b)| b - a).sum();
    let whole = old.chars().count().max(new.chars().count()) * 2;
    if whole > 0 && changed * 4 > whole * 3 {
        return (Vec::new(), Vec::new());
    }
    (removed, added)
}

/// One line of a diff column: its mark, its text, and the char ranges in
/// it to emphasize.
type SideLine = (Mark, String, Vec<(usize, usize)>);

/// A rendered diff: the widget and its size. What it left out, it says
/// itself, in its last line.
pub struct DiffView {
    pub widget: gtk::Widget,
    pub added: usize,
    pub removed: usize,
}

/// The diff of `edit`, in `layout`. With `clip`, only the first so many
/// rows across its hunks, then a dim line saying how many more there were.
pub fn diff_view(edit: &Edit, clip: Option<usize>, layout: Layout) -> DiffView {
    let hunks = hunks(&edit.old, &edit.new);
    let language = sourceview5::LanguageManager::default()
        .guess_language(Some(edit.path.to_string_lossy().as_ref()), None);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let mut budget = clip;
    let mut hidden = 0;
    let (mut added, mut removed) = (0, 0);
    let mut longest = 0usize;
    // Each hunk's two columns, and the same hunk as one unified block: the
    // layout shows one or the other.
    let mut faces: Vec<(gtk::ScrolledWindow, gtk::Widget)> = Vec::new();
    let mut buffers: Vec<glib::WeakRef<sourceview5::Buffer>> = Vec::new();
    let mut probe: Option<sourceview5::View> = None;
    for hunk in &hunks {
        for row in &hunk.rows {
            added += usize::from(row.right.0 == Mark::Added);
            removed += usize::from(row.left.0 == Mark::Removed);
        }
        if !hunk.changed() {
            continue;
        }
        if budget == Some(0) {
            hidden += hunk.rows.len();
            continue;
        }
        let rows = match budget {
            Some(left) if hunk.rows.len() > left => {
                hidden += hunk.rows.len() - left;
                &hunk.rows[..left]
            }
            _ => &hunk.rows[..],
        };
        budget = budget.map(|left| left - rows.len());
        for row in rows {
            longest = longest
                .max(row.left.1.chars().count())
                .max(row.right.1.chars().count());
        }
        column.append(
            &gtk::Label::builder()
                .label(format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_from, hunk.old_len, hunk.new_from, hunk.new_len
                ))
                .xalign(0.0)
                .css_classes(["caption", "dim-label", "monospace", "hunk-head"])
                .build(),
        );
        let shown = Hunk {
            rows: rows.to_vec(),
            ..*hunk
        };
        // Side by side, a changed line is read against its pair: the words
        // that differ are emphasized over the line's wash.
        let paired: Vec<(Vec<(usize, usize)>, Vec<(usize, usize)>)> = shown
            .rows
            .iter()
            .map(|row| match (row.left.0, row.right.0) {
                (Mark::Removed, Mark::Added) => changed_spans(&row.left.1, &row.right.1),
                _ => (Vec::new(), Vec::new()),
            })
            .collect();
        let left = side_view(
            shown
                .rows
                .iter()
                .zip(&paired)
                .map(|(row, spans)| (row.left.0, row.left.1.clone(), spans.0.clone())),
            language.as_ref(),
            false,
        );
        let right = side_view(
            shown
                .rows
                .iter()
                .zip(&paired)
                .map(|(row, spans)| (row.right.0, row.right.1.clone(), spans.1.clone())),
            language.as_ref(),
            false,
        );
        let unified = side_view(
            shown
                .unified()
                .into_iter()
                .map(|(mark, text)| (mark, text, Vec::new())),
            language.as_ref(),
            true,
        );
        // Each side's line numbers, in the file's own numbering, blank
        // against a pad — the gutter VS Code's diff has and a unified block
        // spends on its markers instead.
        let (left_numbers, right_numbers) = numbering(&shown);
        let left_gutter = gutter(&left_numbers);
        let right_gutter = gutter(&right_numbers);
        for view in [&left, &right, &unified, &left_gutter, &right_gutter] {
            if let Ok(buffer) = view.buffer().downcast::<sourceview5::Buffer>() {
                buffers.push(buffer.downgrade());
            }
        }
        probe.get_or_insert_with(|| left.clone());
        let column_of = |gutter: &sourceview5::View, view: &sourceview5::View| {
            let column = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            column.set_overflow(gtk::Overflow::Hidden);
            column.add_css_class("diff-block");
            column.append(gutter);
            column.append(view);
            column
        };
        let pair = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .spacing(8)
            .css_classes(["diff-pair"])
            .build();
        pair.append(&column_of(&left_gutter, &left));
        pair.append(&column_of(&right_gutter, &right));
        // The pair sits in a scroller so its unwrapped lines never become
        // the column's minimum width: a diff must not size the chat
        // (docs/ARCHITECTURE.md, "The chat's width is its own"). In the
        // editor the scroller is what scrolls a long line into view; in
        // the chat its bar is never shown, because the pair is only on
        // screen when it fits.
        let pair_scroller = gtk::ScrolledWindow::builder()
            .child(&pair)
            .hscrollbar_policy(match layout {
                Layout::Auto => gtk::PolicyType::External,
                Layout::SideBySide => gtk::PolicyType::Automatic,
            })
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();
        // The unified block folds its lines, so it is a text view whose
        // height has to be asked for after it has its width
        // (`fit_to_content`); the pair does not fold, and a line count is
        // a line count at any width.
        let unified_block = gtk::ScrolledWindow::builder()
            .child(&unified)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .css_classes(["diff-block"])
            .overflow(gtk::Overflow::Hidden)
            .build();
        fit_to_content(&unified_block);
        // Stacked until the layout says otherwise: the first allocation
        // decides, and a pair that starts visible would state a minimum
        // for one frame.
        pair_scroller.set_visible(layout == Layout::SideBySide);
        unified_block.set_visible(layout == Layout::Auto);
        column.append(&pair_scroller);
        column.append(&unified_block);
        faces.push((pair_scroller, unified_block.upcast()));
    }
    if hidden > 0 {
        column.append(
            &gtk::Label::builder()
                .label(more_text(hidden))
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    // The scheme follows the theme: a card outlives a switch, and a light
    // scheme on a dark ground is unreadable. One handler for the whole
    // view, weak to its buffers, so a capped-out card's buffers are still
    // collectable.
    adw::StyleManager::default().connect_dark_notify(move |_| {
        for buffer in buffers.iter().filter_map(glib::WeakRef::upgrade) {
            apply_scheme(&buffer);
        }
    });
    if layout == Layout::Auto {
        if let Some(probe) = probe {
            watch_width(&column, probe, longest, faces);
        }
    }
    DiffView {
        widget: column.upcast(),
        added,
        removed,
    }
}

/// The path, the size, and the way out: the line above a diff, in the
/// chat and on the editor page alike.
pub fn diff_header(
    edit: &Edit,
    added: usize,
    removed: usize,
    open: Option<Rc<dyn Fn()>>,
) -> gtk::Widget {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let path = gtk::Label::builder()
        .label(edit.path.to_string_lossy())
        .attributes(&crate::chat::no_hyphens())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .tooltip_text(edit.path.to_string_lossy())
        .css_classes(["dim-label", "caption", "monospace"])
        .build();
    line.append(&path);
    line.append(&change_count(added, removed));
    if let Some(open) = open {
        line.append(&open_button("Open the edit in the editor", open));
    }
    line.upcast()
}

/// `+12 −3`, each in the diff's own colour.
pub fn change_count(added: usize, removed: usize) -> gtk::Widget {
    let counts = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    counts.set_valign(gtk::Align::Center);
    let plus = gtk::Label::builder()
        .label(format!("+{added}"))
        .css_classes(["caption", "monospace", "diff-added"])
        .build();
    let minus = gtk::Label::builder()
        .label(format!("−{removed}"))
        .css_classes(["caption", "monospace", "diff-removed"])
        .build();
    counts.append(&plus);
    counts.append(&minus);
    counts.upcast()
}

/// The line numbers each side of a hunk shows: the file's own, blank
/// against a pad.
fn numbering(hunk: &Hunk) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let (mut old, mut new) = (hunk.old_from, hunk.new_from);
    let mut left = Vec::with_capacity(hunk.rows.len());
    let mut right = Vec::with_capacity(hunk.rows.len());
    for row in &hunk.rows {
        left.push((row.left.0 != Mark::Pad).then(|| {
            old += 1;
            old - 1
        }));
        right.push((row.right.0 != Mark::Pad).then(|| {
            new += 1;
            new - 1
        }));
    }
    (left, right)
}

/// A column of line numbers beside a diff column: a source view like its
/// neighbour, so the two share a scheme, a font and a line height, with the
/// numbers dimmed and set right.
fn gutter(numbers: &[Option<usize>]) -> sourceview5::View {
    let width = numbers
        .iter()
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len());
    let text: Vec<String> = numbers
        .iter()
        .map(|n| match n {
            Some(n) => format!("{n:>width$}"),
            None => " ".repeat(width),
        })
        .collect();
    let buffer = sourceview5::Buffer::new(None);
    apply_scheme(&buffer);
    buffer.set_text(&text.join("\n"));
    let dim = gtk::TextTag::builder().name("gutter").build();
    dim.set_foreground_rgba(Some(&crate::palette::rgba(crate::palette::MUTED)));
    buffer.tag_table().add(&dim);
    buffer.apply_tag(&dim, &buffer.start_iter(), &buffer.end_iter());
    let view = sourceview5::View::builder()
        .buffer(&buffer)
        .editable(false)
        .cursor_visible(false)
        .can_focus(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(4)
        .bottom_margin(4)
        .left_margin(SIDE_INSET)
        .right_margin(SIDE_INSET)
        .css_classes(["diff-side", "diff-gutter"])
        .build();
    // A text view asks for next to no width and scrolls the rest away, so
    // beside an expanding neighbour the numbers showed one digit. The
    // width is stated: so many digits of the view's own font, measured
    // again on realize, when the font is the styled one.
    let size = move |view: &sourceview5::View| {
        let digit = view
            .pango_context()
            .metrics(None, None)
            .approximate_digit_width()
            / gtk::pango::SCALE;
        view.set_size_request(digit.max(6) * width as i32 + 2 * SIDE_INSET, -1);
    };
    size(&view);
    view.connect_realize(size);
    view
}

/// One column of a diff (or the unified block, with `prefixed` markers):
/// a source view in the file's language, each line's paragraph washed by
/// its mark, and the changed words within a paired line washed stronger.
fn side_view(
    lines: impl Iterator<Item = SideLine>,
    language: Option<&sourceview5::Language>,
    prefixed: bool,
) -> sourceview5::View {
    let buffer = sourceview5::Buffer::new(None);
    if let Some(language) = language {
        sourceview5::prelude::BufferExt::set_language(&buffer, Some(language));
    }
    apply_scheme(&buffer);
    let table = buffer.tag_table();
    for (name, wash) in [
        ("diff-add", crate::palette::DIFF_ADDED_WASH),
        ("diff-del", crate::palette::DIFF_REMOVED_WASH),
        ("diff-pad", crate::palette::DIFF_PAD_WASH),
    ] {
        let tag = gtk::TextTag::builder().name(name).build();
        tag.set_paragraph_background_rgba(Some(&crate::palette::rgba(wash)));
        table.add(&tag);
    }
    for (name, wash) in [
        ("diff-add-strong", crate::palette::DIFF_ADDED_STRONG),
        ("diff-del-strong", crate::palette::DIFF_REMOVED_STRONG),
    ] {
        let tag = gtk::TextTag::builder().name(name).build();
        tag.set_background_rgba(Some(&crate::palette::rgba(wash)));
        table.add(&tag);
    }
    let mut first = true;
    for (mark, text, spans) in lines {
        let mut end = buffer.end_iter();
        if !first {
            buffer.insert(&mut end, "\n");
        }
        first = false;
        let start_offset = end.offset();
        // The `+ `/`- ` prefixes stay on the unified block: colour alone is
        // not a signal everyone receives, and they survive being copied
        // out. Side by side, the column is the signal.
        let prefix = match (prefixed, mark) {
            (true, Mark::Added) => "+ ",
            (true, Mark::Removed) => "- ",
            (true, _) => "  ",
            (false, _) => "",
        };
        buffer.insert(&mut end, &format!("{prefix}{text}"));
        let tag = match mark {
            Mark::Added => Some("diff-add"),
            Mark::Removed => Some("diff-del"),
            Mark::Pad => Some("diff-pad"),
            Mark::Context => None,
        };
        if let Some(tag) = tag {
            let start = buffer.iter_at_offset(start_offset);
            buffer.apply_tag_by_name(tag, &start, &end);
        }
        let strong = match mark {
            Mark::Added => "diff-add-strong",
            Mark::Removed => "diff-del-strong",
            _ => continue,
        };
        let text_start = start_offset + prefix.chars().count() as i32;
        for (from, to) in spans {
            let a = buffer.iter_at_offset(text_start + from as i32);
            let b = buffer.iter_at_offset(text_start + to as i32);
            buffer.apply_tag_by_name(strong, &a, &b);
        }
    }
    crate::chat::suppress_hyphens(buffer.upcast_ref());
    let view = sourceview5::View::builder()
        .buffer(&buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        // A column does not fold: the two are read line against line. The
        // unified block folds, since it is what shows when there is no room.
        .wrap_mode(if prefixed {
            gtk::WrapMode::WordChar
        } else {
            gtk::WrapMode::None
        })
        .top_margin(4)
        .bottom_margin(4)
        .left_margin(SIDE_INSET)
        .right_margin(SIDE_INSET)
        .hexpand(true)
        .css_classes(["diff-side"])
        .build();
    if prefixed {
        // The fold is a HANGING indent: a continuation resumes past the
        // marker column, so a folded line still reads as one line.
        // Measured on realize, because an unrealized widget has no style
        // to measure.
        view.connect_realize(|view| {
            let width = view
                .pango_context()
                .metrics(None, None)
                .approximate_char_width()
                / gtk::pango::SCALE;
            let hang = if width > 0 { width * 2 } else { 0 };
            view.set_left_margin(SIDE_INSET + hang);
            view.set_indent(-hang);
        });
    }
    view
}

/// A diff column's text inset.
const SIDE_INSET: i32 = 8;

/// Decide the layout from the width the column is actually given, and
/// again whenever that changes: side by side when two columns of the
/// longest line fit unwrapped, else the unified block. A zero-height
/// drawing area is the sensor — the one widget that reports its own
/// allocation as a signal.
fn watch_width(
    column: &gtk::Box,
    probe: sourceview5::View,
    longest: usize,
    faces: Vec<(gtk::ScrolledWindow, gtk::Widget)>,
) {
    let sensor = gtk::DrawingArea::builder()
        .hexpand(true)
        .content_height(0)
        .build();
    column.append(&sensor);
    let wide = Rc::new(Cell::new(false));
    sensor.connect_resize(move |_, width, _| {
        let char_width = probe
            .pango_context()
            .metrics(None, None)
            .approximate_char_width()
            / gtk::pango::SCALE;
        let fits = width >= side_by_side_width(longest, char_width.max(1));
        if wide.replace(fits) == fits {
            return;
        }
        // Not in the allocation that asked: a visibility change here is a
        // resize queued from inside a resize.
        let faces = faces.clone();
        glib::idle_add_local_once(move || {
            for (pair, unified) in &faces {
                pair.set_visible(fits);
                unified.set_visible(!fits);
            }
        });
    });
}

/// The width two unwrapped columns of a `longest`-character line need, in
/// a font whose characters are `char_width` wide: the line in each column,
/// each column's inset, and the gap between them. Bounded below so a
/// two-word diff does not go side by side in a column too narrow to read,
/// and above so one long line does not force the stack on a wide pane.
pub fn side_by_side_width(longest: usize, char_width: i32) -> i32 {
    let chars = longest.clamp(24, 120) as i32;
    2 * (chars * char_width + 2 * SIDE_INSET) + 8
}

/// Size `scroller` to the whole of its content once the content has been
/// laid out at the scroller's width. A `GtkTextView` reports the height of
/// its LAST layout — at first, the height at its minimum width, wrapped
/// hard — so a view placed straight in a box keeps a wrong, tall answer.
/// The scroller's adjustment knows the truth once the view is allocated,
/// and the height is asked for from there, on idle, the way the composer
/// sizes itself (chat.rs).
fn fit_to_content(scroller: &gtk::ScrolledWindow) {
    let adjustment = scroller.vadjustment();
    let scroller = scroller.clone();
    let queued = Rc::new(Cell::new(false));
    adjustment.connect_changed(move |adjustment| {
        if queued.replace(true) {
            return;
        }
        let queued = queued.clone();
        let scroller = scroller.clone();
        let adjustment = adjustment.clone();
        glib::idle_add_local_once(move || {
            queued.set(false);
            let content = adjustment.upper().ceil() as i32;
            if content > 0 && scroller.min_content_height() != content {
                scroller.set_min_content_height(content);
            }
        });
    });
}

/// The Adwaita scheme matching the current dark/light preference — the
/// same pairing the editor uses, so a diff in the transcript and the same
/// file in the editor are coloured alike.
pub fn apply_scheme(buffer: &sourceview5::Buffer) {
    let scheme_id = if adw::StyleManager::default().is_dark() {
        "Adwaita-dark"
    } else {
        "Adwaita"
    };
    if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(scheme_id) {
        sourceview5::prelude::BufferExt::set_style_scheme(buffer, Some(&scheme));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_lines_counts_what_it_left_out() {
        assert_eq!(clip_lines("a\nb\nc", 5), ("a\nb\nc".to_string(), 0));
        assert_eq!(clip_lines("a\nb\nc\nd", 2), ("a\nb".to_string(), 2));
    }

    #[test]
    fn a_clipped_response_cuts_between_paragraphs_and_closes_its_fence() {
        // A blank line in the second half of what fits is where the cut
        // lands, and the blank itself is not kept.
        let text = "p1\n\np2\n\np3\n\np4\n\np5";
        let (head, hidden) = clip_prose(text, 5, 10_000);
        assert_eq!(head, "p1\n\np2");
        assert_eq!(hidden, text.lines().count() - 3);
        // No blank to step back to: the cut lands inside the fence, and the
        // fence is closed so the head renders as code.
        let text = "Intro line.\n\nSecond paragraph.\n\n```rust\nfn a() {}\nfn b() {}\nfn c() {}\n```\n\nafter\nmore\nlines\nhere";
        let (head, hidden) = clip_prose(text, 8, 10_000);
        assert!(head.ends_with("fn c() {}\n```"));
        assert_eq!(hidden, text.lines().count() - 8);
        let (head, _) = clip_prose("```\na\nb\nc\nd\ne\nf\ng", 4, 10_000);
        assert!(head.ends_with("\n```"));
        // Short enough: untouched.
        assert_eq!(clip_prose("one\ntwo", 10, 100), ("one\ntwo".to_string(), 0));
    }

    #[test]
    fn a_character_bound_clips_too() {
        let long = "x".repeat(300);
        let text = format!("{long}\n{long}\n{long}\n{long}");
        let (head, hidden) = clip_prose(&text, 100, 650);
        assert_eq!(head.lines().count(), 2);
        assert_eq!(hidden, 2);
    }

    #[test]
    fn the_digest_is_the_last_line_that_says_anything() {
        assert_eq!(
            digest("Compiling\n\u{1b}[1;32mtest result: ok\u{1b}[0m. 3 passed\n\n"),
            Some("test result: ok. 3 passed".to_string())
        );
        assert_eq!(digest("\n  \n"), None);
        assert_eq!(
            digest("done\n<shellId: 1 completed with exit code 0>"),
            Some("done".to_string())
        );
    }

    #[test]
    fn a_replacement_pairs_its_lines_and_pads_the_short_side() {
        let hunks = hunks("a\nb\nc\nd\n", "a\nB\nC2\nC3\nd\n");
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(
            (hunk.old_from, hunk.old_len, hunk.new_from, hunk.new_len),
            (1, 4, 1, 5)
        );
        let marks: Vec<(Mark, Mark)> = hunk.rows.iter().map(|r| (r.left.0, r.right.0)).collect();
        assert_eq!(
            marks,
            vec![
                (Mark::Context, Mark::Context),
                (Mark::Removed, Mark::Added),
                (Mark::Removed, Mark::Added),
                (Mark::Pad, Mark::Added),
                (Mark::Context, Mark::Context),
            ]
        );
        assert_eq!(hunk.rows[3].right.1, "C3");
        assert_eq!(hunk.rows[3].left.1, "");
    }

    #[test]
    fn the_unified_block_puts_every_removed_line_before_the_added_ones() {
        let hunks = hunks("a\nb\nc\nd\n", "a\nB\nC2\nC3\nd\n");
        let unified = hunks[0].unified();
        let marks: Vec<Mark> = unified.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            marks,
            vec![
                Mark::Context,
                Mark::Removed,
                Mark::Removed,
                Mark::Added,
                Mark::Added,
                Mark::Added,
                Mark::Context,
            ]
        );
    }

    #[test]
    fn far_apart_changes_are_separate_hunks() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 2\n", "LINE 2\n")
            .replace("line 28\n", "LINE 28\n");
        let hunks = hunks(&old, &new);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[1].old_from, 25);
    }

    #[test]
    fn a_paired_line_is_emphasized_word_by_word_unless_it_all_changed() {
        let (removed, added) = changed_spans("let x = a + b;", "let x = a - b;");
        assert_eq!(removed, vec![(10, 11)]);
        assert_eq!(added, vec![(10, 11)]);
        // Nothing in common: no emphasis, the line's own wash says it.
        assert_eq!(changed_spans("alpha beta", "gamma delta"), (vec![], vec![]));
    }

    #[test]
    fn the_gutter_numbers_each_side_in_its_own_file_and_skips_pads() {
        let hunks = hunks("a\nb\nc\nd\n", "a\nB\nC2\nC3\nd\n");
        let (left, right) = numbering(&hunks[0]);
        assert_eq!(left, vec![Some(1), Some(2), Some(3), None, Some(4)]);
        assert_eq!(right, vec![Some(1), Some(2), Some(3), Some(4), Some(5)]);
    }

    #[test]
    fn side_by_side_needs_two_columns_of_the_longest_line() {
        // 40 chars at 8px: 2 × (320 + 16) + 8.
        assert_eq!(side_by_side_width(40, 8), 680);
        // Bounded: a tiny diff still wants a readable column, a huge line
        // does not demand the moon.
        assert_eq!(side_by_side_width(3, 8), side_by_side_width(24, 8));
        assert_eq!(side_by_side_width(900, 8), side_by_side_width(120, 8));
    }

    #[test]
    fn ansi_runs_become_markup_and_the_rest_is_escaped() {
        let markup = ansi_markup("a <b> \u{1b}[1;32mok\u{1b}[0m done");
        assert_eq!(
            markup,
            format!(
                "a &lt;b&gt; <span foreground=\"{}\" weight=\"bold\">ok</span> done",
                crate::palette::ANSI_TEXT[2]
            )
        );
    }

    #[test]
    fn a_prompt_or_response_wears_its_role_and_its_moment() {
        let prompt = Document::Text {
            title: "2026-09-07 12:41".into(),
            body: String::new(),
            markdown: false,
            role: TextRole::Prompt,
        };
        assert_eq!(prompt.title(), "2026-09-07 12:41");
        assert_eq!(prompt.icon(), "taste-human-symbolic");
        assert_eq!(prompt.tooltip(), "Prompt sent 2026-09-07 12:41");
        let response = Document::Text {
            title: "2026-09-07 12:42".into(),
            body: String::new(),
            markdown: true,
            role: TextRole::Response,
        };
        assert_eq!(response.icon(), "taste-agent-symbolic");
    }

    #[test]
    fn a_document_names_itself() {
        let doc = Document::Command {
            command: "cargo test -p taste-app filetree".into(),
            output: String::new(),
        };
        assert_eq!(doc.title(), "cargo test -p taste-app filetree");
        assert_eq!(doc.icon(), "utilities-terminal-symbolic");
        let edit = Document::Edit(Edit {
            path: "crates/taste-app/src/filetree.rs".into(),
            old: String::new(),
            new: String::new(),
        });
        assert_eq!(edit.title(), "Edit filetree.rs");
    }
}
