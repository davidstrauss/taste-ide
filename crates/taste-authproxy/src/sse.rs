//! Put a private server's streaming events in the order the Messages API
//! documents: a content block is stopped before the next one starts.
//!
//! # The fault this corrects
//!
//! Anthropic streams a message as a sequence of content blocks, each one
//! `content_block_start`, then its deltas, then `content_block_stop`, and
//! never opens block N+1 until block N has stopped. `llama-server`'s
//! Anthropic-compatible endpoint does not: it opens the thinking block,
//! opens the text block on top of it, streams the text, and only then
//! sends the thinking block's `signature_delta` and its stop, followed by
//! the text block's stop (observed 2026-09-16 against gpt-oss-20b).
//!
//! That order is what doubled the reply in the chat. Claude Code emits a
//! consolidated `assistant` message per block as each block stops, and
//! the ACP adapter dedupes those against the deltas it has already
//! forwarded, resetting its record after each one. With the thinking
//! block's stop arriving after the text has streamed, the reset threw
//! away the record of the text just as the text block's own consolidation
//! came looking for it — and forwarded the whole text again.
//!
//! # Why the proxy, and why only here
//!
//! The proxy is the one hop the IDE owns on the way to a private server,
//! and the streaming format is documented, so putting the events in the
//! documented order is a gateway doing what a gateway is for — not a
//! reading of anybody's internals. It runs on the private route only:
//! Anthropic's own stream is already in this order, and a transform on
//! bytes the API sent is a risk with nothing to buy.
//!
//! # What it does, exactly
//!
//! Events are split on their blank-line terminator and parsed just far
//! enough to read `type` and `index`. When a block starts while another
//! is open, a `content_block_stop` for the open one is emitted first.
//! Whatever arrives later for a block that was closed this way — a
//! `signature_delta`, the server's own late stop — is dropped, because
//! under the documented order there is no place left for it. For
//! `llama-server` that costs an empty signature and a redundant stop;
//! every other event, including every text and thinking delta, passes
//! through byte for byte. A stream already in order is untouched.

use std::collections::HashSet;

/// The incremental normalizer: fed the upstream's bytes as they arrive,
/// it yields the bytes to forward. Holds at most one incomplete event.
#[derive(Default)]
pub struct BlockOrder {
    pending: Vec<u8>,
    /// The block currently open, when one is.
    open: Option<u64>,
    /// Blocks this normalizer closed ahead of the server, whose later
    /// events are dropped.
    closed_early: HashSet<u64>,
}

/// The terminator between two events.
const EVENT_END: &[u8] = b"\n\n";

impl BlockOrder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next bytes off the wire and return what may be sent on.
    /// Empty when the chunk ended mid-event; the rest comes with the next
    /// chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(chunk.len());
        while let Some(end) = find(&self.pending, EVENT_END) {
            let event: Vec<u8> = self.pending.drain(..end + EVENT_END.len()).collect();
            self.forward(&event, &mut out);
        }
        out
    }

    /// The stream ended: whatever is left was never a complete event (a
    /// non-streaming JSON body, or a truncated stream), and goes through
    /// as it came.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn forward(&mut self, event: &[u8], out: &mut Vec<u8>) {
        let (kind, index) = describe(event);
        match (kind.as_deref(), index) {
            (Some("content_block_start"), Some(index)) => {
                if let Some(open) = self.open {
                    if open != index {
                        out.extend_from_slice(stop_event(open).as_bytes());
                        self.closed_early.insert(open);
                    }
                }
                self.open = Some(index);
                out.extend_from_slice(event);
            }
            (Some("content_block_delta"), Some(index)) if self.closed_early.contains(&index) => {}
            (Some("content_block_stop"), Some(index)) => {
                if self.closed_early.contains(&index) {
                    return;
                }
                if self.open == Some(index) {
                    self.open = None;
                }
                out.extend_from_slice(event);
            }
            _ => out.extend_from_slice(event),
        }
    }
}

