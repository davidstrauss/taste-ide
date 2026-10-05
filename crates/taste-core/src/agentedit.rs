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

    #[test]
    fn numbering_starts_where_asked() {
        let (out, total) = numbered("one\ntwo\nthree\n", 2, 1);
        assert_eq!(out, "     2\ttwo\n");
        assert_eq!(total, 3);
    }
}
