//! The loopback listener, the placeholder gate, and the streaming forward.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, HOST};
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use sha2::{Digest, Sha256};
use taste_core::quota::QuotaSnapshot;

use crate::credentials::{Credential, CredentialSource, X_API_KEY};
use crate::models::ModelListing;
use crate::quota::{attach_refusal_message, harvest, MAX_REFUSAL_BODY};

/// The API the proxy fronts when nothing else is configured.
pub const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// Placeholders are recognisable on sight in an `env` dump, and shaped
/// enough like a key that a client sniffing for a prefix is satisfied.
const PLACEHOLDER_PREFIX: &str = "sk-ant-taste-";

/// A hung upstream must not hang a chat forever. Generous, because a
/// non-streaming completion legitimately takes minutes; it bounds the wait
/// for *headers*, after which the body streams for as long as it likes.
const HEADERS_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Headers that describe one hop and must not be copied to the next.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// What one environment has spent through the proxy.
///
/// Phase 1 records; it does not enforce. Token counts come from the
/// Messages API's own `usage` object as it streams past — attribution the
/// user can see, and the shape a future limit would be checked against.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Spend {
    /// Requests admitted and forwarded upstream.
    pub requests: u64,
    /// Bytes of response body streamed back.
    pub response_bytes: u64,
    /// Largest `usage.input_tokens` seen, summed over requests.
    pub input_tokens: u64,
    /// Largest `usage.output_tokens` seen, summed over requests.
    pub output_tokens: u64,
}

impl Spend {
    fn add_response(&mut self, bytes: u64, input: u64, output: u64) {
        self.response_bytes += bytes;
        self.input_tokens += input;
        self.output_tokens += output;
    }
}

struct ProxyState {
    upstream: Uri,
    credentials: Arc<dyn CredentialSource>,
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming>,
    /// Placeholder token → environment id. Many live tokens may map to one
    /// environment (a chat respawning does not invalidate its siblings);
    /// [`Handle::revoke`] drops all of an environment's at once.
    tokens: Mutex<HashMap<String, String>>,
    spend: Mutex<HashMap<String, Spend>>,
    /// The account's limit state, as the last response described it.
    ///
    /// One snapshot, not one per environment: the subscription is a
    /// single pool that the whole fleet and the user's own interactive
    /// use draw on. `spend` above is the breakdown of who drew.
    quota: Mutex<QuotaSnapshot>,
    /// The account's model listing, once [`Handle::refresh_models`] has
    /// read it (from the cache first, then from the API). `None` is "not
    /// asked yet, or not answered", never an empty account.
    models: Mutex<Option<Vec<ModelListing>>>,
    /// Secret, process-random, and the only entropy placeholders need: a
    /// token is `sha256(seed || counter || env)`, so issuing one cannot
    /// fail the way a fresh RNG read can.
    seed: [u8; 32],
    counter: AtomicU64,
    unauthenticated: AtomicU64,
    unrecognized: AtomicU64,
}

impl ProxyState {
    fn env_for(&self, token: &str) -> Option<String> {
        self.tokens.lock().ok()?.get(token).cloned()
    }

    fn record_request(&self, env: &str) {
        if let Ok(mut spend) = self.spend.lock() {
            spend.entry(env.to_string()).or_default().requests += 1;
        }
    }

    fn record_response(&self, env: &str, bytes: u64, input: u64, output: u64) {
        if let Ok(mut spend) = self.spend.lock() {
            spend
                .entry(env.to_string())
                .or_default()
                .add_response(bytes, input, output);
        }
    }

    /// File what this response said about the account's limits.
    ///
    /// Headers only, and only ones we were already receiving. Nothing is
    /// requested to learn this — see [`crate::quota`] for why that
    /// constraint is the design and not a shortcoming.
    fn record_quota(&self, status: StatusCode, headers: &HeaderMap, env: &str) {
        let observed_at = SystemTime::now();
        let fresh = harvest(status, headers, observed_at, env);
        let served = status.is_success();
        if fresh.is_none() && !served {
            return;
        }
        if let Ok(mut quota) = self.quota.lock() {
            quota.observe(fresh, served);
        }
    }

    /// Attach the API's own words to a refusal already recorded.
    fn record_refusal_message(&self, body: &[u8]) {
        if let Ok(mut quota) = self.quota.lock() {
            attach_refusal_message(&mut quota, body);
        }
    }
}

