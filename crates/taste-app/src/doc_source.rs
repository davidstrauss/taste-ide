//! Where a Markdown document's source lines landed in its preview: what a
//! line asked of a previewed document — a search result's, a link's
//! `#L12` — is turned into a place in the preview's buffer with. Pure, no
//! GTK: the preview's writer (`crate::doc_text`) records a point as it
//! writes each block, and each line of a block's text where it is cheap
//! to know, and this answers from those points.
//!
//! The other way, a place in the preview is a source line, and a line is
//! followed through an edit of the source ([`follow_line`]): how the
//! preview keeps the reader's place when the document is presented again.
//!
//! A point is a source line and a character offset in the rendering's
//! buffer. The buffer is only ever written at its end, so an offset taken
//! while it is written stays where it was for the life of the rendering;
//! a re-rendering records its own.

use pulldown_cmark::{Event, Tag};

/// Where each line of a text starts, by byte: a byte offset's line.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        Self { starts }
    }

    /// The 0-based line `byte` is on.
    pub fn line_of(&self, byte: usize) -> u32 {
        (self.starts.partition_point(|start| *start <= byte) - 1) as u32
    }
}

/// Whether `event` begins a block the preview draws: a point is recorded
/// for it at the place its first line, or its widget, lands.
pub fn opens_block(event: &Event) -> bool {
    matches!(
        event,
        Event::Start(
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::CodeBlock(_)
                | Tag::List(_)
                | Tag::Item
                | Tag::BlockQuote(_)
                | Tag::Table(_)
        ) | Event::Rule
    )
}

/// The points of one rendering, and the place of a source line among them.
#[derive(Default)]
pub struct SourceMap {
    /// (0-based source line, buffer offset), in that order.
    points: Vec<(u32, i32)>,
    /// The same points as (buffer offset, source line), in that order: a
    /// place's line.
    by_offset: Vec<(i32, u32)>,
}

impl SourceMap {
    pub fn new(mut points: Vec<(u32, i32)>) -> Self {
        points.sort_unstable();
        points.dedup_by_key(|point| point.0);
        let mut by_offset: Vec<(i32, u32)> = points
            .iter()
            .map(|&(line, offset)| (offset, line))
            .collect();
        by_offset.sort_unstable();
        Self { points, by_offset }
    }

    /// The source line whose place is at or before buffer `offset`, and
    /// that place: what a reader looking at `offset` is reading, as a line
    /// of the source. The last line written there when several were (a
    /// block that wrote nothing before the next), since each has the same
    /// place. None before the first point.
    pub fn line_at(&self, offset: i32) -> Option<(u32, i32)> {
        let after = self.by_offset.partition_point(|(at, _)| *at <= offset);
        let (at, line) = *self.by_offset.get(after.checked_sub(1)?)?;
        Some((line, at))
    }

    /// Where 0-based source `line` is in the buffer: the first place
    /// recorded for the last line at or before it that has one — a line
    /// of a paragraph or a code block its own, a line of a table or a
    /// blank line between blocks the block it is in or follows. The
    /// document's start for a line before anything.
    pub fn place(&self, line: u32) -> i32 {
        let after = self.points.partition_point(|(at, _)| *at <= line);
        after.checked_sub(1).map_or(0, |index| self.points[index].1)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.points.len()
    }
}

/// Where a line of a source went when the source changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The 0-based line it is in the new source: itself, where it was
    /// kept; the same line of what replaced it, or that replacement's
    /// last, where it was changed; or what follows the place it was, where
    /// it was deleted outright.
    pub line: u32,
    pub fate: Fate,
}

/// What became of a line when its source changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// It is in the new source as it was.
    Kept,
    /// Something else is in its place.
    Changed,
    /// It was deleted, with nothing in its place.
    Gone,
}

/// How long a line's way through an edit is looked for before the diff
/// settles for a coarser answer: well inside a frame. A document is
/// presented again on each edit, debounced, and on each reload.
const FOLLOW_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(4);

