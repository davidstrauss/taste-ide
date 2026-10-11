//! An agent's change to a file's text, as the IDE's own file tools make it.
//!
//! The agent edits through the IDE (`ide_edit_file`, `ide_write_file`) so
//! that a file the user has open takes the change into the buffer they are
//! looking at — their unsaved typing included, one step of their undo
//! history — rather than on the disk behind it (David, 2026-10-05: "The
//! file watcher isn't sufficient to ensure the agent is reading and writing
//! from/to my buffer when a file is open for editing"). The rule for WHAT
//! changes lives here, once, so the editor applying it to a buffer and the
//! files service applying it to a file nobody has open cannot disagree.
//!
//! The rule is an exact replacement, the contract of the edit tools agents
//! already know: `old` must occur exactly once, or every occurrence is
//! replaced when `all` says so. Anything looser would be a guess about
//! which place the agent meant.

/// What a replacement came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// The whole text after the change.
    pub text: String,
    /// How many occurrences were replaced.
    pub count: usize,
    /// The 1-based line the first replacement starts on, in `text`.
    pub first_line: usize,
}

/// `text` with `old` replaced by `new`: the one occurrence, or every one
/// when `all`. The errors are the agent's to act on, so each names what to
/// do next.
pub fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<Replaced, String> {
    if old.is_empty() {
        return Err("old_string is empty; to write a whole file, use ide_write_file".into());
    }
    if old == new {
        return Err("old_string and new_string are the same; nothing to change".into());
    }
    let count = text.matches(old).count();
    match count {
        0 => {
            return Err(
                "old_string was not found. Read the file again with ide_read_file and \
                        copy the text exactly, without the line-number prefix"
                    .into(),
            )
        }
        1 => {}
        n if !all => {
            return Err(format!(
                "old_string occurs {n} times; add surrounding lines to pick one, or pass \
                 replace_all: true to change every one"
            ))
        }
        _ => {}
    }
    let at = text.find(old).unwrap_or(0);
    let replaced = if all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Ok(Replaced {
        first_line: text[..at].matches('\n').count() + 1,
        text: replaced,
        count,
    })
}

/// Lines of `text` numbered from 1, as `cat -n` prints them: from line
/// `offset` (1-based), at most `limit` of them. With how many lines the
/// text has in all, so a caller can say where the rest begins.
pub fn numbered(text: &str, offset: usize, limit: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.max(1) - 1;
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate().skip(start).take(limit) {
        out.push_str(&format!("{:>6}\t{line}\n", index + 1));
    }
    (out, total)
}

/// The most characters one read returns: about ten thousand tokens of
/// prose and fewer than twenty-five thousand of dense JSON, which is the
/// size Claude Code takes as one tool's result. A file with a line longer
/// than that — a record of a hundred thousand characters on one line —
/// is read in parts of that line ([`page`]).
pub const READ_BUDGET: usize = 40_000;

/// One read of a file: its lines numbered as `cat -n` prints them, from
/// line `first` (and from character `column` of it), as many as `limit`
/// and the budget allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub body: String,
    /// How many lines the text has in all.
    pub total: usize,
    /// The first line asked for and the last one shown (below `first`
    /// when nothing was).
    pub first: usize,
    pub last: usize,
    /// The character of `first` the read began at (1 for its start).
    pub column: usize,
    /// The last line shown in part: its number, the last character shown,
    /// and how many it has.
    pub cut: Option<(usize, usize, usize)>,
    /// Where the next read starts, as a line and a character of it; `None`
    /// when this one reached the end, or the end of `limit`.
    pub next: Option<(usize, usize)>,
}

/// From line `offset` (1-based), character `column` of it (1-based), at
/// most `limit` lines and `budget` characters. A later line that does not
/// fit is left for the next read whole; a first line that does not fit by
/// itself is shown as far as it does, and the next read goes on from the
/// character after.
pub fn page(text: &str, offset: usize, column: usize, limit: usize, budget: usize) -> Page {
    let lines: Vec<&str> = text.lines().collect();
    let start = offset.max(1) - 1;
    let column = column.max(1);
    let mut page = Page {
        body: String::new(),
        total: lines.len(),
        first: start + 1,
        last: start,
        column,
        cut: None,
        next: None,
    };
    let mut used = 0;
    for (index, line) in lines.iter().enumerate().skip(start).take(limit.max(1)) {
        let number = index + 1;
        let skipped = if index == start { column - 1 } else { 0 };
        let rest = match line.char_indices().nth(skipped) {
            Some((at, _)) => &line[at..],
            None => "",
        };
        let prefix = format!("{number:>6}\t");
        let length = rest.chars().count();
        let need = prefix.len() + length + 1;
        if used + need <= budget {
            page.body.push_str(&prefix);
            page.body.push_str(rest);
            page.body.push('\n');
            used += need;
            page.last = number;
            continue;
        }
        if index != start {
            page.next = Some((number, 1));
            return page;
        }
        let room = budget.saturating_sub(prefix.len() + 1).max(1);
        let end = rest
            .char_indices()
            .nth(room)
            .map_or(rest.len(), |(at, _)| at);
        page.body.push_str(&prefix);
        page.body.push_str(&rest[..end]);
        page.body.push('\n');
        page.last = number;
        let through = skipped + room;
        page.cut = Some((number, through, skipped + length));
        page.next = Some((number, through + 1));
        return page;
    }
    if page.last >= page.first && page.last < page.total {
        page.next = Some((page.last + 1, 1));
    }
    page
}

