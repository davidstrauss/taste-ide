//! The proxy against a mock upstream: the gate, the header swap, and the
//! property everything else depends on — that responses stream.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use taste_authproxy::{
    AuthProxy, CredentialSource, FileCredentials, FilePrivateUpstream, Handle, IdeCredentials,
    Route, StaticKey,
};

type MockBody = BoxBody<Bytes, Infallible>;

/// What the mock upstream saw. Never anything the proxy did not send.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone)]
struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    hits: Arc<AtomicU64>,
}

impl Upstream {
    fn uri(&self) -> Uri {
        format!("http://{}", self.addr).parse().unwrap()
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }
}

/// A body fed from a channel, so a test can decide when each chunk lands.
struct ChannelBody(tokio::sync::mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.get_mut()
            .0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// A mock Anthropic API.
///
/// `/sse` streams three events 200ms apart; `/limited` refuses the way a
/// spent subscription does; everything else answers at once with a
/// Messages-shaped body carrying `usage`, under the rate-limit headers the
/// API documents.
async fn start_upstream() -> Upstream {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicU64::new(0));

    let accept_seen = seen.clone();
    let accept_hits = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = stream.set_nodelay(true);
            let seen = accept_seen.clone();
            let hits = accept_hits.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let seen = seen.clone();
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::Relaxed);
                        let (parts, body) = req.into_parts();
                        let sse = parts.uri.path() == "/sse";
                        let stall = parts.uri.path() == "/stall";
                        let interleaved = parts.uri.path() == "/interleaved";
                        let limited = parts.uri.path() == "/limited";
                        let models = parts.uri.path() == "/v1/models";
                        let record = Seen {
                            method: parts.method.to_string(),
                            uri: parts.uri.to_string(),
                            headers: parts
                                .headers
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_string(),
                                        value.to_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect(),
                            body: body
                                .collect()
                                .await
                                .map(|c| c.to_bytes().to_vec())
                                .unwrap_or_default(),
                        };
                        seen.lock().unwrap().push(record);

                        let response: Response<MockBody> = if sse {
                            let (tx, rx) = tokio::sync::mpsc::channel(4);
                            tokio::spawn(async move {
                                for index in 0..3u32 {
                                    if tx
                                        .send(Bytes::from(format!(
                                            "event: chunk\ndata: {{\"index\":{index},\"usage\":{{\"output_tokens\":{}}}}}\n\n",
                                            (index + 1) * 10
                                        )))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                            });
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(BodyExt::boxed(ChannelBody(rx)))
                                .unwrap()
                        } else if stall {
                            // One event, then nothing, forever: a server
                            // whose machine went to sleep mid-answer.
                            let (tx, rx) = tokio::sync::mpsc::channel(4);
                            tokio::spawn(async move {
                                let _ = tx
                                    .send(Bytes::from_static(
                                        b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                                    ))
                                    .await;
                                std::future::pending::<()>().await;
                                drop(tx);
                            });
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(BodyExt::boxed(ChannelBody(rx)))
                                .unwrap()
                        } else if interleaved {
                            // A private server's stream as llama-server
                            // sends it: the thinking block left open under
                            // the text block, its signature and stop last.
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(BodyExt::boxed(Full::new(Bytes::from_static(
                                    LLAMA_ORDER.as_bytes(),
                                ))))
                                .unwrap()
                        } else if models {
                            // The Models API's page, as documented: an
                            // account with Opus and two Fables.
                            Response::builder()
                                .header("content-type", "application/json")
                                .body(BodyExt::boxed(Full::new(Bytes::from_static(
                                    br#"{"data":[{"type":"model","id":"claude-opus-5","display_name":"Claude Opus 5","created_at":"2026-04-01T00:00:00Z","max_input_tokens":1000000},{"type":"model","id":"claude-fable-5","display_name":"Claude Fable 5","created_at":"2026-06-01T00:00:00Z","max_input_tokens":1000000},{"type":"model","id":"claude-fable-5-1","display_name":"Claude Fable 5.1","created_at":"2026-08-25T00:00:00Z","max_input_tokens":1000000}],"has_more":false,"first_id":"claude-opus-5","last_id":"claude-fable-5-1"}"#,
                                ))))
                                .unwrap()
                        } else if limited {
                            // A spent subscription, as the API refuses it:
                            // 429, a `retry-after`, and a message naming
                            // the window.
                            Response::builder()
                                .status(StatusCode::TOO_MANY_REQUESTS)
                                .header("content-type", "application/json")
                                .header("retry-after", "1800")
                                .header("anthropic-ratelimit-unified-status", "rejected")
                                .header("anthropic-ratelimit-unified-5h-utilization", "100")
                                .body(BodyExt::boxed(Full::new(Bytes::from_static(
                                    br#"{"type":"error","error":{"type":"rate_limit_error","message":"You have hit your session limit. Access resumes at 4:00 PM."}}"#,
                                ))))
                                .unwrap()
                        } else {
                            Response::builder()
                                .header("content-type", "application/json")
                                // The documented family, plus the plan
                                // windows an OAuth subscription may add.
                                .header("anthropic-ratelimit-requests-limit", "1000")
                                .header("anthropic-ratelimit-requests-remaining", "980")
                                .header("anthropic-ratelimit-input-tokens-limit", "2000000")
                                .header("anthropic-ratelimit-input-tokens-remaining", "1600000")
                                .header("anthropic-ratelimit-unified-status", "allowed")
                                .header("anthropic-ratelimit-unified-5h-utilization", "27")
                                .header("anthropic-ratelimit-unified-7d-utilization", "61")
                                .body(BodyExt::boxed(Full::new(Bytes::from_static(
                                    br#"{"type":"message","usage":{"input_tokens":11,"cache_read_input_tokens":9999,"output_tokens":22}}"#,
                                ))))
                                .unwrap()
                        };
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    Upstream { addr, seen, hits }
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build(HttpConnector::new())
}

