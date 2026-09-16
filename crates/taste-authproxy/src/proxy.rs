//! The loopback listener, the placeholder gate, and the streaming forward.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use taste_core::quota::QuotaSnapshot;

use crate::credentials::{Credential, CredentialSource, X_API_KEY};
use crate::models::ModelListing;
use crate::private::{FilePrivateUpstream, PrivateFacts};
use crate::quota::{attach_refusal_message, harvest, MAX_REFUSAL_BODY};

/// The API the proxy fronts when nothing else is configured.
pub const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// Which upstream an environment's placeholders reach.
///
/// Which upstream a placeholder reaches: the API, or the user's own
/// private server ([`crate::private`]).
///
/// A property of the **placeholder**, fixed when it is minted
/// ([`Handle::issue_placeholder_for`]) and never changed afterwards. The
/// spawn that mints it is an agent's — "Claude Code" gets an Anthropic
/// placeholder, "Claude Code (Private)" a private one — so which host a
/// chat spends on is decided by which agent it was opened as, and two
/// chats in one environment can be on different hosts at once. It used to
/// be a per-environment setting flipped from the model picker, which made
/// the private model look like a model of Claude Code's when it is a
/// different place for the same agent to send its requests.
///
/// [`Route::Anthropic`] is the default and the only value a placeholder
/// minted without saying gets: the private upstream is reached because a
/// placeholder was minted for it, never because something was absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    #[default]
    Anthropic,
    Private,
}

impl Route {
    pub fn is_private(self) -> bool {
        matches!(self, Route::Private)
    }
}

/// A sentence about a private server being woken, and which
/// environment's turn it concerns (`None` for the settings form's test).
pub type Notice = Arc<dyn Fn(Option<&str>, String) + Send + Sync>;

/// Where the proxy says the account's model listing changed: the newest
/// model above Opus the credential can run, or `None` when there is none
/// — see [`Handle::set_models_listener`].
pub type ModelsListener = Arc<dyn Fn(Option<ModelListing>) + Send + Sync>;

/// How long after a failed listing read the next successful turn may try
/// again. A turn succeeding is the cue that the credential works; the
/// Models API refusing while Messages answers is rare, and one retry a
/// minute is not a cost, where one per request could be.
const MODELS_RETRY: Duration = Duration::from_secs(60);

/// Placeholders are recognisable on sight in an `env` dump, and shaped
/// enough like a key that a client sniffing for a prefix is satisfied.
const PLACEHOLDER_PREFIX: &str = "sk-ant-taste-";