/// A running proxy. Dropping the last clone stops the listener.
#[derive(Clone)]
pub struct Handle {
    addr: SocketAddr,
    state: Arc<ProxyState>,
    _accept: Arc<AbortOnDrop>,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Handle {
    /// What to put in `ANTHROPIC_BASE_URL`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Mint a placeholder credential for one environment.
    ///
    /// The agent gets this in `ANTHROPIC_AUTH_TOKEN`; it is worthless off
    /// this loopback port, and identifies the spender when it comes back.
    pub fn issue_placeholder(&self, env_id: &str) -> String {
        let counter = self.state.counter.fetch_add(1, Ordering::Relaxed);
        let mut hasher = Sha256::new();
        hasher.update(self.state.seed);
        hasher.update(counter.to_le_bytes());
        hasher.update(env_id.as_bytes());
        let digest = hasher.finalize();
        let token: String = digest
            .iter()
            .take(20)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .concat();
        let token = format!("{PLACEHOLDER_PREFIX}{token}");
        if let Ok(mut tokens) = self.state.tokens.lock() {
            tokens.insert(token.clone(), env_id.to_string());
        }
        token
    }

    /// Revoke every placeholder issued to an environment. Requests bearing
    /// them are refused from the next one on, with no upstream call.
    pub fn revoke(&self, env_id: &str) {
        if let Ok(mut tokens) = self.state.tokens.lock() {
            tokens.retain(|_, env| env != env_id);
        }
    }

    /// The account's limit state as of the last response that mentioned it.
    ///
    /// Workspace-global, and always historical: the snapshot carries the
    /// moment it was read, and a caller that renders it without saying
    /// when is claiming a liveness this proxy cannot have. Empty until
    /// the first turn — there is nothing to know before any traffic, and
    /// nothing here will generate traffic to find out.
    pub fn quota(&self) -> QuotaSnapshot {
        self.state
            .quota
            .lock()
            .ok()
            .map(|quota| quota.clone())
            .unwrap_or_default()
    }

    /// The account's model listing, as last read. A pure read, for the
    /// GTK thread: nothing here asks the API.
    pub fn models(&self) -> Option<Vec<ModelListing>> {
        self.state
            .models
            .lock()
            .ok()
            .and_then(|models| models.clone())
    }

    /// The most capable model above Opus the account can run, if the
    /// listing has been read and names one — see [`crate::models::top_tier`].
    pub fn top_tier_model(&self) -> Option<ModelListing> {
        let models = self.state.models.lock().ok()?;
        crate::models::top_tier(models.as_deref()?).cloned()
    }

    /// Read the account's models: the cache at `cache` first, so a spawn
    /// that comes seconds after launch has last time's answer, then the
    /// API, whose answer replaces it and is written back. Explicit rather
    /// than part of `spawn`, so a proxy stood up for a test or a tool that
    /// never needs the list never makes the request. Must be called within
    /// a tokio runtime context; the work runs on it and this returns at
    /// once.
    pub fn refresh_models(&self, cache: Option<PathBuf>) {
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Some(path) = &cache {
                let path = path.clone();
                let cached = tokio::task::spawn_blocking(move || crate::models::load_cached(&path))
                    .await
                    .ok()
                    .flatten();
                if let Some(models) = cached {
                    if let Ok(mut slot) = state.models.lock() {
                        slot.get_or_insert(models);
                    }
                }
            }
            match fetch_models(&state).await {
                Ok(models) => {
                    if let Ok(mut slot) = state.models.lock() {
                        *slot = Some(models.clone());
                    }
                    if let Some(path) = cache {
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Err(e) = crate::models::store_cached(&path, &models) {
                                tracing::warn!("could not cache the model listing: {e}");
                            }
                        })
                        .await;
                    }
                }
                Err(e) => tracing::warn!("the account's model listing could not be read: {e}"),
            }
        });
    }

    /// What this environment has spent so far.
    pub fn spend(&self, env_id: &str) -> Spend {
        self.state
            .spend
            .lock()
            .ok()
            .and_then(|spend| spend.get(env_id).copied())
            .unwrap_or_default()
    }

    /// Requests refused because they carried no credential at all.
    ///
    /// Benign in practice: the CLI probes its base URL (`HEAD /api/hello`,
    /// observed live) before authenticating anything. The refusal is still
    /// correct — nothing unauthenticated is ever forwarded — but this
    /// count is expected to be non-zero in normal operation.
    pub fn unauthenticated(&self) -> u64 {
        self.state.unauthenticated.load(Ordering::Relaxed)
    }

    /// Requests refused because they *presented* a credential this proxy
    /// never issued. Always a bug worth chasing: either the placeholder
    /// plumbing is broken, or something else entirely is talking to the
    /// port.
    pub fn unrecognized(&self) -> u64 {
        self.state.unrecognized.load(Ordering::Relaxed)
    }

    /// Serve this same proxy on a byte stream somebody else accepted.
    ///
    /// The proxy's second door, and the only one that is not its loopback
    /// port. A relocated agent is inside a network namespace of the repo's
    /// choosing, where that port does not exist; what reaches it there is a
    /// connection its environment's channel carried out of the container
    /// (`taste_devcontainer::channel`). This proxy neither knows nor cares
    /// which — it is handed something that reads and writes, and speaks
    /// HTTP over it.
    ///
    /// There was a `listen_unix` here that bound a unix socket for the
    /// container to dial. It is gone: on an SELinux-enforcing host a
    /// `container_t` process may not `connectto` a socket the unconfined
    /// IDE bound, so that door was shut on exactly the hosts it was built
    /// for, and keeping a second mechanism that only works some of the time
    /// is worse than having one that always does.
    ///
    /// Same [`ProxyState`], deliberately: a placeholder minted for a spawn
    /// is valid whichever door that spawn ends up using, and spend lands in
    /// one set of counters however it arrived.
    pub fn serve_stream<S>(&self, stream: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        tokio::spawn(serve_connection(stream, self.state.clone()));
    }
}