async fn get(handle: &Handle, path: &str, token: Option<&str>) -> Response<Incoming> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("{}{path}", handle.base_url()));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    client()
        .request(builder.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_placeholder_is_swapped_for_the_real_credential() {
    let upstream = start_upstream().await;
    let handle =
        AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real-api-key"))).unwrap();
    let placeholder = handle.issue_placeholder("primary");
    assert!(placeholder.starts_with("sk-ant-taste-"), "{placeholder}");

    let request = Request::builder()
        .method("POST")
        .uri(format!("{}/v1/messages?beta=true", handle.base_url()))
        .header("authorization", format!("Bearer {placeholder}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(br#"{"model":"claude"}"#)))
        .unwrap();
    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.uri, "/v1/messages?beta=true");
    assert_eq!(seen.body, br#"{"model":"claude"}"#);
    // The real credential went out...
    assert_eq!(seen.header("x-api-key"), Some("real-api-key"));
    // ...and no trace of the placeholder did.
    assert_eq!(seen.header("authorization"), None);
    assert!(
        !String::from_utf8_lossy(&seen.body).contains(&placeholder),
        "placeholder leaked into the body"
    );
    // Unrelated headers ride along untouched.
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
}

/// The proxy's one request of its own: the account's models, read with the
/// real credential and the documented headers, cached beside the IDE's
/// state, and distilled to the newest model above Opus.
#[tokio::test]
async fn the_model_listing_is_read_with_the_real_credential_and_cached() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(
        upstream.uri(),
        Arc::new(StaticKey::oauth("real-oauth-token")),
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cache_path = cache.path().join("taste-ide/models.json");
    // Nothing until asked — a proxy that never needs the list never asks.
    assert_eq!(handle.models(), None);
    assert_eq!(upstream.hits(), 0);

    handle.refresh_models(Some(cache_path.clone()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.top_tier_model().is_none() {
        assert!(Instant::now() < deadline, "the listing never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let top = handle.top_tier_model().unwrap();
    assert_eq!(top.id, "claude-fable-5-1");
    assert!(top.has_1m_context());
    assert_eq!(handle.models().unwrap().len(), 3);

    let seen = upstream.last();
    assert_eq!(seen.method, "GET");
    assert!(seen.uri.starts_with("/v1/models"), "{}", seen.uri);
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer real-oauth-token")
    );
    assert_eq!(seen.header("anthropic-beta"), Some("oauth-2025-04-20"));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));

    // Written back, so the next launch has it before its first spawn.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cache_path.exists() {
        assert!(Instant::now() < deadline, "the cache was never written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cached = taste_authproxy::models::load_cached(&cache_path).unwrap();
    assert_eq!(cached.len(), 3);
}

/// The whole of the second upstream, in one test: two placeholders, two
/// hosts, and the right key on each.
///
/// The negative half is the one that matters most. A placeholder minted
/// without saying reaches the API and nothing else, so a private server
/// is reached because a spawn chose it — never because a default fell
/// through. Both placeholders are one environment's, deliberately: that
/// is a "Claude Code" chat and a "Claude Code (Private)" chat open side
/// by side, which is the mix the agent split exists for.
#[tokio::test]
async fn two_placeholders_reach_two_upstreams_with_the_right_key_on_each() {
    let anthropic = start_upstream().await;
    let private_server = start_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-model.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"base_url":"{}","kind":"bearer","token":"llama-key","model":"gpt-oss-20b"}}"#,
            private_server.uri()
        ),
    )
    .unwrap();

    let handle = AuthProxy::spawn(
        anthropic.uri(),
        Arc::new(StaticKey::api_key("real-api-key")),
    )
    .unwrap();
    handle.set_private_upstream(Some(Arc::new(FilePrivateUpstream::new(&path))));

    let on_the_api = handle.issue_placeholder("i-0028");
    let on_the_card = handle.issue_placeholder_for("i-0028", Route::Private);
    // The route is the placeholder's, fixed at minting, and the agent that
    // holds it is told nothing.
    assert_eq!(handle.route_of(&on_the_card), Some(Route::Private));
    assert_eq!(handle.route_of(&on_the_api), Some(Route::Anthropic));
    assert_eq!(handle.route_of("placeholder-nobody-issued"), None);

    let response = get(&handle, "/v1/messages", Some(&on_the_card)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().collect().await.unwrap();
    assert_eq!(private_server.hits(), 1);
    assert_eq!(anthropic.hits(), 0, "the API must not have been called");
    let seen = private_server.last();
    assert_eq!(seen.uri, "/v1/messages");
    // The private server's own key, in the header the file named...
    assert_eq!(seen.header("authorization"), Some("Bearer llama-key"));
    // ...and no trace of the account's credential on a host that is not
    // Anthropic's. This is the whole point of a second upstream.
    assert_eq!(seen.header("x-api-key"), None);

    let response = get(&handle, "/v1/messages", Some(&on_the_api)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().collect().await.unwrap();
    assert_eq!(anthropic.hits(), 1);
    assert_eq!(
        private_server.hits(),
        1,
        "a placeholder minted for the API stays on it"
    );
    let seen = anthropic.last();
    assert_eq!(seen.header("x-api-key"), Some("real-api-key"));
    assert_eq!(seen.header("authorization"), None);

    // Spend lands in the environment's counters whichever host served it:
    // both chats are i-0028's, and both turns are its.
    assert_eq!(handle.spend("i-0028").requests, 2);
    assert_eq!(handle.spend("i-0028").input_tokens, 22);

    // The account's windows describe the account, so only the turn that
    // went to the account is in them — and it is attributed to the
    // environment, which is the same one either way.
    let quota = handle.quota();
    assert_eq!(quota.observed_for.as_deref(), Some("i-0028"));

    // And what the settings row may say about it, having read the file
    // once.
    let facts = handle.private_model().unwrap();
    assert_eq!(facts.label, "gpt-oss-20b");
    assert_eq!(facts.model.as_deref(), Some("gpt-oss-20b"));
}

/// The stream `llama-server` was observed to send (2026-09-16), which
/// doubled every reply that had a thought in front of it — see
/// `taste_authproxy::sse`.
const LLAMA_ORDER: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"chatcmpl-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"gpt-oss-20b\",\"usage\":{\"input_tokens\":73,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Need words.\"}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello!\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":20}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// A private server's stream reaches the agent in the documented block
/// order — the thinking block stopped before the text block starts — and
/// the API's own stream is not touched at all.
#[tokio::test]
async fn a_private_servers_stream_is_put_in_block_order_and_the_apis_is_not() {
    let anthropic = start_upstream().await;
    let private_server = start_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-model.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"base_url":"{}","token":"llama-key","model":"gpt-oss-20b"}}"#,
            private_server.uri()
        ),
    )
    .unwrap();
    let handle = AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    handle.set_private_upstream(Some(Arc::new(FilePrivateUpstream::new(&path))));

    let on_the_card = handle.issue_placeholder_for("i-0028", Route::Private);
    let response = get(&handle, "/interleaved", Some(&on_the_card)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    let position = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    let stop_0 = position(r#"{"type":"content_block_stop","index":0}"#);
    let start_1 = position(r#""type":"content_block_start","index":1"#);
    assert!(
        stop_0 < start_1,
        "block 0 stops before block 1 starts:
{text}"
    );
    assert_eq!(
        text.matches(r#""type":"content_block_stop","index":0"#)
            .count(),
        1
    );
    assert_eq!(text.matches("signature_delta").count(), 0);
    // The words themselves: every delta once, in order, as sent.
    assert_eq!(text.matches(r#""text":"Hello!""#).count(), 1);
    assert_eq!(text.matches(r#""thinking":"Need words.""#).count(), 1);
    assert!(text.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    // Spend was still counted off the stream that went through.
    assert_eq!(handle.spend("i-0028").output_tokens, 20);

    // The same bytes off the API are the API's: untouched.
    let on_the_api = handle.issue_placeholder("i-0028");
    let response = get(&handle, "/interleaved", Some(&on_the_api)).await;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), LLAMA_ORDER.as_bytes());
}

/// An upstream that stops sending mid-stream — the machine running the
/// private model went to sleep — does not leave the agent waiting on
/// "Working…": after the idle window the proxy ends the stream with an
/// `error` event in the API's own shape, and the connection completes.
#[tokio::test]
async fn a_stream_that_falls_silent_is_ended_with_an_error_event() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("k"))).unwrap();
    handle.set_stream_idle_timeout(Duration::from_millis(300));
    let placeholder = handle.issue_placeholder("i-0028");

    let started = Instant::now();
    let response = get(&handle, "/stall", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
        .await
        .expect("the stream must end on its own")
        .unwrap()
        .to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.starts_with("event: message_start"), "{text}");
    assert!(text.contains("event: error\n"), "{text}");
    assert!(text.contains("\"type\":\"api_error\""), "{text}");
    assert!(text.contains("sent nothing for 0s"), "{text}");
    assert!(text.ends_with("\n\n"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "ended by the idle window, not by anything slower"
    );
    // Spend was still recorded for what did arrive.
    assert_eq!(handle.spend("i-0028").requests, 1);
}

/// A private server that is not there is not simply "unreachable": the
/// proxy tries to wake its machine first and says what it could do. With
/// nothing learned about the machine yet, that is that it cannot wake it
/// and why — and nothing was sent to the API on the way.
#[tokio::test]
async fn an_unreachable_private_server_is_reported_with_the_wake_attempt() {
    let anthropic = start_upstream().await;
    // A port nothing listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = listener.local_addr().unwrap().port();
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-model.json");
    std::fs::write(
        &path,
        format!(r#"{{"base_url":"http://127.0.0.1:{closed}","token":"k","model":"m"}}"#),
    )
    .unwrap();
    let handle = AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    handle.set_private_upstream(Some(Arc::new(FilePrivateUpstream::new(&path))));
    handle.set_wake_wait(Duration::from_millis(10));
    let said = Arc::new(Mutex::new(Vec::<(Option<String>, String)>::new()));
    let recorder = said.clone();
    handle.set_notice(Arc::new(move |env, text| {
        recorder
            .lock()
            .unwrap()
            .push((env.map(str::to_string), text));
    }));

    let placeholder = handle.issue_placeholder_for("i-0028", Route::Private);
    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("cannot be woken yet"), "{text}");
    assert!(text.contains("hardware address"), "{text}");
    assert_eq!(anthropic.hits(), 0, "nothing fell back to the API");
    // ...and the environment's chat was told, as it happened.
    {
        let said = said.lock().unwrap();
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].0.as_deref(), Some("i-0028"));
        assert!(said[0].1.contains("cannot be woken yet"));
    }

    // The connection test says the same thing in its own verdict.
    let err = handle.probe_private().await.unwrap_err().to_string();
    assert!(err.contains("cannot be woken yet"), "{err}");
}

/// The settings form's "test connection": one request to the private
/// server, on the path an agent's turn takes, with the stored key in the
/// header the file names — and nothing at all to the API.
#[tokio::test]
async fn probing_the_private_server_speaks_to_it_the_way_a_turn_would() {
    let anthropic = start_upstream().await;
    let private_server = start_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-model.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"base_url":"{}","token":"llama-key","model":"gpt-oss-20b"}}"#,
            private_server.uri()
        ),
    )
    .unwrap();
    let handle = AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();

    // Nothing provisioned: the probe says so rather than asking the API.
    let err = handle.probe_private().await.unwrap_err().to_string();
    assert!(err.contains("no private model is provisioned"), "{err}");

    handle.set_private_upstream(Some(Arc::new(FilePrivateUpstream::new(&path))));
    let probe = handle.probe_private().await.unwrap();
    assert_eq!(private_server.hits(), 1);
    assert_eq!(anthropic.hits(), 0, "the API must not have been called");
    let seen = private_server.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.uri, "/v1/messages");
    assert_eq!(seen.header("x-api-key"), Some("llama-key"));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    // The mock's answer names no model; a real server's does.
    assert_eq!(probe.model, None);
}

