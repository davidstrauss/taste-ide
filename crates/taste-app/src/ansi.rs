//! ANSI colour, read: a log line's SGR escapes — `ESC[…m`, the colours
//! and weights cargo, podman, and systemd write for a terminal — turned
//! into styled spans for a text buffer, and every other escape dropped
//! (David, 2026-09-21: "Logs should be in color").
//!
//! The palette is chosen to read on both schemes rather than to match any
//! terminal's: a log is text on the editor's background, and the reader
//! has not picked a theme for it.

/// One run of text and how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// A name for a text tag carrying exactly this style.
    pub fn tag_name(&self) -> String {
        let fg = match self.fg {
            None => "-".to_string(),
            Some(Color::Index(i)) => format!("i{i}"),
            Some(Color::Rgb(r, g, b)) => format!("{r:02x}{g:02x}{b:02x}"),
        };
        format!(
            "ansi:{fg}:{}{}{}{}",
            if self.bold { "b" } else { "" },
            if self.dim { "d" } else { "" },
            if self.italic { "i" } else { "" },
            if self.underline { "u" } else { "" }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// One of the 256 indexed colours; 0–15 are the named ones.
    Index(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// The colour as it is drawn, `(r, g, b)` in 0–255. The sixteen named
    /// colours are picked to read on a dark and a light background alike;
    /// the cube and the greys are the standard ones.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Index(i) => match i {
                0 => (0x77, 0x76, 0x7b),
                1 => (0xe0, 0x1b, 0x24),
                2 => (0x26, 0xa2, 0x69),
                3 => (0xc8, 0x88, 0x00),
                4 => (0x35, 0x84, 0xe4),
                5 => (0x91, 0x41, 0xac),
                6 => (0x0e, 0x8a, 0x9c),
                7 => (0x9a, 0x99, 0x96),
                8 => (0x8b, 0x8e, 0x8f),
                9 => (0xf6, 0x61, 0x51),
                10 => (0x33, 0xd1, 0x7a),
                11 => (0xf5, 0xc2, 0x11),
                12 => (0x62, 0xa0, 0xea),
                13 => (0xdc, 0x8a, 0xdd),
                14 => (0x33, 0xc7, 0xde),
                15 => (0xc0, 0xbf, 0xbc),
                16..=231 => {
                    let n = i - 16;
                    let level = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
                    (level(n / 36), level((n / 6) % 6), level(n % 6))
                }
                _ => {
                    let g = 8 + 10 * (i - 232);
                    (g, g, g)
                }
            },
        }
    }
}

/// The line as styled spans, escapes consumed. A line with no escapes is
/// one plain span; an escape that is not SGR (a cursor move, a title) is
/// dropped without a trace.
pub fn spans(line: &str) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = line.chars().peekable();
    let flush = |text: &mut String, style: &Style, out: &mut Vec<(String, Style)>| {
        if !text.is_empty() {
            out.push((std::mem::take(text), style.clone()));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = None;
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            final_byte = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if final_byte == Some('m') {
                        flush(&mut text, &style, &mut out);
                        apply_sgr(&mut style, &params);
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' => {}
            c if c.is_control() && c != '\t' => {}
            c => text.push(c),
        }
    }
    flush(&mut text, &style, &mut out);
    out
}

/// The line with every escape removed.
#[cfg(test)]
pub fn strip(line: &str) -> String {
    spans(line).into_iter().map(|(text, _)| text).collect()
}

