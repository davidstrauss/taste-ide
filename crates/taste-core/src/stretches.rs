//! **Where a conversation's record breaks into stretches.**
//!
//! The chat pane draws a conversation from its record (`chatarchive`) and
//! keeps only the part near the reader drawn: the rest stands in as space
//! of about its height, drawn again when the reader comes near it. The
//! unit drawn and let go is a stretch, and a stretch has to be something a
//! renderer can start fresh at and finish whole, so it begins only where
//! nothing before it is still being drawn into:
//!
//! - at a user message, which is where a turn begins, and
//! - inside a long turn, before a step, once [`STEPS`] steps have gone by
//!   and none is still running, so a turn of hundreds of steps is not one
//!   stretch of hundreds of rows. A step still running is updated later in
//!   the record, and its card has to be in the same stretch as its updates.
//!
//! Decided from the records alone, so the stretches a history is cut into
//! are the ones the live conversation was cut into as it happened.

use std::collections::HashSet;

use serde_json::Value;

/// How many steps a stretch inside a turn holds before the next may begin
/// a stretch of its own.
pub const STEPS: usize = 16;

/// The record's kind of update, as ACP names it (`sessionUpdate`), or
/// `None` for the IDE's own entries (its notes and session markers).
fn kind(record: &Value) -> Option<&str> {
    record.get("sessionUpdate").and_then(Value::as_str)
}

/// Whether a step's status means it will not be updated again.
fn settled(record: &Value) -> bool {
    matches!(
        record.get("status").and_then(Value::as_str),
        Some("completed" | "failed")
    )
}

/// Reads the record in order and says where each stretch begins.
#[derive(Debug, Default, Clone)]
pub struct Segmenter {
    /// Anything read yet: the first record begins the first stretch.
    started: bool,
    /// The last update read was part of a user message.
    in_user: bool,
    /// Steps begun in this stretch.
    steps: usize,
    /// Steps begun and not yet settled.
    running: HashSet<String>,
}

impl Segmenter {
    /// Read `record`; whether it begins a stretch.
    pub fn begins(&mut self, record: &Value) -> bool {
        let first = !self.started;
        self.started = true;
        let Some(kind) = kind(record) else {
            // A note or a marker goes with whatever it is among.
            return first;
        };
        let user = kind == "user_message_chunk";
        let after_user = std::mem::replace(&mut self.in_user, user);
        let mut begins = first;
        if user && !after_user {
            begins = true;
            self.running.clear();
        }
        if kind == "tool_call" && self.steps >= STEPS && self.running.is_empty() {
            begins = true;
        }
        if begins {
            self.steps = 0;
        }
        let id = record
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_string);
        match kind {
            "tool_call" => {
                self.steps += 1;
                if let Some(id) = id.filter(|_| !settled(record)) {
                    self.running.insert(id);
                }
            }
            "tool_call_update" if settled(record) => {
                if let Some(id) = id {
                    self.running.remove(&id);
                }
            }
            _ => {}
        }
        begins
    }
}

/// `records` cut into stretches, in order, and the segmenter as it stands
/// after the last, to go on reading the live conversation with.
pub fn cut(records: Vec<Value>) -> (Vec<Vec<Value>>, Segmenter) {
    let mut segmenter = Segmenter::default();
    let mut stretches: Vec<Vec<Value>> = Vec::new();
    for record in records {
        if segmenter.begins(&record) || stretches.is_empty() {
            stretches.push(Vec::new());
        }
        if let Some(stretch) = stretches.last_mut() {
            stretch.push(record);
        }
    }
    (stretches, segmenter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text}})
    }
    fn said(text: &str) -> Value {
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
    }
    fn step(id: &str, status: &str) -> Value {
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": id, "status": status})
    }
    fn settle(id: &str) -> Value {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"})
    }

    /// A turn begins a stretch at its message, every block of which stays
    /// together, with the notes among them.
    #[test]
    fn a_turn_is_a_stretch_from_its_message() {
        let (stretches, _) = cut(vec![
            json!({"tasteSession": "s"}),
            said("hello"),
            user("one"),
            json!({"tasteNote": "briefed"}),
            user("two"),
            said("answer"),
            step("a", "completed"),
            user("next"),
            said("again"),
        ]);
        let lengths: Vec<usize> = stretches.iter().map(Vec::len).collect();
        assert_eq!(lengths, vec![2, 5, 2]);
    }

    /// A long turn breaks before a step, every [`STEPS`] steps, but never
    /// while a step it began is still running.
    #[test]
    fn a_long_turn_breaks_between_settled_steps() {
        let mut records = vec![user("go")];
        for n in 0..STEPS {
            records.push(step(&format!("s{n}"), "completed"));
        }
        // Running across where the break would be.
        records.push(step("long", "in_progress"));
        records.push(step("after", "completed"));
        records.push(settle("long"));
        records.push(said("done"));
        records.push(step("last", "completed"));
        let (stretches, _) = cut(records);
        let lengths: Vec<usize> = stretches.iter().map(Vec::len).collect();
        assert_eq!(lengths, vec![1 + STEPS, 5]);
        assert_eq!(stretches[1][0]["toolCallId"], "long");
    }

    /// Read live, record by record, the stretches come out as a history
    /// cut whole would cut them.
    #[test]
    fn live_reading_agrees_with_a_cut() {
        let mut records = vec![said("hi")];
        for turn in 0..3 {
            records.push(user(&format!("turn {turn}")));
            for n in 0..(STEPS * 2 + 3) {
                records.push(step(&format!("{turn}-{n}"), "completed"));
                records.push(said("…"));
            }
        }
        let (mut live, mut segmenter) = cut(records[..40].to_vec());
        for record in records[40..].iter().cloned() {
            if segmenter.begins(&record) {
                live.push(Vec::new());
            }
            live.last_mut().unwrap().push(record);
        }
        assert_eq!(live, cut(records).0);
    }
}
