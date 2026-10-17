//! A message written during the agent's turn, and the note it goes out
//! with.
//!
//! A prompt sent while a turn runs waits behind it (`taste_acp`'s command
//! loop holds it until the turn ends), and by the time it goes the agent
//! has done more: the user wrote it against what they could see then, not
//! against what followed. So it goes out after a note that says how long it
//! waited and where in the conversation it was written (David, 2026-10-11:
//! "prefix it with how long it's been queued and a reference point for the
//! agent to understand the context in the feed where it was sent").
//!
//! The note is the first line of the prompt's text, so a replayed
//! conversation carries it back; [`split`] takes it off again, and the
//! transcript draws it as the card's queued badge rather than as words the
//! user typed.

use std::time::{Duration, Instant};

/// The most characters of the agent's own words a note quotes.
pub const QUOTE_CHARS: usize = 120;

const OPEN: &str = "[Queued for ";

/// Where in the conversation a queued message was written: the last thing
/// the user could see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// The agent's message being streamed then, as far as it had got.
    Writing(String),
    /// The agent's last message, finished.
    Message(String),
    /// The agent's last step, as its transcript line reads.
    Step(String),
    /// Nothing of the turn yet.
    TurnStart,
}

/// A message held behind a turn: when it was written, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub at: Instant,
    pub after: Reference,
}

/// The end of `text`, as a quote: one line, at most [`QUOTE_CHARS`]
/// characters, cut at a word and marked with `…` when cut.
pub fn tail(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut length = 0;
    for word in words.iter().rev() {
        let next = length + word.chars().count() + usize::from(!out.is_empty());
        if next > QUOTE_CHARS {
            break;
        }
        out.push(word);
        length = next;
    }
    out.reverse();
    let mut quote = out.join(" ");
    if out.len() < words.len() {
        if quote.is_empty() {
            // One word longer than a quote: its end.
            let last = words.last().copied().unwrap_or_default();
            let skip = last.chars().count().saturating_sub(QUOTE_CHARS - 1);
            quote = last.chars().skip(skip).collect();
        }
        quote.insert(0, '…');
    }
    quote.replace('"', "'")
}

/// How long, as the note says it.
pub fn duration_text(duration: Duration) -> String {
    let secs = duration.as_secs_f64().round() as u64;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The line a queued message goes out after, for one that waited `waited`.
pub fn note(waited: Duration, after: &Reference) -> String {
    let place = match after {
        Reference::Writing(text) => format!("while you were writing \"{}\"", tail(text)),
        Reference::Message(text) => format!("after your message ending \"{}\"", tail(text)),
        Reference::Step(line) => format!("after your step \"{}\"", tail(line)),
        Reference::TurnStart => "as your turn began".to_string(),
    };
    format!(
        "{OPEN}{}. Written {place}; it does not know what you did after that.]",
        duration_text(waited)
    )
}

/// A prompt's text with its note taken off: the note, and the message. `None`
/// when the text does not start with one.
pub fn split(text: &str) -> Option<(String, String)> {
    if !text.starts_with(OPEN) {
        return None;
    }
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    if !first.ends_with(']') {
        return None;
    }
    Some((first.to_string(), rest.trim_start_matches('\n').to_string()))
}

/// A replayed prompt that several queued messages went out as together
/// (`taste_acp`'s `join_waiting`): each note and the message after it, in
/// order. `None` when the text does not start with a note. A line is a
/// note when it is one whole: `[Queued for …]`.
pub fn split_all(text: &str) -> Option<Vec<(String, String)>> {
    split(text)?;
    let mut out: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if line.starts_with(OPEN) && line.ends_with(']') {
            out.push((line.to_string(), Vec::new()));
        } else if let Some((_, lines)) = out.last_mut() {
            lines.push(line);
        }
    }
    Some(
        out.into_iter()
            .map(|(note, lines)| (note, lines.join("\n").trim_matches('\n').to_string()))
            .collect(),
    )
}

/// What the card's badge says for a note read back from a replay: how long
/// it waited.
pub fn badge(note: &str) -> String {
    let waited = note
        .strip_prefix(OPEN)
        .and_then(|rest| rest.split_once(". Written"))
        .map(|(waited, _)| waited)
        .unwrap_or_default();
    format!("queued for {waited}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_is_the_end_cut_at_a_word() {
        assert_eq!(tail("short  and\nsweet"), "short and sweet");
        let long = "word ".repeat(40);
        let quote = tail(&long);
        assert!(quote.starts_with("…word"), "{quote}");
        assert!(quote.chars().count() <= QUOTE_CHARS + 1);
        assert_eq!(tail(&"x".repeat(300)).chars().count(), QUOTE_CHARS);
        assert_eq!(tail("say \"this\""), "say 'this'");
    }

    #[test]
    fn the_note_says_how_long_and_where_and_comes_off_again() {
        let note = note(
            Duration::from_secs(192),
            &Reference::Message("I will move the cache next.".into()),
        );
        assert_eq!(
            note,
            "[Queued for 3m 12s. Written after your message ending \"I will move the cache \
             next.\"; it does not know what you did after that.]"
        );
        let sent = format!("{note}\n\nstop and look at the tests first");
        let (said, message) = split(&sent).unwrap();
        assert_eq!(said, note);
        assert_eq!(message, "stop and look at the tests first");
        assert_eq!(badge(&said), "queued for 3m 12s");
        assert_eq!(split("[Queued for lunch] is what I typed"), None);
        assert_eq!(split("plain"), None);
        assert_eq!(duration_text(Duration::from_secs(3725)), "1h 2m");
        assert!(super::note(Duration::ZERO, &Reference::TurnStart).contains("as your turn began"));
    }

    #[test]
    fn messages_that_went_out_together_come_apart_again() {
        let first = note(Duration::from_secs(90), &Reference::TurnStart);
        let second = note(
            Duration::from_secs(30),
            &Reference::Step("Run tests".into()),
        );
        let sent = format!("{first}\nsaving should not block\n\nreally\n{second}\na bonus budget");
        let parts = split_all(&sent).unwrap();
        assert_eq!(
            parts,
            vec![
                (first, "saving should not block\n\nreally".to_string()),
                (second, "a bonus budget".to_string()),
            ]
        );
        assert_eq!(split_all("plain"), None);
    }
}
