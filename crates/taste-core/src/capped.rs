//! Bounded output capture that says what it dropped.
//!
//! Every place the IDE holds a command's output in memory — `ide_exec`
//! jobs, client-served ACP terminals, the console's mirrors of both — has
//! the same problem and must not solve it three ways: output is unbounded,
//! memory is not, and silently keeping the wrong half is worse than keeping
//! less.
//!
//! **Both ends, never one.** A compiler's first error is usually the real
//! one and the summary is always last, so the middle is what goes — with a
//! line saying how many bytes went. That is the only honest way to hand
//! back a truncated log: a reader (agent or human) who can see the elision
//! knows not to conclude anything from what is missing.
//!
//! ACP's `terminal/create` lets an agent set `outputByteLimit` and says a
//! client "truncates from the beginning of the output to stay within the
//! limit". The observable contract there is the limit and a valid character
//! boundary; both hold here (the split is `output_byte_limit` wide, and
//! rendering goes through `String::from_utf8_lossy`, which cannot produce
//! an invalid sequence). Dropping the middle rather than the head keeps the
//! same promise and answers more questions — see `taste_acp::terminal`,
//! where the deviation is stated for the agent as well.

/// Default per-stream cap, head and tail each. 96 KiB apiece has held the
/// `ide_exec` path since it shipped: big enough for a full `cargo` failure
/// at both ends, small enough that a runaway `yes` costs nothing.
pub const DEFAULT_CAP: usize = 96 * 1024;

/// Bounded capture: head, tail, and an honest count of what fell out.
#[derive(Debug, Clone)]
pub struct CappedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    total: usize,
}

impl Default for CappedOutput {
    fn default() -> Self {
        Self::new(DEFAULT_CAP, DEFAULT_CAP)
    }
}

impl CappedOutput {
    pub fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            head_cap,
            tail_cap,
            total: 0,
        }
    }

    /// A capture bounded by a total byte budget, split evenly between the
    /// two ends. This is what an ACP `outputByteLimit` becomes.
    ///
    /// A budget of zero would make every render pure elision notice, which
    /// tells the caller nothing it did not already know; one byte per end
    /// is the floor.
    pub fn with_budget(budget: usize) -> Self {
        let half = (budget / 2).max(1);
        Self::new(half, half)
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        for &byte in bytes {
            if self.head.len() < self.head_cap {
                self.head.push(byte);
                continue;
            }
            if self.tail.len() == self.tail_cap {
                self.tail.pop_front();
            }
            self.tail.push_back(byte);
        }
    }

    /// Everything kept, with the gap named. Lossy by construction: a cut
    /// can land mid-codepoint, and a replacement character is a better
    /// answer than a decode error or an invalid string on the wire.
    pub fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head).to_string();
        if !self.truncated() {
            return head;
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let elided = self.total - self.head.len() - self.tail.len();
        format!(
            "{head}\n… {elided} bytes elided by the IDE (output cap) …\n{}",
            String::from_utf8_lossy(&tail)
        )
    }

    /// True once anything has been dropped. Not "the cap was reached":
    /// output exactly the size of the head cap has lost nothing.
    pub fn truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    /// Bytes seen, including the ones that fell out.
    pub fn total(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_beyond_the_cap_keeps_both_ends_and_says_what_it_dropped() {
        let mut output = CappedOutput::new(64, 64);
        output.push(b"FIRST");
        output.push(&vec![b'x'; 512]);
        output.push(b"LAST");
        let rendered = output.render();
        assert!(rendered.starts_with("FIRST"), "the first error survives");
        assert!(rendered.ends_with("LAST"), "the summary survives");
        assert!(rendered.contains("bytes elided"), "and the loss is stated");
        assert!(output.truncated());
    }

    /// The elision count is the whole point: a reader must be able to tell
    /// how much is missing, not merely that something is.
    #[test]
    fn the_elided_count_is_the_bytes_actually_dropped() {
        let mut output = CappedOutput::new(10, 10);
        output.push(&[b'a'; 100]);
        assert_eq!(output.total(), 100);
        let rendered = output.render();
        assert!(rendered.contains("… 80 bytes elided"), "{rendered}");
    }

    /// Under the cap nothing is claimed to be missing, and the bytes come
    /// back byte-identical — the case that must never grow a marker.
    #[test]
    fn output_within_the_cap_is_untouched() {
        let mut output = CappedOutput::new(64, 64);
        output.push(b"hello\nworld\n");
        assert!(!output.truncated());
        assert_eq!(output.render(), "hello\nworld\n");

        // Exactly the head cap: full, but nothing dropped.
        let mut exact = CappedOutput::new(4, 4);
        exact.push(b"abcd");
        assert!(!exact.truncated());
        assert_eq!(exact.render(), "abcd");
    }

    /// An agent's `outputByteLimit` becomes a budget, and what comes back
    /// respects it: the retained bytes fit, both ends survive.
    #[test]
    fn a_budget_bounds_what_is_retained_at_both_ends() {
        let mut output = CappedOutput::with_budget(100);
        output.push(b"HEAD");
        output.push(&vec![b'.'; 10_000]);
        output.push(b"TAIL");
        assert!(output.head.len() + output.tail.len() <= 100);
        let rendered = output.render();
        assert!(rendered.starts_with("HEAD"), "{rendered}");
        assert!(rendered.ends_with("TAIL"), "{rendered}");
    }

    /// A cut through the middle of a multi-byte codepoint must still
    /// render — ACP requires a valid string, and lossy decoding gives one.
    #[test]
    fn a_cut_through_a_codepoint_still_renders_a_valid_string() {
        let mut output = CappedOutput::new(3, 3);
        output.push("😀😀😀".as_bytes());
        let rendered = output.render();
        assert!(rendered.contains("bytes elided"), "{rendered}");
        // The assertion is that this is a String at all; a panic or an
        // invalid sequence is what the lossy decode exists to prevent.
        assert!(!rendered.is_empty());
    }
}