/// A private placeholder with nothing behind it is a failed request, never
/// a quiet turn on the user's subscription.
#[tokio::test]
async fn a_private_placeholder_with_no_private_model_never_falls_back_to_the_api() {
    let anthropic = start_upstream().await;
    let handle = AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder_for("i-0028", Route::Private);
    assert_eq!(handle.private_model(), None);

    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(anthropic.hits(), 0, "the API must not have been called");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("private model"), "{text}");
}

/// Revoking an environment takes its routes with it: they were the
/// placeholders' and the placeholders are gone, so a later chat of the same
/// name mints its own rather than inheriting one nobody can see.
#[tokio::test]
async fn revoking_an_environment_forgets_where_it_was_pointed() {
    let anthropic = start_upstream().await;
    let handle = AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder_for("i-0028", Route::Private);
    handle.revoke("i-0028");
    assert_eq!(handle.route_of(&placeholder), None);
    assert_eq!(
        handle.route_of(&handle.issue_placeholder("i-0028")),
        Some(Route::Anthropic)
    );
}

#[tokio::test]
async fn compression_is_declined_so_spend_stays_countable() {
    // The client offers gzip; the proxy must not let that offer reach the
    // upstream, because it scans the response bytes for usage counters as
    // they stream and cannot read them compressed. Found live: a turn
    // completed while every counter sat at zero.
    let upstream = start_upstream().await;
    let handle =
        AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real-api-key"))).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    let request = Request::builder()
        .method("POST")
        .uri(format!("{}/v1/messages", handle.base_url()))
        .header("authorization", format!("Bearer {placeholder}"))
        .header("accept-encoding", "gzip, deflate, br")
        .body(Full::new(Bytes::from_static(b"{}")))
        .unwrap();
    let response = client().request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.last();
    assert_eq!(seen.header("accept-encoding"), None);
}

