//! Where a Markdown document's source lines landed in its preview: what a
//! line asked of a previewed document — a search result's, a link's
//! `#L12` — is turned into a place in the preview's buffer with. Pure, no
//! GTK: the preview's writer (`crate::doc_text`) records a point as it
//! writes each block, and each line of a block's text where it is cheap
//! to know, and this answers from those points.
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
}

impl SourceMap {
    pub fn new(mut points: Vec<(u32, i32)>) -> Self {
        points.sort_unstable();
        points.dedup_by_key(|point| point.0);
        Self { points }
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