pub struct AuthProxy;

impl AuthProxy {
    /// Bind 127.0.0.1 on an ephemeral port and serve until the handle drops.
    ///
    /// Must be called from within a tokio runtime context. The bind itself
    /// is synchronous so the port is known before this returns — the caller
    /// is composing an agent's environment and cannot wait.
    pub fn spawn(upstream: Uri, credentials: Arc<dyn CredentialSource>) -> Result<Handle> {
        anyhow::ensure!(
            upstream.authority().is_some(),
            "auth proxy upstream {upstream} has no host"
        );

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("binding the auth proxy's loopback port")?;
        listener
            .set_nonblocking(true)
            .context("auth proxy listener")?;
        let addr = listener.local_addr().context("auth proxy listener")?;

        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).context("seeding the auth proxy's placeholder tokens")?;

        let state = Arc::new(ProxyState {
            upstream,
            credentials,
            client: build_client(),
            tokens: Mutex::new(HashMap::new()),
            spend: Mutex::new(HashMap::new()),
            quota: Mutex::new(QuotaSnapshot::default()),
            models: Mutex::new(None),
            seed,
            counter: AtomicU64::new(0),
            unauthenticated: AtomicU64::new(0),
            unrecognized: AtomicU64::new(0),
        });

        let accept_state = state.clone();
        let accept = tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::error!("auth proxy listener: {e}");
                    return;
                }
            };
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let state = accept_state.clone();
                        // Nagle off: SSE frames are small and latency here
                        // is latency the user watches token by token.
                        let _ = stream.set_nodelay(true);
                        tokio::spawn(serve_connection(stream, state));
                    }
                    Err(e) => {
                        tracing::warn!("auth proxy accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });

        Ok(Handle {
            addr,
            state,
            _accept: Arc::new(AbortOnDrop(accept)),
        })
    }
}

/// `GET /v1/models` with the real credential — the one request the proxy
/// makes for itself. The same upstream, the same credential application
/// and the same `anthropic-version` a forwarded request carries; an OAuth
/// credential adds the beta the API documents for bearer tokens.
async fn fetch_models(state: &ProxyState) -> Result<Vec<ModelListing>> {
    let credential = state
        .credentials
        .credential()
        .await
        .context("no usable credential")?;
    let requested: Uri = "/v1/models?limit=1000".parse().expect("a static path");
    let uri = upstream_uri(&state.upstream, &requested)?;
    let mut request = Request::get(uri)
        .header("anthropic-version", "2023-06-01")
        .body(http_body_util::Empty::<Bytes>::new())
        .context("composing the models request")?;
    if matches!(credential, Credential::OAuth(_)) {
        request.headers_mut().insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
    }
    credential.apply(request.headers_mut());
    let client = build_client::<http_body_util::Empty<Bytes>>();
    let response = tokio::time::timeout(CONNECT_TIMEOUT * 2, client.request(request))
        .await
        .context("the Models API did not answer in time")?
        .context("reaching the Models API")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("reading the Models API response")?
        .to_bytes();
    anyhow::ensure!(
        status.is_success(),
        "the Models API answered {status}: {}",
        String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>()
    );
    crate::models::parse_models(&body)
}

