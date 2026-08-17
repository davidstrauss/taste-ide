//! Low-distraction WYSIWYG Markdown.
//!
//! Markdown files are styled *in the editing buffer itself*: headings scale
//! up, emphasis renders as emphasis, code gets monospace on a subtle wash —
//! while the markup characters (`#`, `**`, fences, link URLs) stay present
//! but dimmed toward the background. Dimmed-not-hidden is the deliberate
//! taste call: hiding markers makes the cursor jump and the file lie about
//! itself; dimming keeps editing predictable and the distraction low. The
//! buffer content is always plain markdown — this is styling, not
//! transformation.
//!
//! Not a web engine, on purpose: repo content is untrusted (see the trust
//! model) and raw HTML in documents stays inert, visible, dimmed.
//!
//! The pipeline is split so the interesting half is testable headless:
//! [`style_ranges`] (pure: markdown → char-ranges with tags) →
//! [`apply_styles`] (thin GTK).

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StyleTag {
    Heading(u8),
    Bold,
    Italic,
    Strikethrough,
    Code,
    CodeBlock,
    Quote,
    Link,
    /// Markdown syntax: visible but faded.
    Dim,
}

/// How a construct's style covers its range.
#[derive(Clone, Copy, PartialEq)]
enum Coverage {
    /// Style the whole construct, markers included (headings keep their
    /// scale on the dimmed `#`, code blocks keep their wash on the fences).
    Whole,
    /// Style only the content; markers get nothing but Dim (bold text is
    /// bold, its `**` are not).
    Content,
    /// No style of its own; exists only to dim its non-content parts
    /// (tables' pipes, list items' bullets).
    MarkersOnly,
}

fn classify(tag: &Tag) -> Option<(Option<StyleTag>, Coverage)> {
    Some(match tag {
        Tag::Heading { level, .. } => (
            Some(StyleTag::Heading(heading_number(*level))),
            Coverage::Whole,
        ),
        Tag::CodeBlock(_) => (Some(StyleTag::CodeBlock), Coverage::Whole),
        Tag::BlockQuote(_) => (Some(StyleTag::Quote), Coverage::Whole),
        Tag::Emphasis => (Some(StyleTag::Italic), Coverage::Content),
        Tag::Strong => (Some(StyleTag::Bold), Coverage::Content),
        Tag::Strikethrough => (Some(StyleTag::Strikethrough), Coverage::Content),
        Tag::Link { .. } | Tag::Image { .. } => (Some(StyleTag::Link), Coverage::Content),
        Tag::TableHead => (Some(StyleTag::Bold), Coverage::Content),
        Tag::Item => (None, Coverage::MarkersOnly),
        Tag::Table(_) => (None, Coverage::MarkersOnly),
        _ => return None,
    })
}

fn heading_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

struct OpenConstruct {
    style: Option<StyleTag>,
    coverage: Coverage,
    range: Range<usize>,
    /// Byte ranges within this construct that are real content.
    content: Vec<Range<usize>>,
}

