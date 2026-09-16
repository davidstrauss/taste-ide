//! `issue_start` with the private value produces a chat whose placeholder
//! is routed privately.
//!
//! The sentence is the gate's, and this is as much of it as a headless
//! test can hold. What `issue_start` does with a `model` is hand it to the
//! chat strip, which hands it to the pane, which calls exactly the two
//! functions below — [`taste_acp::authproxy::route_for_model`] and
//! [`taste_acp::authproxy::Handle::set_route`] — against the environment
//! the placeholder is minted for. So this stands a real proxy up in front
//! of two mock upstreams, does to it precisely what `ChatPane::apply_route`
//! does, and asks the one question that matters: where did the turn go.
//!
//! The widget hop is the only thing not covered, and it is covered by
//! construction: `ChatPane::apply_route` has no branch of its own, and
//! `ChatPane::set_model_value` — the function `issue_start` reaches
//! through `chats::create_orchestrated` — calls it before it stores
//! anything, so the route is set before the chat's first request rather
//! than at its first Ready.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use taste_acp::authproxy::{Route, PRIVATE_MODEL_VALUE};
use taste_authproxy::{AuthProxy, FilePrivateUpstream, StaticKey};

/// One mock Messages API, remembering the auth header of the last request.
#[derive(Clone)]
struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<(String, String)>>>,
    hits: Arc<AtomicU64>,
}

impl Server {
    fn uri(&self) -> Uri {
        format!("http://{}", self.addr).parse().unwrap()
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    fn header(&self, name: &str) -> Option<String> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

async fn start_server() -> Server {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicU64::new(0));
    let accept_seen = seen.clone();
    let accept_hits = hits.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = accept_seen.clone();
            let hits = accept_hits.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let seen = seen.clone();
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::Relaxed);
                        let mut recorded = seen.lock().unwrap();
                        for (name, value) in req.headers() {
                            recorded.push((
                                name.as_str().to_string(),
                                value.to_str().unwrap_or_default().to_string(),
                            ));
                        }
                        let body: BoxBody<Bytes, Infallible> =
                            BodyExt::boxed(Full::new(Bytes::from_static(
                                br#"{"type":"message","usage":{"output_tokens":1}}"#,
                            )));
                        Ok::<_, Infallible>(Response::new(body))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Server { addr, seen, hits }
}

#[tokio::test]
async fn a_chat_started_on_the_private_value_spends_on_the_private_server() {
    let anthropic = start_server().await;
    let private_server = start_server().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private-model.json");
    std::fs::write(
        &file,
        format!(
            r#"{{"base_url":"{}","token":"llama-key","model":"gpt-oss-20b"}}"#,
            private_server.uri()
        ),
    )
    .unwrap();

    let proxy =
        AuthProxy::spawn(anthropic.uri(), Arc::new(StaticKey::oauth("account-token"))).unwrap();
    proxy.set_private_upstream(Some(Arc::new(FilePrivateUpstream::new(&file))));

    // What `issue_start {"issue": "i-0028", "model": "private"}` amounts
    // to by the time it reaches the proxy: one environment, one
    // placeholder, and the route its model implies.
    let environment = "i-0028";
    let placeholder = proxy.issue_placeholder(environment);
    proxy.set_route(
        environment,
        taste_acp::authproxy::route_for_model(Some(PRIVATE_MODEL_VALUE)),
    );

    let response = Client::builder(TokioExecutor::new())
        .build(HttpConnector::new())
        .request(
            Request::builder()
                .method("POST")
                .uri(format!("{}/v1/messages", proxy.base_url()))
                .header("authorization", format!("Bearer {placeholder}"))
                .body(Full::new(Bytes::from_static(br#"{"model":"claude"}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.into_body().collect().await.unwrap();

    assert_eq!(
        private_server.hits(),
        1,
        "the turn went to the private model"
    );
    assert_eq!(anthropic.hits(), 0, "and nowhere near the API");
    assert_eq!(
        private_server.header("x-api-key").as_deref(),
        Some("llama-key"),
        "the private server's own key, in the header its file named"
    );
    assert_eq!(
        private_server.header("authorization"),
        None,
        "the account's token must not reach a host that is not Anthropic's"
    );
    // The chat is still that environment's, and its spend is still filed
    // under it — the free rung is a different upstream, not a different
    // spender.
    assert_eq!(proxy.spend(environment).requests, 1);
}

/// The other half of the same rule: every value that is not the private
/// one — including the agent's own default, which is no value at all —
/// stays on the API. A chat reaches the user's hardware because somebody
/// named it.
#[test]
fn every_other_model_value_stays_on_the_api() {
    assert_eq!(
        taste_acp::authproxy::route_for_model(Some(PRIVATE_MODEL_VALUE)),
        Route::Private
    );
    for value in [None, Some("opus[1m]"), Some("sonnet"), Some("")] {
        assert_eq!(
            taste_acp::authproxy::route_for_model(value),
            Route::Anthropic,
            "{value:?}"
        );
    }
}