#[tokio::test]
async fn an_unknown_token_is_refused_without_touching_the_upstream() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    handle.issue_placeholder("primary");

    let response = get(&handle, "/v1/messages", Some("sk-ant-taste-guessed")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.hits(), 0, "the upstream must not have been called");

    // And with no credential at all.
    let response = get(&handle, "/v1/messages", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.hits(), 0);

    // The two refusals are different kinds: a credential we never issued
    // is always a bug worth chasing; a bare request is the CLI's own
    // connectivity probe and merely gets turned away.
    assert_eq!(handle.unrecognized(), 1);
    assert_eq!(handle.unauthenticated(), 1);
    assert_eq!(handle.spend("primary").requests, 0);
}

#[tokio::test]
async fn revoking_an_environment_kills_its_placeholders() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let first = handle.issue_placeholder("agent-1");
    let second = handle.issue_placeholder("agent-1");
    let other = handle.issue_placeholder("agent-2");
    assert_ne!(first, second, "each placeholder is distinct");

    assert_eq!(
        get(&handle, "/v1/messages", Some(&first)).await.status(),
        StatusCode::OK
    );
    handle.revoke("agent-1");
    assert_eq!(
        get(&handle, "/v1/messages", Some(&first)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&handle, "/v1/messages", Some(&second)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    // Revocation is per environment, not global.
    assert_eq!(
        get(&handle, "/v1/messages", Some(&other)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn responses_stream_chunk_by_chunk() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    let started = Instant::now();
    let response = get(&handle, "/sse", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let mut arrivals = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if frame.data_ref().is_some() {
            arrivals.push(started.elapsed());
        }
    }

    assert_eq!(arrivals.len(), 3, "one frame per upstream event");
    // Buffering would have made all three arrive together, after 400ms+.
    assert!(
        arrivals[0] < Duration::from_millis(150),
        "first chunk waited {:?}",
        arrivals[0]
    );
    assert!(
        arrivals[2] >= Duration::from_millis(350),
        "last chunk arrived too early to have been streamed: {:?}",
        arrivals[2]
    );
    assert!(arrivals[0] < arrivals[1] && arrivals[1] < arrivals[2]);

    // The cumulative usage counter rode along with the stream.
    let spend = handle.spend("primary");
    assert_eq!(spend.requests, 1);
    assert_eq!(spend.output_tokens, 30);
}

#[tokio::test]
async fn spend_is_attributed_to_the_environment_that_spent_it() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let one = handle.issue_placeholder("agent-1");
    let two = handle.issue_placeholder("agent-2");

    for _ in 0..3 {
        let response = get(&handle, "/v1/messages", Some(&one)).await;
        let _ = response.into_body().collect().await.unwrap();
    }
    let response = get(&handle, "/v1/messages", Some(&two)).await;
    let _ = response.into_body().collect().await.unwrap();

    let first = handle.spend("agent-1");
    assert_eq!(first.requests, 3);
    assert_eq!(
        first.input_tokens, 33,
        "11 per request, cache reads excluded"
    );
    assert_eq!(first.output_tokens, 66);
    assert!(first.response_bytes > 0);

    let second = handle.spend("agent-2");
    assert_eq!(second.requests, 1);
    assert_eq!(second.input_tokens, 11);

    assert_eq!(handle.spend("never-issued"), Default::default());
}

/// The account's limit state arrives on responses the proxy was already
/// carrying, and the client still gets the headers unaltered.
#[tokio::test]
async fn quota_is_read_off_the_responses_that_pass_through() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    // Before any traffic there is nothing to know, and the proxy does not
    // go and find out.
    assert!(handle.quota().is_empty(), "no traffic, no snapshot");
    assert_eq!(upstream.hits(), 0);

    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The headers are the client's too — this reads the mail, it does not
    // hold it.
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-requests-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1000")
    );
    let _ = response.into_body().collect().await.unwrap();

    let now = std::time::SystemTime::now();
    let quota = handle.quota();
    assert!(!quota.is_empty());
    assert_eq!(quota.observed_for.as_deref(), Some("primary"));
    assert!(
        quota.age(now).unwrap() < Duration::from_secs(5),
        "the snapshot is stamped with when it was read"
    );
    assert_eq!(quota.requests.limit, Some(1000));
    assert_eq!(quota.input_tokens.utilization(), Some(0.2));
    assert_eq!(quota.session.used(), Some(0.27));
    assert_eq!(quota.weekly.used(), Some(0.61));
    assert_eq!(quota.session.status.as_deref(), Some("allowed"));

    // The plan window is what a gauge shows, not the per-minute bucket.
    let headline = quota.headline(now).unwrap();
    assert_eq!(headline.meter, taste_core::quota::Meter::Weekly);
    assert!(quota.current_exhaustion(now).is_none());
}

