//! MDX: Markdown with JSX in it — `import` and `export` statements, `{…}`
//! expressions, and components written as tags (David, 2026-11-06: "MDX
//! files need to work, too"). An `.mdx` file is edited as Markdown and
//! previewed as Markdown, through the same preview a `.md` file has.
//!
//! What only MDX's compiler could make of it is taken out of the text the
//! preview draws, never run: the ESM blocks, the expressions, and the
//! lines that are nothing but component tags. Running it would mean
//! evaluating a repository's JavaScript on the host, which the trust model
//! forbids, so a component's own drawing is not shown — the Markdown
//! between its tags is, as MDX itself treats it as Markdown. A tag in the
//! middle of a line is left to the Markdown parser, which treats it as
//! inline HTML: the preview leaves it out, and the words around it and
//! inside it stay, as an `.md` file's raw HTML does.
//!
//! [`prose`] is pure and line-preserving — what it takes out leaves the
//! line breaks behind — so a blank line still ends a paragraph where the
//! source has one, and a component's opening tag on a line of its own
//! separates what follows from what came before, as MDX parses it.

use std::path::Path;

/// Whether `path` is an MDX document.
pub fn is_mdx(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mdx"))
}

/// The Markdown of an MDX document: `text` without its front matter, its
/// ESM blocks, its `{…}` expressions on lines of their own and its
/// comments anywhere, and its lines of nothing but component tags. Fenced
/// code is left whole: a fence's import is an example, not one.
pub fn prose(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut keep = vec![true; chars.len()];
    let mut fence: Option<(char, usize)> = None;
    let mut esm = false;
    let mut at = front_matter(&chars).unwrap_or(0);
    drop_span(&mut keep, &chars, 0, at);
    while at < chars.len() {
        let end = line_end(&chars, at);
        let line: String = chars[at..end].iter().collect();
        let trimmed = line.trim_start();
        let indent = line.chars().count() - trimmed.chars().count();
        let next = (end + 1).min(chars.len());
        if let Some((mark, length)) = fence {
            if fence_of(trimmed).is_some_and(|(m, l)| m == mark && l >= length)
                && trimmed.trim_start_matches(mark).trim().is_empty()
            {
                fence = None;
            }
            at = next;
            continue;
        }
        if esm {
            // An ESM block runs to the next blank line.
            if line.trim().is_empty() {
                esm = false;
            } else {
                drop_span(&mut keep, &chars, at, end);
            }
            at = next;
            continue;
        }
        if let Some(open) = fence_of(trimmed) {
            fence = Some(open);
            at = next;
            continue;
        }
        if indent == 0 && is_esm(&line) {
            esm = true;
            drop_span(&mut keep, &chars, at, end);
            at = next;
            continue;
        }
        if let Some(stop) = flow(&chars, at + indent) {
            // Tags and expressions to the end of a line, which may be a
            // later one than this: an opening tag's attributes can run
            // over several.
            let stop_end = line_end(&chars, stop);
            drop_span(&mut keep, &chars, at, stop_end);
            at = (stop_end + 1).min(chars.len());
            continue;
        }
        drop_comments(&mut keep, &chars, at, end);
        at = next;
    }
    chars
        .into_iter()
        .zip(keep)
        .filter_map(|(c, kept)| kept.then_some(c))
        .collect()
}

/// Past the YAML front matter the document opens with — a `---` line,
/// then everything to the next `---` or `...` line — which nearly every
/// MDX site's toolchain reads and the preview would otherwise draw as a
/// rule over a heading.
fn front_matter(chars: &[char]) -> Option<usize> {
    let line = |at: usize| -> String { chars[at..line_end(chars, at)].iter().collect() };
    if line(0).trim_end() != "---" {
        return None;
    }
    let mut at = line_end(chars, 0) + 1;
    while at < chars.len() {
        let end = line_end(chars, at);
        if matches!(line(at).trim_end(), "---" | "...") {
            return Some(end);
        }
        at = end + 1;
    }
    None
}

/// Where the line starting at `at` ends: its newline, or the end.
fn line_end(chars: &[char], at: usize) -> usize {
    chars[at..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |n| at + n)
}

/// Take out `chars[from..to]`, its line breaks left in.
fn drop_span(keep: &mut [bool], chars: &[char], from: usize, to: usize) {
    for i in from..to {
        if chars[i] != '\n' {
            keep[i] = false;
        }
    }
}

/// A code fence a line opens or closes with: its character and length.
pub(crate) fn fence_of(trimmed: &str) -> Option<(char, usize)> {
    let mark = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = trimmed.chars().take_while(|c| *c == mark).count();
    (length >= 3).then_some((mark, length))
}

/// An ESM statement's first line: `import` or `export` as a statement,
/// which MDX takes at the start of a line, unindented.
fn is_esm(line: &str) -> bool {
    ["import", "export"].iter().any(|word| {
        line.strip_prefix(word).is_some_and(|rest| {
            rest.starts_with(|c: char| c.is_whitespace() || matches!(c, '{' | '*' | '"' | '\''))
        })
    })
}