/// A hung upstream must not hang a chat forever. Generous, because a
/// non-streaming completion legitimately takes minutes; it bounds the wait
/// for *headers*, after which the body streams for as long as it likes.
const HEADERS_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a streaming response may go without a byte before the proxy
/// ends it with an error the agent can read.
///
/// A stream that stops arriving is what an upstream that went away looks
/// like — the machine running a private model went to sleep, a network
/// path died — and TCP will hold the connection open for as long as
/// nobody sends anything. Left alone, that is the agent's own ten-minute
/// request timeout, and a chat saying "Working…" for ten minutes (David,
/// 2026-09-16: "You should handle the API going away without hanging on
/// Working"). Ninety seconds is long past any gap a live stream has: the
/// API sends `ping` events every few seconds, and a private server
/// streams each token, thinking included.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

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
///
/// Both routes are counted. A turn against the user's own hardware costs
/// no money and no quota, but the question these counters answer is who
/// drew and how much, and an environment that spent its afternoon on the
/// free rung is exactly the thing worth being able to see. What the
/// private route does not touch is [`Handle::quota`] — see `handle`.
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
    /// Placeholder token → the environment it was minted for and the
    /// upstream it reaches. Many live tokens may map to one environment (a
    /// chat respawning does not invalidate its siblings, and two agents in
    /// one environment may be on two hosts); [`Handle::revoke`] drops all
    /// of an environment's at once.
    tokens: Mutex<HashMap<String, Issued>>,
    /// The private upstream, when the user has provisioned one. `None` is
    /// the ordinary case: most workspaces have one upstream, and a private
    /// placeholder with nothing behind it fails the request rather than
    /// falling back to the API — a chat the user opened on their own
    /// hardware must not quietly spend their subscription instead.
    private: Mutex<Option<Arc<FilePrivateUpstream>>>,
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
    /// Where the listing is cached, once [`Handle::refresh_models`] has
    /// said; the refresh a successful turn triggers writes there too.
    models_cache: Mutex<Option<PathBuf>>,
    /// Whether `models` came from the API with the credential in force —
    /// as against the cache, or nothing. False at launch, false again
    /// after an upstream 401 (the credential in force is not the one that
    /// answered), and what a successful turn checks before asking.
    models_from_api: AtomicBool,
    /// One listing read at a time: a fleet's first turns land together.
    models_refreshing: AtomicBool,
    /// When the API was last asked and did not answer, for [`MODELS_RETRY`].
    models_failed_at: Mutex<Option<std::time::Instant>>,
    /// Who is told when the top tier changes ([`Handle::set_models_listener`]).
    models_listener: Mutex<Option<ModelsListener>>,
    /// Environments whose chat has been told this project has no
    /// credential, so it is said once per chat and not once per retry;
    /// an environment leaves the set when a turn of its goes through.
    unprovisioned_told: Mutex<HashSet<String>>,
    /// [`STREAM_IDLE_TIMEOUT`], unless a test shortened it.
    stream_idle: Mutex<Duration>,
    /// [`crate::wake::WAKE_WAIT`], unless a test shortened it.
    wake_wait: Mutex<Duration>,
    /// Where the proxy says what it is doing about a sleeping private
    /// server: the app routes it into the chat of the environment named,
    /// or to a toast when none is ([`Handle::set_notice`]).
    notice: Mutex<Option<Notice>>,
    /// Secret, process-random, and the only entropy placeholders need: a
    /// token is `sha256(seed || counter || env)`, so issuing one cannot
    /// fail the way a fresh RNG read can.
    seed: [u8; 32],
    counter: AtomicU64,
    unauthenticated: AtomicU64,
    unrecognized: AtomicU64,
}

/// What one placeholder stands for: whose spend it is, and where it goes.
#[derive(Debug, Clone)]
struct Issued {
    env: String,
    route: Route,
}

impl ProxyState {
    fn issued_for(&self, token: &str) -> Option<Issued> {
        self.tokens.lock().ok()?.get(token).cloned()
    }

    fn notify(&self, env: Option<&str>, text: String) {
        let notice = self.notice.lock().ok().and_then(|slot| slot.clone());
        match notice {
            Some(notice) => notice(env, text),
            None => tracing::info!("auth proxy: {text}"),
        }
    }

    fn wake_wait(&self) -> Duration {
        self.wake_wait
            .lock()
            .map(|wait| *wait)
            .unwrap_or(crate::wake::WAKE_WAIT)
    }