/// A refusal is the one reading that needs no interpretation, and its
/// message is the only thing read out of any body.
#[tokio::test]
async fn a_refusal_records_the_closed_window_and_the_next_turn_reopens_it() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    let response = get(&handle, "/limited", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // The refusal reaches the agent untouched — the proxy observes, it
    // does not swallow.
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("session limit"));

    let now = std::time::SystemTime::now();
    let quota = handle.quota();
    let refusal = quota.current_exhaustion(now).expect("a standing refusal");
    assert_eq!(refusal.retry_after, Some(Duration::from_secs(1800)));
    assert!(refusal.until.unwrap() > now);
    assert!(
        refusal
            .message
            .as_deref()
            .unwrap()
            .contains("session limit"),
        "{refusal:?}"
    );
    assert_eq!(quota.session.used(), Some(1.0));

    // A served response afterwards is proof the window reopened; nothing
    // was asked to learn that.
    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().collect().await.unwrap();
    let quota = handle.quota();
    assert!(quota.exhausted.is_none(), "the refusal was lifted");
    assert_eq!(quota.session.used(), Some(0.27), "and the window refreshed");
}

#[tokio::test]
async fn re_provisioning_rewrites_the_credential_file_and_the_proxy_follows() {
    let upstream = start_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("anthropic.json");
    let write = |token: &str| {
        std::fs::write(
            &path,
            format!(r#"{{"kind":"oauth_token","token":"{token}"}}"#),
        )
        .unwrap();
    };
    write("token-before-relogin");

    let credentials = Arc::new(FileCredentials::new(&path));
    let handle = AuthProxy::spawn(upstream.uri(), credentials.clone()).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(
        upstream.last().header("authorization"),
        Some("Bearer token-before-relogin")
    );

    std::thread::sleep(Duration::from_millis(10));
    write("token-after-relogin-and-longer");
    get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(
        upstream.last().header("authorization"),
        Some("Bearer token-after-relogin-and-longer")
    );
    // An OAuth credential goes out as a bearer token and never as a key.
    assert_eq!(upstream.last().header("x-api-key"), None);
}

#[tokio::test]
async fn a_credential_that_cannot_be_read_fails_the_request_not_the_proxy() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(
        upstream.uri(),
        Arc::new(FileCredentials::new("/nonexistent/anthropic.json")),
    )
    .unwrap();
    let placeholder = handle.issue_placeholder("primary");

    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.hits(), 0);

    // The proxy is still serving: a later request with a working
    // credential would go through, so a failed sign-in is recoverable
    // without restarting the IDE.
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("authentication_error") || text.contains("api_error"),
        "{text}"
    );
}