/// The `type` and `index` of an event's `data:` payload, if it has them.
fn describe(event: &[u8]) -> (Option<String>, Option<u64>) {
    let mut data = Vec::new();
    for line in event.split(|b| *b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"data:") {
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(rest);
        }
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) else {
        return (None, None);
    };
    (
        value
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        value.get("index").and_then(|i| i.as_u64()),
    )
}

fn stop_event(index: u64) -> String {
    format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, json: &str) -> String {
        format!("event: {kind}\ndata: {json}\n\n")
    }

    /// The stream as `llama-server` sends it, event for event.
    fn llama_order() -> Vec<String> {
        vec![
            event(
                "message_start",
                r#"{"type":"message_start","message":{"id":"chatcmpl-1","type":"message","role":"assistant","content":[],"model":"gpt-oss-20b","usage":{"input_tokens":73,"output_tokens":0}}}"#,
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need"}}"#,
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" words."}}"#,
            ),
            event(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"!"}}"#,
            ),
            event(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":""}}"#,
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            event(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}}"#,
            ),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ]
    }

    fn kinds(bytes: &[u8]) -> Vec<(String, Option<u64>)> {
        let text = std::str::from_utf8(bytes).unwrap();
        text.split("\n\n")
            .filter(|e| !e.is_empty())
            .map(|e| {
                let (kind, index) = describe(format!("{e}\n\n").as_bytes());
                (kind.unwrap(), index)
            })
            .collect()
    }

    /// The thinking block is stopped before the text block starts, its
    /// late signature and stop are dropped, and everything else is the
    /// server's own bytes.
    #[test]
    fn a_block_left_open_is_closed_before_the_next_one_opens() {
        let mut order = BlockOrder::new();
        let mut out = Vec::new();
        for e in llama_order() {
            out.extend(order.feed(e.as_bytes()));
        }
        out.extend(order.finish());
        let k = |s: &str, i: Option<u64>| (s.to_string(), i);
        assert_eq!(
            kinds(&out),
            vec![
                k("message_start", None),
                k("content_block_start", Some(0)),
                k("content_block_delta", Some(0)),
                k("content_block_delta", Some(0)),
                k("content_block_stop", Some(0)),
                k("content_block_start", Some(1)),
                k("content_block_delta", Some(1)),
                k("content_block_delta", Some(1)),
                k("content_block_stop", Some(1)),
                k("message_delta", None),
                k("message_stop", None),
            ]
        );
        // Every text delta reached the client exactly once, as sent.
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches(r#""text":"Hello""#).count(), 1);
        assert_eq!(text.matches("signature_delta").count(), 0);
        // The stop that was moved carries the documented shape.
        assert!(text.contains(&stop_event(0)));
    }

    /// Anthropic's order — every block stopped before the next starts —
    /// comes out byte for byte as it went in.
    #[test]
    fn a_stream_already_in_order_is_untouched() {
        let events = [
            event("message_start", r#"{"type":"message_start","message":{"id":"m"}}"#),
            event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            event("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#),
            event("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#),
            event("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#),
            event("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat();
        let mut order = BlockOrder::new();
        let mut out = order.feed(events.as_bytes());
        out.extend(order.finish());
        assert_eq!(out, events.as_bytes());
    }

    /// Chunk boundaries fall wherever the network put them; an event split
    /// across them is forwarded whole, once, and never early.
    #[test]
    fn an_event_split_across_chunks_is_reassembled() {
        let whole: String = llama_order().concat();
        let mut order = BlockOrder::new();
        let mut out = Vec::new();
        for byte in whole.as_bytes() {
            out.extend(order.feed(&[*byte]));
        }
        out.extend(order.finish());
        let mut at_once = BlockOrder::new();
        let mut expected = at_once.feed(whole.as_bytes());
        expected.extend(at_once.finish());
        assert_eq!(out, expected);
    }

    /// A body that is not a stream at all — a plain JSON answer, an error
    /// — has no event terminator and comes out of `finish` as it went in.
    #[test]
    fn a_non_streaming_body_passes_through_on_finish() {
        let body = br#"{"type":"error","error":{"type":"authentication_error","message":"no"}}"#;
        let mut order = BlockOrder::new();
        assert!(order.feed(body).is_empty());
        assert_eq!(order.finish(), body.to_vec());
    }
}