/// Generic over the body: forwarded requests stream the agent's `Incoming`
/// through; the proxy's own one request (the models listing) sends none.
fn build_client<B>() -> Client<hyper_rustls::HttpsConnector<HttpConnector>, B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // rustls, never openssl (Flatpak). Installing the provider is
    // idempotent and races benignly with any other caller.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.enforce_http(false);
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new()).build(https)
}

/// Serve one connection.
///
/// Generic over the transport on purpose: Phase 4 relocates the agent into
/// containers that may not share the host network namespace, and the
/// answer there is a bind-mounted `UnixListener`. Its accept loop hands
/// `UnixStream`s to exactly this function — nothing else has to change.
async fn serve_connection<S>(stream: S, state: Arc<ProxyState>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let service = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        async move { Ok::<_, std::convert::Infallible>(handle(req, state).await) }
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        // Client hang-ups are normal (a cancelled turn closes the socket).
        tracing::debug!("auth proxy connection ended: {e}");
    }
}

async fn handle(req: Request<Incoming>, state: Arc<ProxyState>) -> Response<ProxyBody> {
    let Some(presented) = presented_token(req.headers()) else {
        state.unauthenticated.fetch_add(1, Ordering::Relaxed);
        return error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "no credential presented to the taste-ide auth proxy",
        );
    };
    let Some(env_id) = state.env_for(&presented) else {
        // Deliberately before anything else: an unknown token costs the
        // user nothing, reaches no network, and reveals nothing about
        // whether a credential is even configured.
        state.unrecognized.fetch_add(1, Ordering::Relaxed);
        return error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "credential not issued by the taste-ide auth proxy",
        );
    };

    let credential = match state.credentials.credential().await {
        Ok(credential) => credential,
        Err(e) => {
            tracing::warn!("auth proxy has no usable credential: {e}");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("taste-ide auth proxy has no usable credential: {e}"),
            );
        }
    };

    let (mut parts, body) = req.into_parts();
    let uri = match upstream_uri(&state.upstream, &parts.uri) {
        Ok(uri) => uri,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("taste-ide auth proxy could not rewrite the request URI: {e}"),
            )
        }
    };
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove(HOST);
    // Identity encoding, deliberately: the client offers gzip, hyper does
    // not decompress mid-stream, and the spend scanner reads the bytes as
    // they pass — compressed usage counters read as no usage at all
    // (found live: a completed turn, counters at zero). A proxy that
    // accounts for what it carries asks the upstream not to compress.
    parts.headers.remove(http::header::ACCEPT_ENCODING);
    credential.apply(&mut parts.headers);
    parts.uri = uri;
    // The body is forwarded as-is: `Incoming` is a stream, so an upload
    // (or a long prompt) never lands in memory here.
    let outbound = Request::from_parts(parts, body);

    state.record_request(&env_id);
    let sent = tokio::time::timeout(HEADERS_TIMEOUT, state.client.request(outbound)).await;
    let upstream = match sent {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            tracing::warn!("auth proxy upstream request failed: {e}");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("taste-ide auth proxy could not reach the API: {e}"),
            );
        }
        Err(_) => {
            return error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "taste-ide auth proxy timed out waiting for the API",
            )
        }
    };

    if upstream.status() == StatusCode::UNAUTHORIZED || upstream.status() == StatusCode::FORBIDDEN {
        // The stored credential is stale (expired, or rotated by a
        // re-login). Drop the cache so the next request re-reads.
        state.credentials.invalidate();
    }

    // What the account said about itself, on the way past. Before the
    // hop-by-hop strip only in the sense that it does not matter: none of
    // these are hop-scoped, and the client gets them either way — this
    // proxy reads the mail, it does not intercept it.
    state.record_quota(upstream.status(), upstream.headers(), &env_id);

    let (mut parts, body) = upstream.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    let metered = MeteredBody {
        inner: body,
        state: state.clone(),
        env: env_id,
        usage: UsageScan::default(),
        bytes: 0,
        flushed: false,
        // A quota refusal names the window it closed and when it
        // reopens, and only the body says so. Bounded, and never kept
        // for any other status.
        refusal: (parts.status == StatusCode::TOO_MANY_REQUESTS).then(Vec::new),
    };
    Response::from_parts(parts, BodyExt::boxed(metered))
}