#[tokio::test]
async fn an_upstream_401_invalidates_the_cached_credential() {
    // A source that counts how often it is asked and notices invalidation.
    struct Counting {
        reads: AtomicU64,
        invalidated: AtomicU64,
    }
    impl CredentialSource for Counting {
        fn credential(&self) -> taste_authproxy::CredentialFuture<'_> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(taste_authproxy::Credential::ApiKey("k".into())) })
        }
        fn invalidate(&self) {
            self.invalidated.fetch_add(1, Ordering::Relaxed);
        }
    }

    // An upstream that always says 401.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(|_req: Request<Incoming>| async {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .body(BodyExt::boxed(Full::new(Bytes::from_static(b"{}"))))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let source = Arc::new(Counting {
        reads: AtomicU64::new(0),
        invalidated: AtomicU64::new(0),
    });
    let handle =
        AuthProxy::spawn(format!("http://{addr}").parse().unwrap(), source.clone()).unwrap();
    let placeholder = handle.issue_placeholder("primary");

    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(source.reads.load(Ordering::Relaxed), 1);
    assert_eq!(source.invalidated.load(Ordering::Relaxed), 1);
}

/// A request over the proxy's second door: a plain byte stream, standing in
/// for a connection an environment channel carried out of a container.
///
/// The far end really is a container in production. Here it is one half of
/// a duplex, which is the whole of what `serve_stream` promises to accept —
/// and the point: the proxy's policy must not depend on what carried the
/// bytes.
async fn get_over_stream(handle: &Handle, uri: &str, token: Option<&str>) -> Response<Incoming> {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    handle.serve_stream(theirs);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(ours))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "taste-ide.invalid");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    sender
        .send_request(builder.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap()
}