/// Tags and `{…}` expressions from `at` to the end of a line and nothing
/// else: where they end, or `None` when the line has anything besides
/// them, or none of them.
fn flow(chars: &[char], at: usize) -> Option<usize> {
    let mut i = at;
    let mut any = false;
    loop {
        while i < chars.len() && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        match chars.get(i) {
            None | Some('\n') => return any.then_some(i),
            Some('{') => i = after_braces(chars, i)?,
            Some('<') if tag_starts(chars.get(i + 1).copied()) => i = after_tag(chars, i)?,
            Some(_) => return None,
        }
        any = true;
    }
}

/// What may follow a `<` for it to open a tag: a name, a closing slash,
/// or a fragment's `>`.
fn tag_starts(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphabetic() || matches!(c, '/' | '>' | '_' | '$'))
}

/// Past the `>` that ends the tag at `at`, quotes and `{…}` attribute
/// values stepped over.
fn after_tag(chars: &[char], at: usize) -> Option<usize> {
    let mut i = at + 1;
    let mut quote: Option<char> = None;
    while let Some(&c) = chars.get(i) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => {
                i = after_braces(chars, i)?;
                continue;
            }
            (None, '>') => return Some(i + 1),
            (None, _) => {}
        }
        i += 1;
    }
    None
}

/// Past the `}` that closes the `{` at `at`, strings, comments, and
/// nested braces stepped over — a comment's apostrophe is no quote.
fn after_braces(chars: &[char], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = at;
    while let Some(&c) = chars.get(i) {
        match (quote, c) {
            // An escape inside a string: the next character is its own.
            (Some(_), '\\') => {
                i += 2;
                continue;
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '/') if chars.get(i + 1) == Some(&'*') => {
                let close = (i + 2..chars.len().saturating_sub(1))
                    .find(|&j| chars[j] == '*' && chars[j + 1] == '/')?;
                i = close + 2;
                continue;
            }
            (None, '/') if chars.get(i + 1) == Some(&'/') => {
                i = line_end(chars, i);
                continue;
            }
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            (None, _) => {}
        }
        i += 1;
    }
    None
}

/// Take out the `{/* … */}` comments in the line `chars[from..to]`.
fn drop_comments(keep: &mut [bool], chars: &[char], from: usize, to: usize) {
    let mut i = from;
    while i + 3 < to {
        if chars[i] == '{' && chars[i + 1] == '/' && chars[i + 2] == '*' {
            if let Some(end) = after_braces(chars, i).filter(|&end| end <= to) {
                let inner: String = chars[i + 1..end - 1].iter().collect();
                if inner.trim_end().ends_with("*/") {
                    drop_span(keep, chars, i, end);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_mdx_file_is_told_by_its_extension() {
        assert!(is_mdx(Path::new("docs/intro.mdx")));
        assert!(is_mdx(Path::new("README.MDX")));
        assert!(!is_mdx(Path::new("README.md")));
    }

    #[test]
    fn esm_blocks_go_and_their_lines_stay() {
        let text = "import { Chart } from './chart'\nimport data from './data.json'\n\nexport const meta = {\n  title: 'x',\n}\n\n# Title\n";
        assert_eq!(prose(text), "\n\n\n\n\n\n\n# Title\n");
    }

    #[test]
    fn a_component_s_markdown_stays_and_its_tags_go() {
        let text = "Before.\n<Callout type=\"warn\">\n**Careful** here.\n</Callout>\nAfter.\n";
        assert_eq!(prose(text), "Before.\n\n**Careful** here.\n\nAfter.\n");
    }

    #[test]
    fn a_tag_whose_attributes_run_over_lines_goes_whole() {
        let text = "<Chart\n  data={[1, 2, 3]}\n  title=\"a > b\"\n/>\n\nText.\n";
        assert_eq!(prose(text), "\n\n\n\n\nText.\n");
    }

    #[test]
    fn expressions_and_comments_go() {
        let text = "{/* don't show this */}\n{new Date().getFullYear()}\nSee {/* inline */}this.\n";
        assert_eq!(prose(text), "\n\nSee this.\n");
    }

    #[test]
    fn front_matter_goes() {
        let text = "---\ntitle: Start\n---\n\n# Start\n";
        assert_eq!(prose(text), "\n\n\n\n# Start\n");
        // A rule later on is a rule.
        assert_eq!(prose("Text\n\n---\n"), "Text\n\n---\n");
        // An unclosed one is not front matter.
        assert_eq!(prose("---\nnot closed\n"), "---\nnot closed\n");
    }

    #[test]
    fn a_tag_inside_a_line_is_left_to_the_markdown_parser() {
        let text = "Press <Kbd>Ctrl</Kbd> to go.\n";
        assert_eq!(prose(text), text);
    }

    #[test]
    fn fenced_code_is_left_whole() {
        let text = "```js\nimport x from 'y'\n<App />\n```\n";
        assert_eq!(prose(text), text);
    }

    #[test]
    fn prose_that_only_looks_like_mdx_stays() {
        let text = "importing is easy\n1 < 2 and 3 > 2\n";
        assert_eq!(prose(text), text);
    }
}