    /// Read the listing: the cache first when `seed_from_cache` and one is
    /// named, then the API. The API's answer replaces what was there, is
    /// written back to the cache, and — when it changes the top tier —
    /// goes to the listener. One read at a time, and a failed read is not
    /// retried for [`MODELS_RETRY`]. Must run within a tokio runtime
    /// context; the work is spawned and this returns at once.
    fn refresh_models(self: &Arc<Self>, seed_from_cache: bool) {
        if self.models_refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            // No credential is "not yet", not a refusal: the read is not
            // held back for it, and the first turn that goes through
            // asks again. Only the API declining arms the retry wait.
            match state.credentials.credential().await {
                Ok(_) => {
                    if let Err(e) = state.read_models(seed_from_cache).await {
                        tracing::warn!("the account's model listing could not be read: {e}");
                    }
                }
                Err(e) => {
                    tracing::info!("the account's model listing waits for a credential: {e:#}")
                }
            }
            state.models_refreshing.store(false, Ordering::Release);
        });
    }

    /// The read itself, awaited: what [`ProxyState::refresh_models`] runs
    /// in the background and [`Handle::probe_account`] runs for its
    /// verdict. The cache seeds the slot first when asked; the API's
    /// answer replaces the slot, arms or clears the retry wait, tells the
    /// listener if the top tier moved, and is written back to the cache.
    async fn read_models(self: &Arc<Self>, seed_from_cache: bool) -> Result<Vec<ModelListing>> {
        let cache = self.models_cache.lock().ok().and_then(|slot| slot.clone());
        if let (true, Some(path)) = (seed_from_cache, &cache) {
            let path = path.clone();
            let cached = tokio::task::spawn_blocking(move || crate::models::load_cached(&path))
                .await
                .ok()
                .flatten();
            if let Some(models) = cached {
                if let Ok(mut slot) = self.models.lock() {
                    slot.get_or_insert(models);
                }
            }
        }
        let before = self.top_tier();
        let models = match fetch_models(self).await {
            Ok(models) => models,
            Err(e) => {
                if let Ok(mut failed) = self.models_failed_at.lock() {
                    *failed = Some(std::time::Instant::now());
                }
                return Err(e);
            }
        };
        if let Ok(mut slot) = self.models.lock() {
            *slot = Some(models.clone());
        }
        self.models_from_api.store(true, Ordering::Release);
        if let Ok(mut failed) = self.models_failed_at.lock() {
            *failed = None;
        }
        let after = self.top_tier();
        if after.as_ref().map(|m| &m.id) != before.as_ref().map(|m| &m.id) {
            let listener = self
                .models_listener
                .lock()
                .ok()
                .and_then(|slot| slot.clone());
            if let Some(listener) = listener {
                listener(after);
            }
        }
        if let Some(path) = cache {
            let stored = models.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::models::store_cached(&path, &stored) {
                    tracing::warn!("could not cache the model listing: {e}");
                }
            })
            .await;
        }
        Ok(models)
    }

    /// The listing's top tier, as it stands — see [`crate::models::top_tier`].
    fn top_tier(&self) -> Option<ModelListing> {
        let models = self.models.lock().ok()?;
        crate::models::top_tier(models.as_deref()?).cloned()
    }

    /// A turn on the account just went through: if the listing has not
    /// been read with the credential that made it go through, read it now.
    /// This is how a project provisioned after launch gets its top tier
    /// without a restart, and how one re-provisioned onto another account
    /// stops offering the old account's row.
    fn models_wanted_after_success(&self) -> bool {
        if self.models_from_api.load(Ordering::Acquire) {
            return false;
        }
        let recently_failed = self
            .models_failed_at
            .lock()
            .ok()
            .and_then(|failed| *failed)
            .is_some_and(|at| at.elapsed() < MODELS_RETRY);
        !recently_failed
    }

    fn private_source(&self) -> Option<Arc<FilePrivateUpstream>> {
        self.private.lock().ok()?.clone()
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

    /// Mint a placeholder credential for one environment, reaching the API.
    ///
    /// The agent gets this in `ANTHROPIC_AUTH_TOKEN`; it is worthless off
    /// this loopback port, and identifies the spender when it comes back.
    pub fn issue_placeholder(&self, env_id: &str) -> String {
        self.issue_placeholder_for(env_id, Route::Anthropic)
    }

    /// Mint a placeholder credential for one environment, reaching the
    /// upstream `route` names — for the whole life of the placeholder.
    ///
    /// A pure bookkeeping write: it starts nothing, waits on nothing, and
    /// touches no network. The route is decided by the spawn that mints
    /// the placeholder, which is to say by the agent (see [`Route`]); the
    /// agent itself is told nothing and needs nothing — see
    /// [`crate::private`] for why the upstream is the only thing that
    /// differs between the two Claude Codes.
    pub fn issue_placeholder_for(&self, env_id: &str, route: Route) -> String {
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
            tokens.insert(
                token.clone(),
                Issued {
                    env: env_id.to_string(),
                    route,
                },
            );
        }
        token
    }

    /// Revoke every placeholder issued to an environment. Requests bearing
    /// them are refused from the next one on, with no upstream call. The
    /// routes go with them, because a route is a placeholder's and nothing
    /// else's: a later chat of the same name mints its own.
    pub fn revoke(&self, env_id: &str) {
        if let Ok(mut tokens) = self.state.tokens.lock() {
            tokens.retain(|_, issued| issued.env != env_id);
        }
    }

    /// Where sentences about waking a private server go
    /// (`crate::wake`). Without one they are logged.
    pub fn set_notice(&self, notice: Notice) {
        if let Ok(mut slot) = self.state.notice.lock() {
            *slot = Some(notice);
        }
    }

    /// Shorten how long a wake-up is waited for ([`crate::wake::WAKE_WAIT`]).
    /// For tests.
    pub fn set_wake_wait(&self, wait: Duration) {
        if let Ok(mut slot) = self.state.wake_wait.lock() {
            *slot = wait;
        }
    }

    /// Shorten the silence a streaming response may fall into before it
    /// is ended with an error ([`STREAM_IDLE_TIMEOUT`]). For tests, which
    /// cannot wait ninety seconds to watch an upstream go away.
    pub fn set_stream_idle_timeout(&self, timeout: Duration) {
        if let Ok(mut idle) = self.state.stream_idle.lock() {
            *idle = timeout;
        }
    }

    /// Where a placeholder goes. `None` for a token this proxy never
    /// issued, or has revoked.
    pub fn route_of(&self, placeholder: &str) -> Option<Route> {
        self.state
            .issued_for(placeholder)
            .map(|issued| issued.route)
    }

    /// Install the private upstream this workspace was provisioned with.
    ///
    /// Separate from [`AuthProxy::spawn`] for the reason
    /// [`Self::refresh_models`] is: a proxy stood up for a test, or for a
    /// workspace with no private model, should carry no second upstream at
    /// all, and "none" is the ordinary case rather than a degraded one.
    pub fn set_private_upstream(&self, source: Option<Arc<FilePrivateUpstream>>) {
        if let Ok(mut private) = self.state.private.lock() {
            *private = source;
        }
    }

    /// Read the private-model file once, now, so the first chat pane to
    /// build its model drop-down has something to put in it.
    ///
    /// Must be called within a tokio runtime context; the read runs on it
    /// and this returns at once. Every later read happens on the request
    /// path, where the file is re-read whenever it changes.
    pub fn warm_private_upstream(&self) {
        let Some(source) = self.state.private_source() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = source.upstream().await {
                tracing::warn!("the private model could not be read: {e:#}");
            }
        });
    }

    /// Read this project's credential once, now, so a header drawn before
    /// the first turn can say which identity is in force.
    ///
    /// Must be called within a tokio runtime context; the read runs on it
    /// and this returns at once. A project with no credential is the
    /// ordinary case on a first launch and is logged rather than raised —
    /// the honest complaint belongs to the first request, where the chat
    /// shows it and the fix is one step away.
    pub fn warm_credentials(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Err(e) = state.credentials.credential().await {
                tracing::info!("this project has no Anthropic credential yet: {e:#}");
            }
        });
    }

    /// Read this project's credential now and wait for it: what the
    /// settings shade does right after writing the file, so the label it
    /// then reads (`credential_label`) is the file's and not the absence
    /// before it. The error is the file's own complaint.
    pub async fn read_credentials(&self) -> Result<()> {
        self.state.credentials.credential().await.map(|_| ())
    }

    /// Speak to the account once, the way the proxy does for itself: the
    /// Models API, with the credential in force. The settings shade's
    /// "Save and test connection" — a saved token nobody has used is a
    /// setting, not a working one, and the first place a bad token would
    /// otherwise surface is a turn failing mid-conversation. The listing
    /// read here is kept, cached, and announced exactly as a background
    /// read's is, so a passing test also puts the top tier in the pickers.
    /// Fails naming the reason: no credential, a refusal, no answer.
    pub async fn probe_account(&self) -> Result<AccountProbe> {
        self.state
            .credentials
            .credential()
            .await
            .context("no usable credential")?;
        let started = std::time::Instant::now();
        let models = self.state.read_models(false).await?;
        Ok(AccountProbe {
            models: models.len(),
            top_tier: crate::models::top_tier(&models).cloned(),
            elapsed: started.elapsed(),
        })
    }

    /// What the user calls the identity this project is provisioned with
    /// — "work", "personal" — if they named it.
    ///
    /// A pure read, for the GTK thread: nothing here touches the disk, and
    /// the token is not in what comes back. `None` means an unlabelled
    /// credential, a credential from an environment variable (which names
    /// no identity), or a file nothing has read yet — and all three show
    /// the same thing, which is nothing.
    pub fn credential_label(&self) -> Option<String> {
        self.state.credentials.label()
    }

    /// What the private server is, for a picker row or a header mark.
    ///
    /// A pure read, for the GTK thread: nothing here touches the disk, and
    /// the key is not in what comes back. `None` means no private model is
    /// provisioned, or the file has not been read yet.
    pub fn private_model(&self) -> Option<PrivateFacts> {
        self.state.private_source()?.facts()
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
        self.state.top_tier()
    }

    /// Read the account's models: the cache at `cache` first, so a spawn
    /// that comes seconds after launch has last time's answer, then the
    /// API, whose answer replaces it and is written back. Explicit rather
    /// than part of `spawn`, so a proxy stood up for a test or a tool that
    /// never needs the list never makes the request. Must be called within
    /// a tokio runtime context; the work runs on it and this returns at
    /// once.
    ///
    /// A project with no credential yet fails this read quietly, and the
    /// listing stays unread until the first turn that goes through — the
    /// moment the credential is known to work — which asks again on its
    /// own (`ProxyState::refresh_models`). The cache path given here is
    /// the one that later read writes to.
    pub fn refresh_models(&self, cache: Option<PathBuf>) {
        self.set_models_cache(cache);
        self.state.refresh_models(true);
    }

    /// Where the listing is cached, for reads that happen later — a turn's,
    /// or [`Handle::probe_account`]'s. `refresh_models` sets it too.
    pub fn set_models_cache(&self, cache: Option<PathBuf>) {
        if let Ok(mut slot) = self.state.models_cache.lock() {
            *slot = cache;
        }
    }

    /// Be told when the account's top tier changes: the first time the
    /// listing is read off the API (a project provisioned after launch,
    /// whose first turn just went through), or after a re-provision that
    /// changed accounts. What comes back is the new top tier, or `None`
    /// when the account has nothing above Opus. Called off the GTK
    /// thread, like [`Handle::set_notice`].
    pub fn set_models_listener(&self, listener: ModelsListener) {
        if let Ok(mut slot) = self.state.models_listener.lock() {
            *slot = Some(listener);
        }
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
            private: Mutex::new(None),
            spend: Mutex::new(HashMap::new()),
            quota: Mutex::new(QuotaSnapshot::default()),
            models: Mutex::new(None),
            models_cache: Mutex::new(None),
            models_from_api: AtomicBool::new(false),
            models_refreshing: AtomicBool::new(false),
            models_failed_at: Mutex::new(None),
            models_listener: Mutex::new(None),
            unprovisioned_told: Mutex::new(HashSet::new()),
            stream_idle: Mutex::new(STREAM_IDLE_TIMEOUT),
            wake_wait: Mutex::new(crate::wake::WAKE_WAIT),
            notice: Mutex::new(None),
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

/// What one probe of the account found ([`Handle::probe_account`]): how
/// many models the credential can run, the newest above Opus among them,
/// and how long the API took to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProbe {
    pub models: usize,
    pub top_tier: Option<ModelListing>,
    pub elapsed: Duration,
}