/// Compute style ranges (in **character** offsets, ready for GtkTextBuffer)
/// for a markdown source.
pub fn style_ranges(text: &str) -> Vec<(Range<usize>, StyleTag)> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut out: Vec<(Range<usize>, StyleTag)> = Vec::new();
    let mut stack: Vec<OpenConstruct> = Vec::new();

    let note_content = |stack: &mut Vec<OpenConstruct>, range: &Range<usize>| {
        for open in stack.iter_mut() {
            open.content.push(range.clone());
        }
    };

    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if let Some((style, coverage)) = classify(&tag) {
                    stack.push(OpenConstruct {
                        style,
                        coverage,
                        range,
                        content: Vec::new(),
                    });
                }
            }
            Event::End(end) => {
                if !matches!(
                    end,
                    TagEnd::Heading(_)
                        | TagEnd::CodeBlock
                        | TagEnd::BlockQuote(_)
                        | TagEnd::Emphasis
                        | TagEnd::Strong
                        | TagEnd::Strikethrough
                        | TagEnd::Link
                        | TagEnd::Image
                        | TagEnd::TableHead
                        | TagEnd::Item
                        | TagEnd::Table
                ) {
                    continue;
                }
                let Some(open) = stack.pop() else { continue };
                match open.coverage {
                    Coverage::Whole => {
                        if let Some(style) = open.style {
                            out.push((open.range.clone(), style));
                        }
                    }
                    Coverage::Content => {
                        if let Some(style) = open.style {
                            for content in &open.content {
                                out.push((content.clone(), style));
                            }
                        }
                    }
                    Coverage::MarkersOnly => {}
                }
                for gap in subtract(&open.range, &open.content) {
                    out.push((gap, StyleTag::Dim));
                }
            }
            Event::Text(_) => note_content(&mut stack, &range),
            Event::Code(_) => {
                note_content(&mut stack, &range);
                out.push((range.clone(), StyleTag::Code));
                // Dim the backtick runs themselves.
                let inner = &text[range.clone()];
                let leading = inner.bytes().take_while(|&b| b == b'`').count();
                let trailing = inner.bytes().rev().take_while(|&b| b == b'`').count();
                if leading > 0 && leading + trailing <= inner.len() {
                    out.push((range.start..range.start + leading, StyleTag::Dim));
                    out.push((range.end - trailing..range.end, StyleTag::Dim));
                }
            }
            Event::Rule => out.push((range, StyleTag::Dim)),
            Event::TaskListMarker(_) => {
                // The checkbox is a marker, but a meaningful one: dim it
                // less by tagging it code-ish? No — keep the system simple.
                note_content(&mut stack, &range);
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                // Inert, visible, faded — never interpreted.
                note_content(&mut stack, &range);
                out.push((range, StyleTag::Dim));
            }
            _ => {}
        }
    }

    to_char_ranges(text, out)
}

/// `range` minus the union of `content` ranges.
fn subtract(range: &Range<usize>, content: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut content: Vec<Range<usize>> = content
        .iter()
        .map(|r| r.start.max(range.start)..r.end.min(range.end))
        .filter(|r| r.start < r.end)
        .collect();
    content.sort_by_key(|r| r.start);
    let mut gaps = Vec::new();
    let mut cursor = range.start;
    for r in content {
        if r.start > cursor {
            gaps.push(cursor..r.start);
        }
        cursor = cursor.max(r.end);
    }
    if cursor < range.end {
        gaps.push(cursor..range.end);
    }
    gaps
}

/// Convert byte ranges to character ranges (GtkTextBuffer speaks chars).
fn to_char_ranges(
    text: &str,
    ranges: Vec<(Range<usize>, StyleTag)>,
) -> Vec<(Range<usize>, StyleTag)> {
    let boundaries: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let to_char = |byte: usize| -> usize {
        if byte >= text.len() {
            return boundaries.len();
        }
        boundaries.partition_point(|&b| b < byte)
    };
    ranges
        .into_iter()
        .map(|(r, tag)| (to_char(r.start)..to_char(r.end), tag))
        .filter(|(r, _)| r.start < r.end)
        .collect()
}

pub const ALL_TAG_NAMES: &[&str] = &[
    "md-h1",
    "md-h2",
    "md-h3",
    "md-h4",
    "md-bold",
    "md-italic",
    "md-strike",
    "md-code",
    "md-codeblock",
    "md-quote",
    "md-link",
    "md-dim",
];