/// The transport is not the policy. A relocated agent reaches the proxy
/// over its environment channel instead of loopback, and everything the
/// loopback tests above assert has to hold there too — the gate, the header
/// swap, and the attribution.
#[tokio::test]
async fn the_stream_door_serves_the_same_proxy() {
    let upstream = start_upstream().await;
    let handle =
        AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real-api-key"))).unwrap();

    // A placeholder issued for a spawn works on whichever door that spawn
    // ends up using: one state, two doors.
    let placeholder = handle.issue_placeholder("review");
    let response = get_over_stream(&handle, "/v1/messages?beta=true", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.last();
    assert_eq!(seen.uri, "/v1/messages?beta=true");
    assert_eq!(seen.header("x-api-key"), Some("real-api-key"));
    assert_eq!(seen.header("authorization"), None);

    // Spend lands in the same counters, attributed to the same environment.
    let _ = response.into_body().collect().await.unwrap();
    let spend = handle.spend("review");
    assert_eq!(spend.requests, 1);
    assert_eq!(spend.input_tokens, 11);
}

#[tokio::test]
async fn the_stream_door_refuses_what_loopback_refuses() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();

    let response = get_over_stream(&handle, "/v1/messages", Some("sk-ant-taste-guessed")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = get_over_stream(&handle, "/v1/messages", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.hits(), 0, "the upstream must not have been called");
    assert_eq!(handle.unrecognized(), 1);
    assert_eq!(handle.unauthenticated(), 1);
}

/// Many connections at once, which is the reason the channel multiplexes at
/// all: hyper pools connections and an SSE turn holds one open, so the
/// relocated auth path is never one connection at a time.
#[tokio::test]
async fn the_stream_door_takes_more_than_one_at_a_time() {
    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("real"))).unwrap();
    let placeholder = handle.issue_placeholder("review");

    for _ in 0..4 {
        let response = get_over_stream(&handle, "/v1/messages", Some(&placeholder)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.into_body().collect().await.unwrap();
    }
    assert_eq!(handle.spend("review").requests, 4);
}

// --- the credential is the project's -----------------------------------

/// One state base for this test binary, and no machine-wide credential in
/// its environment.
///
/// `XDG_STATE_HOME` is what [`taste_authproxy::credential_path`] keys
/// under, so every test in here provisions its own workspace beneath one
/// base rather than racing the others over the variable — the workspaces
/// are told apart by the hash of their roots, which is the property under
/// test. The aimed path and the two documented environment variables are
/// cleared for the same reason a bench is cleared before a measurement:
/// what resolution does when nobody has aimed anything is exactly what
/// these tests are about.
fn isolate_state() {
    static BASE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", dir.path());
        std::env::remove_var("TASTE_ANTHROPIC_CREDENTIALS");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        std::env::remove_var("TASTE_PRIVATE_MODEL");
        dir
    });
}

/// Provision a project, writing the file exactly where the IDE looks for
/// it — which is under the state root, keyed by the checkout's path, and
/// never inside the checkout.
fn provision(root: &std::path::Path, name: &str, json: &str) {
    let path = taste_authproxy::credential_path(root).with_file_name(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, json).unwrap();
    assert!(
        !path.starts_with(root),
        "{} is in the checkout",
        path.display()
    );
}

