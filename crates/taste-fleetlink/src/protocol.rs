//! The varlink wire protocol, hand-rolled — the whole of it that a
//! read-only service needs.
//!
//! Framing is one JSON object per message, terminated by a NUL byte, in
//! both directions (varlink.org → "Protocol"). A call names a
//! fully-qualified method and carries optional `parameters`; three flags
//! modify it, and this service honours all three by answering correctly
//! rather than by pretending they do not exist:
//!
//! - `more` — the client will accept a sequence of replies. Replies that
//!   are not the last carry `continues: true`.
//! - `oneway` — the client wants no reply at all. Send nothing.
//! - `upgrade` — the client wants the connection switched to some other
//!   protocol. Nothing here offers one, so it is refused.
//!
//! Hand-rolled rather than pulled in: this is the same decision, for the
//! same reasons, as `taste-mcp`'s JSON-RPC. The wire format is a hundred
//! lines and fully specified; the `varlink` crate's model is a synchronous
//! `std::io` server plus a build-time code generator, which would put a
//! second concurrency style and a codegen step into a workspace that has
//! neither. The IDL stays the interface's source of truth either way — it
//! is checked in beside this file and served verbatim over
//! `GetInterfaceDescription`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Message terminator, both directions.
pub const DELIMITER: u8 = 0;

/// The interface whose errors every varlink service is expected to speak.
pub const SERVICE_INTERFACE: &str = "org.varlink.service";

/// One call, as it arrives.
#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    /// Fully qualified: `<interface>.<Method>`.
    pub method: String,
    #[serde(default)]
    pub parameters: Value,
    /// The client accepts more than one reply.
    #[serde(default)]
    pub more: bool,
    /// The client wants no reply.
    #[serde(default)]
    pub oneway: bool,
    /// The client wants the connection upgraded to another protocol.
    #[serde(default)]
    pub upgrade: bool,
}

impl Call {
    /// The interface half of `method`, or `None` when it is not qualified.
    pub fn interface(&self) -> Option<&str> {
        let (interface, method) = self.method.rsplit_once('.')?;
        if interface.is_empty() || method.is_empty() {
            return None;
        }
        Some(interface)
    }
}

/// One reply. Either `parameters` or `error` is set, never both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set on every reply of a `more` sequence except the last.
    #[serde(default, skip_serializing_if = "is_false")]
    pub continues: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Reply {
    pub fn ok(parameters: Value) -> Self {
        Self {
            parameters: Some(parameters),
            error: None,
            continues: false,
        }
    }

    /// One reply of a `more` sequence, with others to follow.
    pub fn ok_more(parameters: Value) -> Self {
        Self {
            continues: true,
            ..Self::ok(parameters)
        }
    }

    pub fn error(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            parameters: Some(parameters),
            error: Some(name.into()),
            continues: false,
        }
    }

    // The standard errors, spelled once. A client that knows varlink knows
    // these already, which is the point of using the standard names rather
    // than inventing a parallel vocabulary under our own interface.

    pub fn interface_not_found(interface: &str) -> Self {
        Self::error(
            format!("{SERVICE_INTERFACE}.InterfaceNotFound"),
            json!({ "interface": interface }),
        )
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::error(
            format!("{SERVICE_INTERFACE}.MethodNotFound"),
            json!({ "method": method }),
        )
    }

    pub fn invalid_parameter(parameter: &str) -> Self {
        Self::error(
            format!("{SERVICE_INTERFACE}.InvalidParameter"),
            json!({ "parameter": parameter }),
        )
    }

    /// A method that only streams, called without the `more` flag.
    pub fn expected_more() -> Self {
        Self::error(format!("{SERVICE_INTERFACE}.ExpectedMore"), json!({}))
    }
}

/// Encode a reply as a framed message: JSON, then the NUL.
pub fn encode(reply: &Reply) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(reply).unwrap_or_else(|_| {
        // Serializing a Reply cannot fail for any value this crate builds;
        // an unwrap here would still be a panic in a connection task, so
        // answer with something a client can parse instead.
        br#"{"error":"org.varlink.service.MethodNotImplemented"}"#.to_vec()
    });
    bytes.push(DELIMITER);
    bytes
}

/// Decode one framed message. The NUL must already be stripped.
pub fn decode_call(bytes: &[u8]) -> Result<Call, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_json_and_its_flags_default_to_off() {
        let call = decode_call(br#"{"method":"net.davidstrauss.taste.Fleet.List"}"#).unwrap();
        assert_eq!(call.method, "net.davidstrauss.taste.Fleet.List");
        assert!(!call.more && !call.oneway && !call.upgrade);
        assert_eq!(call.interface(), Some("net.davidstrauss.taste.Fleet"));
        assert_eq!(
            decode_call(br#"{"method":"List"}"#).unwrap().interface(),
            None,
            "an unqualified method names no interface"
        );
    }

    #[test]
    fn a_reply_carries_parameters_or_an_error_and_frames_with_a_nul() {
        let framed = encode(&Reply::ok(json!({ "rows": [] })));
        assert_eq!(framed.last(), Some(&0u8));
        assert_eq!(
            String::from_utf8(framed[..framed.len() - 1].to_vec()).unwrap(),
            r#"{"parameters":{"rows":[]}}"#,
            "no continues on a final reply, and no null error field"
        );
        let framed = encode(&Reply::ok_more(json!({})));
        assert!(String::from_utf8(framed)
            .unwrap()
            .contains(r#""continues":true"#));
        let framed = encode(&Reply::method_not_found("x.Y"));
        let text = String::from_utf8(framed).unwrap();
        assert!(text.contains(r#""error":"org.varlink.service.MethodNotFound""#));
        assert!(text.contains(r#""method":"x.Y""#));
    }
}