fn tag_name(tag: StyleTag) -> &'static str {
    match tag {
        StyleTag::Heading(1) => "md-h1",
        StyleTag::Heading(2) => "md-h2",
        StyleTag::Heading(3) => "md-h3",
        StyleTag::Heading(_) => "md-h4",
        StyleTag::Bold => "md-bold",
        StyleTag::Italic => "md-italic",
        StyleTag::Strikethrough => "md-strike",
        StyleTag::Code => "md-code",
        StyleTag::CodeBlock => "md-codeblock",
        StyleTag::Quote => "md-quote",
        StyleTag::Link => "md-link",
        StyleTag::Dim => "md-dim",
    }
}

/// Restyle a buffer in place: clear our tags, re-apply from a fresh parse.
/// Text is never modified — WYSIWYG here means styling, not transformation.
pub fn apply_styles(buffer: &gtk::TextBuffer, text: &str) {
    use gtk::prelude::*;

    ensure_tags(buffer);
    let (start, end) = buffer.bounds();
    for name in ALL_TAG_NAMES {
        buffer.remove_tag_by_name(name, &start, &end);
    }
    for (range, tag) in style_ranges(text) {
        let start = buffer.iter_at_offset(range.start as i32);
        let end = buffer.iter_at_offset(range.end as i32);
        buffer.apply_tag_by_name(tag_name(tag), &start, &end);
    }
}