fn apply_sgr(style: &mut Style, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(Color::Index((codes[i] - 30) as u8)),
            90..=97 => style.fg = Some(Color::Index((codes[i] - 90 + 8) as u8)),
            39 => style.fg = None,
            38 => match codes.get(i + 1) {
                Some(5) => {
                    if let Some(&n) = codes.get(i + 2) {
                        style.fg = Some(Color::Index(n.min(255) as u8));
                    }
                    i += 2;
                }
                Some(2) => {
                    if let (Some(&r), Some(&g), Some(&b)) =
                        (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                    {
                        style.fg = Some(Color::Rgb(
                            r.min(255) as u8,
                            g.min(255) as u8,
                            b.min(255) as u8,
                        ));
                    }
                    i += 4;
                }
                _ => {}
            },
            // Backgrounds are read and dropped: a log's background is the
            // page's.
            40..=47 | 100..=107 | 49 => {}
            48 => match codes.get(i + 1) {
                Some(5) => i += 2,
                Some(2) => i += 4,
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
}

/// The severity word a tracing or systemd line leads with, and the
/// colour it earns: ERROR red, WARN yellow, INFO none, DEBUG and TRACE
/// dim. `Some((start, end, style))` names the word's byte range.
pub fn level_span(line: &str) -> Option<(usize, usize, Style)> {
    // The first eighty bytes, cut at a character boundary: a line whose
    // eightieth byte is inside an arrow panicked the main thread (David,
    // 2026-09-22: "end byte index 80 is not a char boundary; it is inside
    // '→'").
    let mut end = line.len().min(80);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let head = &line[..end];
    for (word, style) in [
        (
            "ERROR",
            Style {
                fg: Some(Color::Index(9)),
                bold: true,
                ..Style::default()
            },
        ),
        (
            "WARN",
            Style {
                fg: Some(Color::Index(11)),
                ..Style::default()
            },
        ),
        (
            "DEBUG",
            Style {
                dim: true,
                ..Style::default()
            },
        ),
        (
            "TRACE",
            Style {
                dim: true,
                ..Style::default()
            },
        ),
    ] {
        if let Some(at) = head.find(word) {
            let before_ok = at == 0 || !head.as_bytes()[at - 1].is_ascii_alphanumeric();
            let after = at + word.len();
            let after_ok = after >= head.len() || !head.as_bytes()[after].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return Some((at, after, style));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_escapes_become_styled_spans_and_other_escapes_vanish() {
        let line =
            "\u{1b}[1;32m   Compiling\u{1b}[0m taste-core v0.1.0 \u{1b}[2K\u{1b}]0;title\u{7}(lib)";
        let spans = spans(line);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[0].0, "   Compiling");
        assert!(spans[0].1.bold);
        assert_eq!(spans[0].1.fg, Some(Color::Index(2)));
        assert_eq!(spans[1].0, " taste-core v0.1.0 (lib)");
        assert!(spans[1].1.is_plain());
        assert_eq!(strip(line), "   Compiling taste-core v0.1.0 (lib)");
    }

    #[test]
    fn extended_colours_are_read_and_backgrounds_dropped() {
        let spans = spans("\u{1b}[38;5;208;48;5;17mx\u{1b}[38;2;10;20;30my");
        assert_eq!(spans[0].1.fg, Some(Color::Index(208)));
        assert_eq!(spans[1].1.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(Color::Index(196).rgb(), (255, 0, 0));
        assert_eq!(Color::Index(232).rgb(), (8, 8, 8));
    }

    #[test]
    fn a_multibyte_character_at_the_cut_does_not_panic() {
        // Seventy-eight ASCII bytes, then a three-byte arrow straddling
        // byte eighty.
        let line = format!("{}→ WARN after the arrow", "x".repeat(78));
        assert!(level_span(&line).is_none());
        let line = format!("WARN {}→ tail", "y".repeat(73));
        assert_eq!(level_span(&line).map(|(s, e, _)| (s, e)), Some((0, 4)));
    }

    #[test]
    fn a_tracing_line_leads_with_its_level() {
        let line =
            "2026-09-21T22:29:53.409945Z  WARN taste_ide::filetree: file tree: status failed";
        let (start, end, style) = level_span(line).unwrap();
        assert_eq!(&line[start..end], "WARN");
        assert_eq!(style.fg, Some(Color::Index(11)));
        assert!(level_span("nothing to see").is_none());
        assert!(level_span("INFORMATION").is_none());
    }
}