/// The placeholder, from either header the agent might have used.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let token = value.strip_prefix("Bearer ").unwrap_or(value).trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    let key = headers.get(X_API_KEY).and_then(|v| v.to_str().ok())?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // `Connection: x, y` names further headers that are hop-scoped.
    let named: Vec<String> = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    for name in HOP_BY_HOP.iter().map(|n| n.to_string()).chain(named) {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// Point the agent's request at the real API, preserving path and query.
fn upstream_uri(upstream: &Uri, requested: &Uri) -> Result<Uri> {
    let path = requested
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let base = upstream.path().trim_end_matches('/');
    let joined = if base.is_empty() {
        path.to_string()
    } else {
        format!("{base}{path}")
    };
    let mut parts = upstream.clone().into_parts();
    parts.path_and_query = Some(joined.parse().context("path and query")?);
    Uri::from_parts(parts).context("composing the upstream URI")
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response<ProxyBody> {
    // The Anthropic error envelope, so the adapter renders it as an API
    // error instead of a parse failure.
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    })
    .to_string();
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(BodyExt::boxed(
            Full::new(Bytes::from(body)).map_err(|never| match never {}),
        ))
        .expect("static error response")
}

/// The upstream body, passed through frame by frame while counting.
///
/// Nothing is buffered and nothing is logged: each frame is measured, its
/// bytes scanned for the Messages API's `usage` counters, and handed on.
struct MeteredBody {
    inner: Incoming,
    state: Arc<ProxyState>,
    env: String,
    usage: UsageScan,
    bytes: u64,
    flushed: bool,
    /// A refusal body, accumulated only for a 429 and only up to
    /// [`MAX_REFUSAL_BODY`]. `None` for every other response, which is
    /// every response that streams.
    refusal: Option<Vec<u8>>,
}

impl MeteredBody {
    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        self.state
            .record_response(&self.env, self.bytes, self.usage.input, self.usage.output);
        if let Some(body) = self.refusal.take() {
            self.state.record_refusal_message(&body);
        }
    }
}

/// Enough to bridge `"cache_read_input_tokens":1234567,` style splits.
const TAIL_BYTES: usize = 48;

/// Streaming search for the Messages API's `usage` counters.
///
/// Chunk boundaries fall wherever the network put them, so each chunk is
/// scanned twice: once bridged onto the tail of the previous one, once on
/// its own. Double-counting is harmless — both fields take a maximum.
#[derive(Default)]
struct UsageScan {
    tail: Vec<u8>,
    input: u64,
    output: u64,
}

impl UsageScan {
    fn feed(&mut self, chunk: &[u8]) {
        if self.tail.is_empty() {
            scan_usage(chunk, &mut self.input, &mut self.output);
            self.keep_tail(chunk);
            return;
        }
        // The carried tail joined to this chunk. The tail is what makes
        // byte-at-a-time delivery work at all: it *is* the sliding window,
        // so it has to be the last bytes of the joined stream, not the
        // last bytes of the last chunk.
        let mut bridge = std::mem::take(&mut self.tail);
        bridge.extend_from_slice(&chunk[..chunk.len().min(TAIL_BYTES)]);
        scan_usage(&bridge, &mut self.input, &mut self.output);
        if chunk.len() > TAIL_BYTES {
            scan_usage(chunk, &mut self.input, &mut self.output);
            self.keep_tail(chunk);
        } else {
            self.keep_tail(&bridge);
        }
    }

    fn keep_tail(&mut self, bytes: &[u8]) {
        self.tail = bytes[bytes.len().saturating_sub(TAIL_BYTES)..].to_vec();
    }
}