impl Page {
    /// The line a read ends with when it is not the whole file: what it
    /// showed, and the `offset` (and `column`) that go on from it. `None`
    /// for a whole file.
    pub fn footer(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        if self.last < self.first {
            return Some(format!(
                "[line {} is past the end: {} lines]\n",
                self.first, self.total
            ));
        }
        if self.next.is_none() && self.first == 1 && self.column == 1 {
            return None;
        }
        let mut parts = vec![format!(
            "lines {}-{} of {}",
            self.first, self.last, self.total
        )];
        if self.column > 1 {
            parts.push(format!(
                "line {} from character {}",
                self.first, self.column
            ));
        }
        if let Some((line, through, of)) = self.cut {
            parts.push(format!("line {line} cut at character {through} of {of}"));
        }
        match self.next {
            Some((line, 1)) => parts.push(format!("pass offset {line} for more")),
            Some((line, column)) => {
                parts.push(format!("pass offset {line}, column {column} for more"))
            }
            None => {}
        }
        Some(format!("[{}]\n", parts.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_occurrence_is_replaced_and_its_line_named() {
        let done = replace("a\nb\nc\n", "b", "B", false).unwrap();
        assert_eq!(done.text, "a\nB\nc\n");
        assert_eq!((done.count, done.first_line), (1, 2));
    }

    #[test]
    fn several_need_all_or_more_context() {
        let err = replace("x x", "x", "y", false).unwrap_err();
        assert!(err.contains("2 times"), "{err}");
        let done = replace("x x", "x", "y", true).unwrap();
        assert_eq!((done.text.as_str(), done.count), ("y y", 2));
    }

    #[test]
    fn nothing_to_find_or_change_is_refused() {
        assert!(replace("abc", "z", "y", false)
            .unwrap_err()
            .contains("not found"));
        assert!(replace("abc", "", "y", false).is_err());
        assert!(replace("abc", "a", "a", false).is_err());
    }

    /// A file of short lines reads whole, or a window of it with the line
    /// that goes on; a budget leaves a later line that does not fit for the
    /// next read, whole.
    #[test]
    fn a_read_pages_by_lines() {
        let text = "one\ntwo\nthree\n";
        let whole = page(text, 1, 1, 2000, READ_BUDGET);
        assert_eq!(whole.body, "     1\tone\n     2\ttwo\n     3\tthree\n");
        assert_eq!(whole.footer(), None);
        let window = page(text, 2, 1, 1, READ_BUDGET);
        assert_eq!(window.body, "     2\ttwo\n");
        assert_eq!(
            window.footer().unwrap(),
            "[lines 2-2 of 3; pass offset 3 for more]\n"
        );
        let tight = page(text, 1, 1, 2000, 22);
        assert_eq!(tight.body, "     1\tone\n     2\ttwo\n");
        assert_eq!(tight.next, Some((3, 1)));
        assert_eq!(
            page(text, 9, 1, 10, READ_BUDGET).footer().unwrap(),
            "[line 9 is past the end: 3 lines]\n"
        );
    }

    /// A line longer than a read is read in parts, each one naming the
    /// column the next begins at, and the parts put back together are the
    /// line — multi-byte characters included.
    #[test]
    fn a_line_longer_than_a_read_is_read_in_parts() {
        let long: String = (0..2500)
            .map(|i| if i % 7 == 0 { 'é' } else { 'x' })
            .collect();
        let text = format!("short\n{long}\nafter\n");
        let first = page(&text, 1, 1, 2000, 1000);
        assert_eq!(first.body, "     1\tshort\n");
        assert_eq!(first.next, Some((2, 1)));
        let (mut line, mut column, mut read) = (2, 1, String::new());
        let mut footers = Vec::new();
        loop {
            let part = page(&text, line, column, 2000, 1000);
            assert!(part.body.chars().count() <= 1000, "{}", part.body.len());
            let shown = part
                .body
                .lines()
                .next()
                .unwrap()
                .split_once('\t')
                .unwrap()
                .1;
            read.push_str(shown);
            footers.push(part.footer().unwrap_or_default());
            match part.next {
                Some((2, c)) => column = c,
                _ => break,
            }
            line = 2;
        }
        assert_eq!(read, long);
        assert_eq!(
            footers[0],
            "[lines 2-2 of 3; line 2 cut at character 992 of 2500; pass offset 2, column 993 \
             for more]\n"
        );
        assert!(footers[1].starts_with("[lines 2-2 of 3; line 2 from character 993; line 2 cut"));
        assert_eq!(
            footers.last().unwrap(),
            "[lines 2-3 of 3; line 2 from character 1985]\n"
        );
    }

    #[test]
    fn numbering_starts_where_asked() {
        let (out, total) = numbered("one\ntwo\nthree\n", 2, 1);
        assert_eq!(out, "     2\ttwo\n");
        assert_eq!(total, 3);
    }
}