fn ensure_tags(buffer: &gtk::TextBuffer) {
    use gtk::prelude::*;
    let table = buffer.tag_table();
    if table.lookup("md-h1").is_some() {
        return;
    }
    let add = |name: &str, f: &dyn Fn(&gtk::TextTag)| {
        let tag = gtk::TextTag::new(Some(name));
        f(&tag);
        table.add(&tag);
    };
    add("md-h1", &|t| {
        t.set_scale(1.7);
        t.set_weight(700);
        t.set_pixels_above_lines(12);
    });
    add("md-h2", &|t| {
        t.set_scale(1.4);
        t.set_weight(700);
        t.set_pixels_above_lines(10);
    });
    add("md-h3", &|t| {
        t.set_scale(1.2);
        t.set_weight(700);
        t.set_pixels_above_lines(8);
    });
    add("md-h4", &|t| {
        t.set_scale(1.05);
        t.set_weight(700);
    });
    add("md-bold", &|t| t.set_weight(700));
    add("md-italic", &|t| t.set_style(gtk::pango::Style::Italic));
    add("md-strike", &|t| t.set_strikethrough(true));
    add("md-code", &|t| {
        t.set_family(Some("monospace"));
        t.set_background_rgba(Some(&gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 0.15)));
    });
    add("md-codeblock", &|t| {
        t.set_family(Some("monospace"));
        t.set_paragraph_background_rgba(Some(&gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 0.12)));
    });
    add("md-quote", &|t| {
        t.set_left_margin(24);
        t.set_style(gtk::pango::Style::Italic);
    });
    add("md-link", &|t| {
        t.set_underline(gtk::pango::Underline::Single);
        t.set_foreground_rgba(Some(&gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0)));
    });
    // The whole point: markup fades, content stands.
    add("md-dim", &|t| {
        t.set_foreground_rgba(Some(&gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 0.55)));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges_of(text: &str, tag: StyleTag) -> Vec<Range<usize>> {
        style_ranges(text)
            .into_iter()
            .filter(|(_, t)| *t == tag)
            .map(|(r, _)| r)
            .collect()
    }

    fn slice(text: &str, range: &Range<usize>) -> String {
        text.chars()
            .skip(range.start)
            .take(range.end - range.start)
            .collect()
    }

    #[test]
    fn heading_styles_whole_line_and_dims_hashes() {
        let text = "## Section\n";
        let headings = ranges_of(text, StyleTag::Heading(2));
        assert_eq!(headings.len(), 1);
        assert_eq!(slice(text, &headings[0]).trim_end(), "## Section");
        let dims = ranges_of(text, StyleTag::Dim);
        assert!(dims.iter().any(|r| slice(text, r).contains("## ")));
        // The word itself is not dimmed.
        assert!(!dims.iter().any(|r| slice(text, r).contains("Section")));
    }

    #[test]
    fn bold_styles_content_only_and_dims_markers() {
        let text = "an **important** word";
        let bolds = ranges_of(text, StyleTag::Bold);
        assert_eq!(bolds.len(), 1);
        assert_eq!(slice(text, &bolds[0]), "important");
        let dims = ranges_of(text, StyleTag::Dim);
        assert_eq!(dims.iter().filter(|r| slice(text, r) == "**").count(), 2);
    }

    #[test]
    fn nested_emphasis_inside_bold() {
        let text = "**bold *and italic***";
        let italics = ranges_of(text, StyleTag::Italic);
        assert_eq!(italics.len(), 1);
        assert_eq!(slice(text, &italics[0]), "and italic");
    }

    #[test]
    fn link_text_is_linked_and_url_is_dimmed() {
        let text = "see [docs](https://example.com) now";
        let links = ranges_of(text, StyleTag::Link);
        assert_eq!(links.len(), 1);
        assert_eq!(slice(text, &links[0]), "docs");
        let dims = ranges_of(text, StyleTag::Dim);
        assert!(dims
            .iter()
            .any(|r| slice(text, r).contains("https://example.com")));
    }

    #[test]
    fn list_markers_are_dimmed_content_is_not() {
        let text = "- alpha\n- beta\n";
        let dims = ranges_of(text, StyleTag::Dim);
        assert!(dims.iter().any(|r| slice(text, r).contains("- ")));
        assert!(!dims.iter().any(|r| slice(text, r).contains("alpha")));
    }

    #[test]
    fn fenced_code_block_covers_whole_and_dims_fences() {
        let text = "```\nlet x = 1;\n```\n";
        let blocks = ranges_of(text, StyleTag::CodeBlock);
        assert_eq!(blocks.len(), 1);
        assert!(slice(text, &blocks[0]).contains("let x = 1;"));
        let dims = ranges_of(text, StyleTag::Dim);
        assert!(dims.iter().any(|r| slice(text, r).contains("```")));
        assert!(!dims.iter().any(|r| slice(text, r).contains("let x")));
    }

    #[test]
    fn inline_code_dims_backticks() {
        let text = "run `cargo test` here";
        let codes = ranges_of(text, StyleTag::Code);
        assert_eq!(slice(text, &codes[0]), "`cargo test`");
        let dims = ranges_of(text, StyleTag::Dim);
        assert_eq!(dims.iter().filter(|r| slice(text, r) == "`").count(), 2);
    }

    #[test]
    fn html_is_dimmed_inert_text() {
        let text = "before <b>markup</b> after";
        let dims = ranges_of(text, StyleTag::Dim);
        assert!(dims.iter().any(|r| slice(text, r) == "<b>"));
    }

    #[test]
    fn unicode_offsets_are_character_based() {
        let text = "héllo **wörld**";
        let bolds = ranges_of(text, StyleTag::Bold);
        assert_eq!(slice(text, &bolds[0]), "wörld");
    }

    #[test]
    fn empty_and_plain_text_produce_no_styles() {
        assert!(style_ranges("").is_empty());
        assert!(style_ranges("just a plain paragraph\n").is_empty());
    }

    /// Profiling harness (run on demand):
    /// `cargo test -p taste-app perf_ -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_style_ranges_on_large_document() {
        let chunk = "## Heading\n\nSome **bold** and *italic* text with `code` and \
                     [links](https://example.com).\n\n```rust\nlet x = 1;\n```\n\n- item one\n- item two\n\n";
        for kib in [64, 256, 512] {
            let doc = chunk.repeat(kib * 1024 / chunk.len());
            let start = std::time::Instant::now();
            let ranges = style_ranges(&doc);
            println!(
                "style_ranges: {:>4} KiB → {:>5} ranges in {:>6.1?}",
                doc.len() / 1024,
                ranges.len(),
                start.elapsed()
            );
        }
    }
}