impl Body for MeteredBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.len() as u64;
                    let data = data.clone();
                    this.usage.feed(&data);
                    if let Some(refusal) = this.refusal.as_mut() {
                        let room = MAX_REFUSAL_BODY.saturating_sub(refusal.len());
                        refusal.extend_from_slice(&data[..data.len().min(room)]);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.flush();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.flush();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for MeteredBody {
    /// A cancelled turn drops the body mid-stream; what was spent still was.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Pull `usage.input_tokens` / `usage.output_tokens` out of a chunk.
///
/// `output_tokens` is cumulative across `message_delta` events, so the
/// largest value seen is the total; `input_tokens` appears once. The
/// leading quote in the needle is what keeps `cache_read_input_tokens`
/// from being mistaken for `input_tokens`.
fn scan_usage(haystack: &[u8], input: &mut u64, output: &mut u64) {
    max_field(haystack, b"\"input_tokens\":", input);
    max_field(haystack, b"\"output_tokens\":", output);
}

fn max_field(haystack: &[u8], needle: &[u8], out: &mut u64) {
    if haystack.len() < needle.len() {
        return;
    }
    let mut from = 0;
    while let Some(offset) = haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let at = from + offset;
        from = at + needle.len();
        // Only a field of its own object, never the tail of a longer name.
        if at == 0 || !matches!(haystack[at - 1], b'{' | b',' | b' ') {
            continue;
        }
        if let Some(value) = parse_u64(&haystack[from..]) {
            *out = (*out).max(value);
        }
    }
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    let mut digits = 0;
    for byte in bytes {
        match byte {
            b' ' if digits == 0 => continue,
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add((byte - b'0') as u64)?;
                digits += 1;
            }
            _ => break,
        }
    }
    (digits > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_counters_survive_a_chunk_boundary() {
        let event = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":4321,"cache_creation_input_tokens":0,"cache_read_input_tokens":99999,"output_tokens":1}}}

"#;
        let mut input = 0;
        let mut output = 0;
        scan_usage(event, &mut input, &mut output);
        // cache_read_input_tokens must not be mistaken for input_tokens.
        assert_eq!(input, 4321);
        assert_eq!(output, 1);

        // The same bytes delivered one at a time still land, thanks to the
        // bridging tail.
        let mut scan = UsageScan::default();
        for byte in event.iter() {
            scan.feed(&[*byte]);
        }
        assert_eq!(scan.input, 4321);
        assert_eq!(scan.output, 1);
    }

    #[test]
    fn output_tokens_take_the_largest_delta() {
        let mut input = 0;
        let mut output = 0;
        scan_usage(
            br#"{"usage":{"output_tokens":12}}"#,
            &mut input,
            &mut output,
        );
        scan_usage(
            br#"{"usage":{"output_tokens":97}}"#,
            &mut input,
            &mut output,
        );
        scan_usage(
            br#"{"usage":{"output_tokens":40}}"#,
            &mut input,
            &mut output,
        );
        assert_eq!(output, 97);
    }

    #[test]
    fn the_upstream_uri_keeps_path_and_query() {
        let upstream: Uri = "https://api.anthropic.com".parse().unwrap();
        let requested: Uri = "/v1/messages?beta=true".parse().unwrap();
        assert_eq!(
            upstream_uri(&upstream, &requested).unwrap().to_string(),
            "https://api.anthropic.com/v1/messages?beta=true"
        );

        // A base URL with a path prefix keeps it.
        let prefixed: Uri = "https://gateway.example/anthropic/".parse().unwrap();
        assert_eq!(
            upstream_uri(&prefixed, &requested).unwrap().to_string(),
            "https://gateway.example/anthropic/v1/messages?beta=true"
        );
    }

    #[test]
    fn connection_named_headers_are_stripped_too() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, X-Hop".parse().unwrap(),
        );
        headers.insert("x-hop", "1".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        strip_hop_by_hop(&mut headers);
        assert!(headers.get("x-hop").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get(http::header::CONNECTION).is_none());
        assert!(headers.get("anthropic-version").is_some());
    }

    #[test]
    fn a_bearer_or_an_api_key_both_present_a_token() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(presented_token(&headers).as_deref(), Some("abc"));

        let mut headers = HeaderMap::new();
        headers.insert(X_API_KEY, "def".parse().unwrap());
        assert_eq!(presented_token(&headers).as_deref(), Some("def"));

        assert_eq!(presented_token(&HeaderMap::new()), None);
    }
}