/// What one probe of the private server found: which model it says it is
/// serving, if its answer named one, and how long the round trip took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateProbe {
    pub model: Option<String>,
    pub elapsed: Duration,
    /// What had to happen before the server could be asked — it was
    /// asleep and was woken — or nothing.
    pub note: Option<String>,
}

/// How long a probe waits for the private server. Longer than a connect,
/// because a `llama-server` that has just loaded a model can take a few
/// seconds over its first tokens, and the probe wants an answer rather
/// than a connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

impl Handle {
    /// Ask the private server for one short answer, exactly as an agent's
    /// turn would reach it: `POST /v1/messages` at the stored endpoint,
    /// the stored key in the header the file names, and the same
    /// `anthropic-version` a forwarded request carries.
    ///
    /// The one request the proxy makes of the private server for itself,
    /// and it is the settings form's "test connection": a saved endpoint
    /// nobody has spoken to is a setting, not a working one, and the
    /// first place a wrong host or key would otherwise surface is an
    /// agent's turn failing mid-conversation. A refused key drops the
    /// cached upstream the way a refused turn does, so the next request
    /// re-reads the file.
    pub async fn probe_private(&self) -> Result<PrivateProbe> {
        let source = self
            .state
            .private_source()
            .context("no private model is provisioned for this project")?;
        let upstream = source.upstream().await?;
        let model = source
            .facts()
            .and_then(|facts| facts.model)
            .unwrap_or_else(|| "private".to_string());
        let requested: Uri = "/v1/messages".parse().expect("a static path");
        let uri = upstream_uri(&upstream.uri, &requested)?;
        // Asleep? Wake it first, and say so in the verdict. The test is
        // where a sleeping machine is most often met, and "unreachable"
        // with no attempt behind it would send the user to check cables.
        let wake_path = crate::wake::wake_path(source.path());
        let state = self.state.clone();
        let woke = crate::wake::ensure_awake(
            &upstream.uri,
            &wake_path,
            self.state.wake_wait(),
            &move |text| state.notify(None, text),
        )
        .await;
        let host = upstream.uri.host().unwrap_or("the private server");
        if !woke.reached() {
            anyhow::bail!("{}", woke.note(host).unwrap_or_default());
        }
        let note = woke.note(host);
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "Reply with the single word: ready"}],
        })
        .to_string();
        let mut request = Request::post(uri)
            .header("anthropic-version", "2023-06-01")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .context("composing the probe request")?;
        upstream.credential.apply(request.headers_mut());
        let client = build_client::<Full<Bytes>>();
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(PROBE_TIMEOUT, client.request(request))
            .await
            .with_context(|| {
                format!(
                    "{} did not answer within {}s",
                    upstream.uri,
                    PROBE_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("reaching {}", upstream.uri))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .context("reading the server's answer")?
            .to_bytes();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            source.invalidate();
        }
        anyhow::ensure!(
            status.is_success(),
            "{} answered {status}: {}",
            upstream.uri,
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(200)
                .collect::<String>()
        );
        let answered_model = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("model")?.as_str().map(str::to_string));
        // It answered: now is when the neighbour table has its address.
        let learn_uri = upstream.uri.clone();
        tokio::spawn(async move {
            crate::wake::learn(&learn_uri, &wake_path).await;
        });
        Ok(PrivateProbe {
            model: answered_model,
            elapsed: started.elapsed(),
            note,
        })
    }
}