/// Where 0-based `line` of `old` is in `new`, by a diff of their lines:
/// a line in a stretch both texts have is that line of the new; one in a
/// stretch replaced is the same line of the replacement, or its last when
/// the replacement is shorter; and one deleted with nothing in its place
/// is the line that now stands where it was.
///
/// The lines both texts begin with, and those they end with, are found by
/// comparing their bytes first, and only what lies between is diffed: an
/// edit as it is typed leaves all but a line or two of a document alone.
pub fn follow_line(old: &str, new: &str, line: u32) -> Moved {
    let kept = Moved {
        line,
        fate: Fate::Kept,
    };
    if old == new {
        return kept;
    }
    let (head, tail) = common_lines(old, new);
    let head_lines = old[..head].matches('\n').count() as u32;
    if line < head_lines {
        return kept;
    }
    let old_middle = &old[head..old.len() - tail];
    let new_middle = &new[head..new.len() - tail];
    // What follows a middle that does not run to the end starts a line.
    let old_lines = old_middle.matches('\n').count() as u32;
    let new_lines = new_middle.matches('\n').count() as u32;
    if tail > 0 && line >= head_lines + old_lines {
        return Moved {
            line: line - old_lines + new_lines,
            fate: Fate::Kept,
        };
    }
    let moved = follow_through_diff(old_middle, new_middle, line - head_lines);
    Moved {
        line: moved.line + head_lines,
        ..moved
    }
}

/// How many bytes `old` and `new` begin with alike, and end with alike,
/// each a run of whole lines and the two not overlapping.
fn common_lines(old: &str, new: &str) -> (usize, usize) {
    let same = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let head = old[..same].rfind('\n').map_or(0, |at| at + 1);
    let (old_rest, new_rest) = (&old.as_bytes()[head..], &new.as_bytes()[head..]);
    let same = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old_rest.len())
        .min(new_rest.len());
    // From the first line that starts inside the run alike: the line it
    // begins in is one the two differ in.
    let from = old_rest.len() - same;
    let tail = old_rest[from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(0, |at| same - at - 1);
    (head, tail)
}

