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
use taste_authproxy::{AuthProxy, CredentialSource, FileCredentials, Handle, StaticKey};

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