/// Generic over the body: forwarded requests stream the agent's `Incoming`
/// through; the proxy's own requests (the models listing, the private
/// probe) send a fixed one or none.
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
    let Some(Issued { env: env_id, route }) = state.issued_for(&presented) else {
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

    // Which upstream, and whose credential. One decision, not two: a
    // placeholder minted for the private server must never carry the
    // Anthropic credential, and one minted for Anthropic must never reach
    // the private server. Pairing them here is what makes that structural
    // rather than a rule two branches have to remember.
    let private = route.is_private().then(|| state.private_source()).flatten();
    let (target, credential) = match route {
        Route::Anthropic => match state.credentials.credential().await {
            Ok(credential) => (state.upstream.clone(), credential),
            Err(e) => {
                tracing::warn!("auth proxy has no usable credential: {e}");
                // Said in the chat, once: the agent's own rendering of this
                // refusal is an API error deep in a step, and the fix is a
                // settings row it should be pointed at.
                let first = state
                    .unprovisioned_told
                    .lock()
                    .map(|mut told| told.insert(env_id.clone()))
                    .unwrap_or(false);
                if first {
                    state.notify(
                        Some(&env_id),
                        "This project has no Anthropic credential, so its Claude Code chats \
                         cannot reach the API. Add one under Settings → Anthropic account, \
                         then send again."
                            .to_string(),
                    );
                }
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("taste-ide auth proxy has no usable credential: {e}"),
                );
            }
        },
        // A private placeholder with nothing behind it fails the request.
        // It does NOT fall back to the API: this chat was deliberately
        // opened on the user's own hardware, and spending their
        // subscription instead because a file went missing would be the
        // one outcome nobody asked for.
        Route::Private => match &private {
            Some(source) => match source.upstream().await {
                Ok(upstream) => (upstream.uri, upstream.credential),
                Err(e) => {
                    tracing::warn!("auth proxy has no usable private model: {e:#}");
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        &format!("taste-ide auth proxy could not read the private model: {e}"),
                    );
                }
            },
            None => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "this chat runs on Claude Code (Private) and no private model is \
                     provisioned for this project; nothing was sent to the API",
                )
            }
        },
    };

    // A private server's machine may be asleep. Before the request goes,
    // make sure something is listening — and if nothing is, wake it and
    // wait, saying so in the environment's chat as it happens. The check
    // comes before the request because a request's body is a stream that
    // cannot be sent twice; a connection refused after it was sent would
    // be the turn lost, not retried.
    let wake_path = private
        .as_ref()
        .map(|source| crate::wake::wake_path(source.path()));
    if let Some(path) = &wake_path {
        let env = env_id.clone();
        let outcome = crate::wake::ensure_awake(&target, path, state.wake_wait(), &|text| {
            state.notify(Some(&env), text)
        })
        .await;
        if !outcome.reached() {
            let host = target.host().unwrap_or("the private server");
            return error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!(
                    "taste-ide auth proxy: {}",
                    outcome.note(host).unwrap_or_default()
                ),
            );
        }
    }

    let (mut parts, body) = req.into_parts();
    let uri = match upstream_uri(&target, &parts.uri) {
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

    // The private server answered: the one moment the neighbour table is
    // sure to hold its address. Off this request's path, and a no-op while
    // what is on file is fresh (`wake::learn`).
    if let (Some(path), true) = (wake_path, upstream.status().is_success()) {
        let learn_uri = target.clone();
        tokio::spawn(async move {
            crate::wake::learn(&learn_uri, &path).await;
        });
    }

    if upstream.status() == StatusCode::UNAUTHORIZED || upstream.status() == StatusCode::FORBIDDEN {
        // The stored credential is stale (expired, or rotated by a
        // re-login). Drop the cache so the next request re-reads — the
        // one this request actually used, which on the private route is
        // the private server's key and never the account's.
        match &private {
            Some(source) => source.invalidate(),
            None => {
                state.credentials.invalidate();
                // ...and whatever the listing says, it says about a
                // credential that no longer works: the next turn that
                // does go through reads it again, for whichever account
                // the re-provision named.
                state.models_from_api.store(false, Ordering::Release);
            }
        }
    }

    // The account answered: the credential works, which at launch it may
    // not have (a project provisioned after the IDE opened has its first
    // working turn here, not at start-up). The listing that could not be
    // read then is read now, off this request's path, and the app is told
    // if it changes the picker.
    if !route.is_private() && upstream.status().is_success() {
        if let Ok(mut told) = state.unprovisioned_told.lock() {
            told.remove(&env_id);
        }
        if state.models_wanted_after_success() {
            state.refresh_models(false);
        }
    }

    // What the account said about itself, on the way past. Before the
    // hop-by-hop strip only in the sense that it does not matter: none of
    // these are hop-scoped, and the client gets them either way — this
    // proxy reads the mail, it does not intercept it.
    //
    // The private route is skipped, and that is not an omission. A private
    // server's response says nothing about the subscription, and
    // `observe(None, served)` would read a turn it served as proof that a
    // closed Anthropic window had reopened — a gauge lying about a pool
    // this request never touched.
    if !route.is_private() {
        state.record_quota(upstream.status(), upstream.headers(), &env_id);
    }

    let (mut parts, body) = upstream.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    // A private server's stream is put in the documented block order on
    // its way through (`crate::sse`); the API's already is, and its bytes
    // are not touched.
    let streaming = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let normalize = route.is_private() && streaming;
    if normalize {
        // The body that goes out is not the body that came in, so a
        // length the server declared for its own would be a lie hyper
        // enforces by closing the connection; the response streams
        // chunked, as a stream should.
        parts.headers.remove(http::header::CONTENT_LENGTH);
    }
    // A stream is watched for silence; a whole body is not, since it
    // arrives at once or not at all and the headers timeout above already
    // covers "not at all".
    let idle = state
        .stream_idle
        .lock()
        .map(|idle| *idle)
        .unwrap_or(STREAM_IDLE_TIMEOUT);
    let metered = MeteredBody {
        inner: body,
        state: state.clone(),
        env: env_id,
        usage: UsageScan::default(),
        bytes: 0,
        flushed: false,
        order: normalize.then(crate::sse::BlockOrder::new),
        ended: false,
        idle,
        silence: streaming.then(|| Box::pin(tokio::time::sleep(idle))),
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
/// Nothing is logged, and nothing is buffered beyond one incomplete event
/// on the private route: each frame is measured, its bytes scanned for the
/// Messages API's `usage` counters, and handed on — through the block
/// normalizer when there is one (`crate::sse`), as they came otherwise.
struct MeteredBody {
    inner: Incoming,
    state: Arc<ProxyState>,
    env: String,
    usage: UsageScan,
    bytes: u64,
    flushed: bool,
    /// The event-order normalizer, on a private server's stream only.
    order: Option<crate::sse::BlockOrder>,
    /// The upstream has ended and said so; a finished `Incoming` is not
    /// polled again, whatever the normalizer still had to send.
    ended: bool,
    /// How long the stream may be silent, and the timer that says it has
    /// been ([`STREAM_IDLE_TIMEOUT`]). `None` for a body that does not
    /// stream. Reset on every byte; when it fires, the response ends with
    /// an `error` event naming the silence.
    idle: Duration,
    silence: Option<Pin<Box<tokio::time::Sleep>>>,
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
        if this.ended {
            return Poll::Ready(None);
        }
        loop {
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let Some(data) = frame.data_ref() else {
                        return Poll::Ready(Some(Ok(frame)));
                    };
                    // A byte arrived: the silence starts over.
                    if let Some(silence) = this.silence.as_mut() {
                        silence
                            .as_mut()
                            .reset(tokio::time::Instant::now() + this.idle);
                    }
                    this.bytes += data.len() as u64;
                    let data = data.clone();
                    this.usage.feed(&data);
                    if let Some(refusal) = this.refusal.as_mut() {
                        let room = MAX_REFUSAL_BODY.saturating_sub(refusal.len());
                        refusal.extend_from_slice(&data[..data.len().min(room)]);
                    }
                    let Some(order) = this.order.as_mut() else {
                        return Poll::Ready(Some(Ok(frame)));
                    };
                    // The normalizer holds an incomplete event back; a
                    // chunk that ended mid-event yields nothing yet, and
                    // an empty frame is not a thing to send, so read on.
                    let out = order.feed(&data);
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    this.flush();
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    this.flush();
                    this.ended = true;
                    let left = this
                        .order
                        .as_mut()
                        .map(crate::sse::BlockOrder::finish)
                        .unwrap_or_default();
                    if left.is_empty() {
                        return Poll::Ready(None);
                    }
                    // The remainder goes out now; the end is said on the
                    // next poll.
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(left)))));
                }
                Poll::Pending => {
                    // Nothing from the upstream. If nothing has come for
                    // the whole of the idle window, the upstream is gone
                    // as far as this response is concerned: say so in the
                    // stream's own vocabulary — an `error` event, which
                    // the agent's client surfaces as a failed turn — and
                    // end. Left open, the agent would wait out its own
                    // timeout with "Working…" on screen.
                    let Some(silence) = this.silence.as_mut() else {
                        return Poll::Pending;
                    };
                    if silence.as_mut().poll(cx).is_pending() {
                        return Poll::Pending;
                    }
                    tracing::warn!(
                        "auth proxy: no bytes from the upstream for {}s on {}'s stream; ending it",
                        this.idle.as_secs(),
                        this.env
                    );
                    this.flush();
                    this.ended = true;
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(stalled_event(
                        this.idle,
                    ))))));
                }
            }
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        // A normalized stream may not be the upstream's length; the
        // upstream's hint is honest only where nothing is changed.
        if self.order.is_some() {
            return hyper::body::SizeHint::default();
        }
        self.inner.size_hint()
    }
}

impl Drop for MeteredBody {
    /// A cancelled turn drops the body mid-stream; what was spent still was.
    fn drop(&mut self) {
        self.flush();
    }
}

/// The event that ends a stream the upstream stopped feeding: the Messages
/// API's own `error` event, so the agent's client raises it as an API
/// error with this message rather than a parse failure or a silent stop.
fn stalled_event(idle: Duration) -> String {
    let message = format!(
        "taste-ide auth proxy: the upstream sent nothing for {}s and the response was ended. \
         The server may be asleep or unreachable; check it and send again.",
        idle.as_secs()
    );
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": message},
    });
    format!("event: error\ndata: {body}\n\n")
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