/// [`follow_line`] for what lies between the lines alike: a diff.
fn follow_through_diff(old: &str, new: &str, line: u32) -> Moved {
    let diff = similar::TextDiff::configure()
        .timeout(FOLLOW_TIMEOUT)
        .diff_lines(old, new);
    let at = line as usize;
    let moved = |line: usize, fate: Fate| Moved {
        line: line as u32,
        fate,
    };
    for op in diff.ops() {
        let (tag, was, now) = op.as_tag_tuple();
        if !was.contains(&at) {
            continue;
        }
        let into = at - was.start;
        return match tag {
            similar::DiffTag::Equal => moved(now.start + into, Fate::Kept),
            similar::DiffTag::Delete => moved(now.start, Fate::Gone),
            similar::DiffTag::Replace => moved(
                now.start + into.min(now.len().saturating_sub(1)),
                Fate::Changed,
            ),
            // An insertion replaces no old line, so none is in it.
            similar::DiffTag::Insert => moved(now.start, Fate::Changed),
        };
    }
    // Past the old text's last line, by as many past the new one's.
    let (old_end, new_end) = diff
        .ops()
        .last()
        .map_or((0, 0), |op| (op.old_range().end, op.new_range().end));
    moved(new_end + at.saturating_sub(old_end), Fate::Kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    const DOC: &str = "# Title\n\
        \n\
        A paragraph that runs\n\
        over three lines\n\
        of its source.\n\
        \n\
        - one\n\
        - two,\n\
        \x20 continued\n\
        \n\
        ```rust\n\
        fn main() {\n\
        \x20   run();\n\
        }\n\
        ```\n\
        \n\
        | a | b |\n\
        |---|---|\n\
        | 1 | 2 |\n\
        | 3 | 4 |\n\
        \n\
        ---\n\
        \n\
        The end.\n";

    /// The blocks' points as the writer records them, each block's place
    /// standing in as the index of the event that opened it.
    fn blocks(text: &str) -> SourceMap {
        let lines = LineIndex::new(text);
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        let points = Parser::new_ext(text, options)
            .into_offset_iter()
            .enumerate()
            .filter(|(_, (event, _))| opens_block(event))
            .map(|(index, (_, range))| (lines.line_of(range.start), index as i32))
            .collect();
        SourceMap::new(points)
    }

    #[test]
    fn a_byte_is_on_its_line() {
        let lines = LineIndex::new("ab\ncd\n\nef");
        assert_eq!(lines.line_of(0), 0);
        assert_eq!(lines.line_of(2), 0, "a line's newline is on it");
        assert_eq!(lines.line_of(3), 1);
        assert_eq!(lines.line_of(6), 2);
        assert_eq!(lines.line_of(7), 3);
        assert_eq!(lines.line_of(99), 3);
    }

    /// Each kind of block is found from any of its lines: a paragraph's
    /// later lines, an item's continuation, a code block's lines, and a
    /// table's rows are the block they are in, and a blank line the block
    /// before it.
    #[test]
    fn a_line_is_the_block_it_is_in() {
        let map = blocks(DOC);
        let at = |line: u32| map.place(line - 1);
        let heading = at(1);
        let paragraph = at(3);
        assert!(paragraph > heading);
        assert_eq!(at(4), paragraph);
        assert_eq!(at(5), paragraph);
        assert_eq!(at(6), paragraph, "a blank line is the block before it");
        // The list and its first item open on one line: the first point.
        let one = at(7);
        assert!(one > paragraph);
        let two = at(8);
        assert!(two > one);
        assert_eq!(at(9), two, "an item's continuation is the item");
        let code = at(11);
        assert!(code > two);
        for line in 12..=15 {
            assert_eq!(at(line), code, "line {line} is the code block's");
        }
        let table = at(17);
        assert!(table > code);
        for line in 18..=20 {
            assert_eq!(at(line), table, "line {line} is the table's");
        }
        let rule = at(22);
        assert!(rule > table);
        assert!(at(24) > rule);
        assert_eq!(at(1000), at(24), "past the end is the last block");
    }

    #[test]
    fn a_line_before_anything_is_the_start() {
        let map = blocks("\n\n\nText.\n");
        assert_eq!(map.place(0), 0);
        assert_eq!(SourceMap::default().place(7), 0);
    }

    /// A place is the line written last at or before it: a paragraph's
    /// later lines, a code block's lines, and a table from anywhere in it;
    /// nothing before the first point.
    #[test]
    fn a_place_is_the_line_written_there() {
        let map = SourceMap::new(vec![(2, 10), (4, 30), (5, 30), (3, 18), (9, 60)]);
        assert_eq!(map.line_at(9), None);
        assert_eq!(map.line_at(10), Some((2, 10)));
        assert_eq!(map.line_at(17), Some((2, 10)));
        assert_eq!(map.line_at(18), Some((3, 18)));
        assert_eq!(
            map.line_at(30),
            Some((5, 30)),
            "the last line written there"
        );
        assert_eq!(map.line_at(59), Some((5, 30)));
        assert_eq!(map.line_at(1000), Some((9, 60)));
        assert_eq!(SourceMap::default().line_at(4), None);
        // Each line found from its place lands back on that place.
        let doc = blocks(DOC);
        for offset in 0..200 {
            if let Some((line, at)) = doc.line_at(offset) {
                assert_eq!(doc.place(line), at, "offset {offset}");
                assert!(at <= offset);
            }
        }
    }

    fn numbered(lines: std::ops::Range<u32>) -> String {
        lines.map(|n| format!("line {n}\n")).collect()
    }

    /// A line in a stretch both texts have is that line of the new:
    /// moved down by lines inserted above it, up by lines deleted above
    /// it, and left alone by an edit below it.
    #[test]
    fn a_kept_line_follows_the_edit_above_it() {
        let old = numbered(0..100);
        let inserted = format!(
            "{}new a\nnew b\nnew c\n{}",
            numbered(0..10),
            numbered(10..100)
        );
        let deleted = format!("{}{}", numbered(0..10), numbered(15..100));
        let below = format!("{}changed\n{}", numbered(0..80), numbered(81..100));
        let kept = |line| Moved {
            line,
            fate: Fate::Kept,
        };
        assert_eq!(follow_line(&old, &inserted, 50), kept(53));
        assert_eq!(follow_line(&old, &inserted, 9), kept(9));
        assert_eq!(follow_line(&old, &deleted, 50), kept(45));
        assert_eq!(follow_line(&old, &below, 50), kept(50));
        assert_eq!(follow_line(&old, &old, 50), kept(50));
    }

    /// A line changed is the same line of what replaced it, or its last
    /// when that is shorter; a line deleted outright is what now stands
    /// where it was.
    #[test]
    fn a_changed_line_lands_on_its_replacement() {
        let old = numbered(0..20);
        let edited = format!("{}line 5, edited\n{}", numbered(0..5), numbered(6..20));
        assert_eq!(
            follow_line(&old, &edited, 5),
            Moved {
                line: 5,
                fate: Fate::Changed
            }
        );
        let shorter = format!("{}one\ntwo\n{}", numbered(0..5), numbered(10..20));
        assert_eq!(
            follow_line(&old, &shorter, 6),
            Moved {
                line: 6,
                fate: Fate::Changed
            }
        );
        assert_eq!(
            follow_line(&old, &shorter, 9),
            Moved {
                line: 6,
                fate: Fate::Changed
            }
        );
        assert_eq!(
            follow_line(&old, &shorter, 10),
            Moved {
                line: 7,
                fate: Fate::Kept
            }
        );
        let deleted = format!("{}{}", numbered(0..5), numbered(10..20));
        assert_eq!(
            follow_line(&old, &deleted, 7),
            Moved {
                line: 5,
                fate: Fate::Gone
            }
        );
        assert_eq!(
            follow_line(&old, &deleted, 12),
            Moved {
                line: 7,
                fate: Fate::Kept
            }
        );
        // The end deleted: the end.
        assert_eq!(
            follow_line(&old, &numbered(0..10), 15),
            Moved {
                line: 10,
                fate: Fate::Gone
            }
        );
        // The end, the empty line after the last newline, is the end.
        assert_eq!(
            follow_line(&old, &numbered(0..30), 20),
            Moved {
                line: 30,
                fate: Fate::Kept
            }
        );
    }

    /// What following a line through an edit costs, on a document of
    /// thousands of lines with one edit near its start (each keystroke's)
    /// and with edits all through it (an agent's rewrite read in again):
    /// `cargo test -p taste-app perf_follow -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn perf_follow_line() {
        let paragraph = "A sentence of the kind a document is made of, long enough to wrap.\n";
        let old: String = (0..4000)
            .map(|n| match n % 5 {
                4 => "\n".to_string(),
                _ => format!("{n}: {paragraph}"),
            })
            .collect();
        let typed = old.replacen("12: A sentence", "12: A sentencex", 1);
        let rewritten: String = old
            .lines()
            .enumerate()
            .map(|(n, line)| {
                if n % 37 == 0 {
                    format!("{line} (edited)\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        for (name, new) in [("one edit", &typed), ("an edit every 37 lines", &rewritten)] {
            let start = std::time::Instant::now();
            let runs = 20;
            for _ in 0..runs {
                std::hint::black_box(follow_line(&old, new, 3000));
            }
            println!(
                "follow_line over {} lines, {name}: {:.2}ms",
                old.lines().count(),
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
            );
        }
    }

    /// Two points on one line are its first place: a list and its first
    /// item, or a block's start and its first text.
    #[test]
    fn a_line_is_its_first_place() {
        let map = SourceMap::new(vec![(4, 30), (2, 10), (4, 25), (2, 12)]);
        assert_eq!(map.len(), 2);
        assert_eq!(map.place(2), 10);
        assert_eq!(map.place(3), 10);
        assert_eq!(map.place(4), 25);
        assert_eq!(map.place(1), 0);
    }
}