/// **The issue this scope exists for.** Two projects on one machine hold
/// two credentials, each proxy sends its own, and a project with none is
/// refused rather than quietly served out of its neighbour's — which is
/// the very mechanism that would let a work key into a personal project
/// (David, 2026-09-16).
#[tokio::test]
async fn two_projects_resolve_two_credentials_and_neither_lends_to_the_other() {
    isolate_state();
    let upstream = start_upstream().await;
    let proxy_for = |root: &std::path::Path| {
        AuthProxy::spawn(upstream.uri(), Arc::new(IdeCredentials::new(root))).unwrap()
    };

    let work = std::path::Path::new("/projects/acme");
    let personal = std::path::Path::new("/projects/tinkering");
    let fresh = std::path::Path::new("/projects/brand-new");
    provision(
        work,
        "anthropic.json",
        r#"{"kind":"oauth_token","token":"work-token","label":"work"}"#,
    );
    provision(
        personal,
        "anthropic.json",
        r#"{"kind":"api_key","token":"personal-key","label":"personal"}"#,
    );

    let at_work = proxy_for(work);
    let placeholder = at_work.issue_placeholder("primary");
    get(&at_work, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(
        upstream.last().header("authorization"),
        Some("Bearer work-token")
    );
    assert_eq!(at_work.credential_label().as_deref(), Some("work"));

    let at_home = proxy_for(personal);
    let placeholder = at_home.issue_placeholder("primary");
    get(&at_home, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(upstream.last().header("x-api-key"), Some("personal-key"));
    assert_eq!(
        upstream.last().header("authorization"),
        None,
        "the other project's credential is not even in the running"
    );
    assert_eq!(at_home.credential_label().as_deref(), Some("personal"));

    // The third project is the assertion that matters: unprovisioned, with
    // two provisioned neighbours under the same state root, a machine-wide
    // file of the old shape sitting right there, and nothing reaching the
    // API on its behalf. The machine file is the sharp end of it — that is
    // the fallback this scope exists to not have, and the file nothing
    // reads any more, not even to offer it.
    let machine = std::path::PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap())
        .join("taste-ide/anthropic.json");
    std::fs::create_dir_all(machine.parent().unwrap()).unwrap();
    std::fs::write(
        &machine,
        r#"{"kind":"api_key","token":"machine-wide-key","label":"left over"}"#,
    )
    .unwrap();
    let forwarded = upstream.hits();
    let unprovisioned = proxy_for(fresh);
    let placeholder = unprovisioned.issue_placeholder("primary");
    let response = get(&unprovisioned, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.hits(), forwarded, "nothing was forwarded");
    assert_eq!(unprovisioned.credential_label(), None);

    // ...and the refusal names THIS project's file, which is the one the
    // user has to write, rather than a machine-wide one they might expect
    // to have covered it.
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    let expected = taste_authproxy::credential_path(fresh);
    assert!(text.contains(&expected.display().to_string()), "{text}");
    assert!(text.contains("setup-token"), "{text}");

    // ...and no offer of the machine-wide file is a thing this crate can
    // make: the only way that project gets a credential is a file of its
    // own, written by the user (David, 2026-09-16: "Never offer to import
    // system credentials into a project").
    assert!(
        machine.exists(),
        "the leftover file is left exactly where it was"
    );
    assert!(!expected.exists(), "and nothing copied it into the project");
}

/// The private model scopes the same way, and for the same reason: a
/// server on the user's own hardware is a thing they chose for this work.
#[tokio::test]
async fn the_private_model_file_is_this_projects_too() {
    isolate_state();
    let with_a_server = std::path::Path::new("/projects/has-a-tower");
    let without = std::path::Path::new("/projects/has-none");
    provision(
        with_a_server,
        "private-model.json",
        r#"{"base_url":"http://tower.lan:9931","token":"k","model":"gpt-oss-20b"}"#,
    );

    let source = taste_authproxy::private::discover(with_a_server).expect("this project has one");
    assert_eq!(
        source.upstream().await.unwrap().uri.to_string(),
        "http://tower.lan:9931/"
    );
    assert_eq!(source.facts().unwrap().label, "gpt-oss-20b");

    // The neighbouring project has no private model, and does not inherit
    // one: opening Claude Code (Private) there fails the request rather
    // than reaching somebody else's server.
    assert!(
        taste_authproxy::private::discover(without).is_none(),
        "a private model is provisioned per project, never machine-wide"
    );
    assert_ne!(
        taste_authproxy::private_model_path(with_a_server),
        taste_authproxy::private_model_path(without)
    );

    let upstream = start_upstream().await;
    let handle = AuthProxy::spawn(upstream.uri(), Arc::new(StaticKey::api_key("k"))).unwrap();
    handle.set_private_upstream(taste_authproxy::private::discover(without).map(Arc::new));
    let placeholder = handle.issue_placeholder_for("primary", Route::Private);
    let response = get(&handle, "/v1/messages", Some(&placeholder)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.hits(), 0, "and nothing fell back to the API");
}
