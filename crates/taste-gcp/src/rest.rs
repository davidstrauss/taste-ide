//! Calling Google's APIs as the IDE's service account.
//!
//! A small JSON client over the same hyper and rustls stack the proxy
//! uses, with three things every caller would otherwise get wrong: a
//! token that is renewed before it lapses rather than after a 401, Google's
//! error envelope read into an [`ApiError`] that says which kind of failure
//! it was (so "it already exists" and "there is no capacity" can be told
//! apart by code, not by matching prose), and Compute's long-running
//! operations awaited to their end, because a create that was accepted is
//! not a create that succeeded.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::Method;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::Value;

use crate::gcloud::Gcloud;

/// How long before its stated expiry a token stops being used, so no
/// request goes out with one that lapses on the way.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A bearer token for Google's APIs, and when it lapses.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("token", &"…")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AccessToken {
    /// Usable for another [`EXPIRY_MARGIN`] at least.
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        self.expires_at
            .duration_since(now)
            .is_ok_and(|left| left > EXPIRY_MARGIN)
    }
}

/// Where each API is. Fixed in the product; a test points them at a mock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub compute: String,
    pub dns: String,
    pub resource_manager: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            compute: "https://compute.googleapis.com/compute/v1".into(),
            dns: "https://dns.googleapis.com/dns/v1".into(),
            resource_manager: "https://cloudresourcemanager.googleapis.com/v1".into(),
        }
    }
}

enum Source {
    Gcloud(Gcloud),
    #[cfg(any(test, feature = "testing"))]
    Fixed(String),
}

/// Access tokens for the IDE's calls, asked for again when the last one is
/// within [`EXPIRY_MARGIN`] of lapsing and not before.
pub struct TokenSource {
    source: Source,
    cached: tokio::sync::Mutex<Option<AccessToken>>,
}

impl TokenSource {
    /// Tokens from the project's own gcloud, as the service account it
    /// impersonates.
    pub fn gcloud(gcloud: Gcloud) -> Self {
        Self {
            source: Source::Gcloud(gcloud),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// One token that never lapses, for tests.
    #[cfg(any(test, feature = "testing"))]
    pub fn fixed(token: &str) -> Self {
        Self {
            source: Source::Fixed(token.to_string()),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref().filter(|t| t.is_fresh(SystemTime::now())) {
            return Ok(token.token.clone());
        }
        let fresh = match &self.source {
            Source::Gcloud(gcloud) => gcloud.access_token().await?,
            #[cfg(any(test, feature = "testing"))]
            Source::Fixed(token) => AccessToken {
                token: token.clone(),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            },
        };
        let token = fresh.token.clone();
        *cached = Some(fresh);
        Ok(token)
    }
}

/// A failed call, as Google's error envelope describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// The canonical code, `NOT_FOUND`, `ALREADY_EXISTS`, … when given.
    pub code: Option<String>,
    /// The first detailed reason, `alreadyExists`,
    /// `ZONE_RESOURCE_POOL_EXHAUSTED`, … when given.
    pub reason: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(reason) = self.reason.as_ref().or(self.code.as_ref()) {
            write!(f, " ({reason})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        self.status == 404 || self.code.as_deref() == Some("NOT_FOUND")
    }

    /// The zone could not supply the machine right now, or does not offer
    /// it at all: either way, another machine of the same size may do.
    pub fn is_unavailable_machine(&self) -> bool {
        let said = |text: &Option<String>| {
            text.as_deref().is_some_and(|t| {
                t.contains("RESOURCE_POOL_EXHAUSTED")
                    || t.contains("STOCKOUT")
                    || t.contains("UNSUPPORTED_OPERATION")
            })
        };
        said(&self.reason)
            || said(&self.code)
            || (self.message.contains("machineType") && self.message.contains("does not exist"))
            || self.message.contains("does not have enough resources")
    }

    pub fn is_already_exists(&self) -> bool {
        self.status == 409
            || self.code.as_deref() == Some("ALREADY_EXISTS")
            || self.reason.as_deref() == Some("alreadyExists")
    }

    /// Read Google's `{"error": {"code", "message", "status", "errors":
    /// [{"reason"}]}}`, or say what came back instead.
    fn from_body(status: u16, body: &[u8]) -> Self {
        let parsed: Option<Value> = serde_json::from_slice(body).ok();
        let error = parsed.as_ref().and_then(|v| v.get("error"));
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        Self {
            status,
            code: text(error.and_then(|e| e.get("status"))),
            reason: text(
                error
                    .and_then(|e| e.get("errors"))
                    .and_then(|e| e.get(0))
                    .and_then(|e| e.get("reason")),
            ),
            message: text(error.and_then(|e| e.get("message")))
                .unwrap_or_else(|| format!("Google answered {status}")),
        }
    }

    /// A Compute operation's own `error`, which is shaped differently:
    /// `{"errors": [{"code", "message"}]}`, with the reason in `code`.
    fn from_operation(error: &Value) -> Self {
        let first = error.get("errors").and_then(|e| e.get(0));
        let text = |key: &str| first.and_then(|f| f.get(key)).and_then(Value::as_str);
        Self {
            status: 0,
            code: None,
            reason: text("code").map(str::to_string),
            message: text("message")
                .unwrap_or("the operation failed without saying why")
                .to_string(),
        }
    }
}

/// How long one wait on an operation may last before asking again;
/// Compute's own `wait` returns after at most two minutes.
const CALL_TIMEOUT: Duration = Duration::from_secs(150);
/// How long a create or start may take, in all, before the caller is told.
pub const OPERATION_DEADLINE: Duration = Duration::from_secs(20 * 60);

/// Google's APIs, as one workspace's identity.
pub struct Gcp {
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    tokens: Arc<TokenSource>,
    pub endpoints: Endpoints,
}

impl Gcp {
    pub fn new(tokens: Arc<TokenSource>, endpoints: Endpoints) -> Self {
        // `https_or_http` only so a test can point the endpoints at a
        // loopback mock; every endpoint the product uses is https.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            http: Client::builder(TokioExecutor::new()).build(https),
            tokens,
            endpoints,
        }
    }

    /// One call: `body` as JSON if given, the answer as JSON, a failure as
    /// an [`ApiError`] (inside the `anyhow::Error`, for `downcast_ref`).
    pub async fn call(&self, method: Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let token = self.tokens.token().await?;
        let mut request = http::Request::builder()
            .method(method.clone())
            .uri(url)
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        let payload = match body {
            Some(body) => {
                request = request.header(http::header::CONTENT_TYPE, "application/json");
                Bytes::from(body.to_string())
            }
            None => {
                // Google refuses a write with no stated length (411), and an
                // empty body is sent with none: Compute's `wait`, `start`,
                // and `stop` are all POSTs with nothing in them.
                if method != Method::GET && method != Method::DELETE {
                    request = request.header(http::header::CONTENT_LENGTH, "0");
                }
                Bytes::new()
            }
        };
        let request = request.body(Full::new(payload))?;
        let response = tokio::time::timeout(CALL_TIMEOUT, self.http.request(request))
            .await
            .with_context(|| format!("{method} {url} did not answer"))?
            .with_context(|| format!("{method} {url}"))?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            let mut error = ApiError::from_body(status.as_u16(), &bytes);
            if error.code.is_none() && error.reason.is_none() {
                // Nothing of Google's own to say, so at least say to what.
                let path = url.split_once("googleapis.com").map_or(url, |(_, p)| p);
                error.message = format!("{} to {method} {path}", error.message);
            }
            return Err(error.into());
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Wait for a Compute operation to finish, and fail as it failed.
    pub async fn wait(&self, operation: Value) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + OPERATION_DEADLINE;
        let mut operation = operation;
        loop {
            if operation["status"] == "DONE" {
                if let Some(error) = operation.get("error") {
                    return Err(ApiError::from_operation(error).into());
                }
                return Ok(operation);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "operation {} was still running after {} minutes",
                    operation["name"],
                    OPERATION_DEADLINE.as_secs() / 60
                );
            }
            let link = operation["selfLink"]
                .as_str()
                .context("an operation without a selfLink")?;
            let path = link
                .split_once("/compute/v1")
                .map_or(link, |(_, path)| path);
            let url = format!("{}{path}/wait", self.endpoints.compute);
            operation = self.call(Method::POST, &url, None).await?;
        }
    }

    /// Create something with a Compute insert, and wait for it. Something
    /// that already exists is not an error: every create the IDE makes is
    /// meant to be repeatable, which is how a half-finished setup is
    /// finished by running it again.
    pub async fn ensure_compute(&self, collection: &str, body: &Value) -> Result<()> {
        let url = format!("{}/{collection}", self.endpoints.compute);
        match self.call(Method::POST, &url, Some(body)).await {
            Ok(operation) => match self.wait(operation).await {
                Ok(_) => Ok(()),
                Err(e) if is_already_exists(&e) => Ok(()),
                Err(e) => Err(e),
            },
            Err(e) if is_already_exists(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Delete something by its Compute path, and wait. Something that is
    /// already gone is not an error, for the same reason.
    pub async fn remove_compute(&self, path: &str) -> Result<()> {
        let url = format!("{}/{path}", self.endpoints.compute);
        match self.call(Method::DELETE, &url, None).await {
            Ok(operation) => match self.wait(operation).await {
                Err(e) if !is_not_found(&e) => Err(e),
                _ => Ok(()),
            },
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// A Compute resource by its path, or `None` if there is none.
    pub async fn get_compute(&self, path: &str) -> Result<Option<Value>> {
        let url = format!("{}/{path}", self.endpoints.compute);
        match self.call(Method::GET, &url, None).await {
            Ok(value) => Ok(Some(value)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create a Cloud DNS policy, which unlike Compute answers at once
    /// rather than with an operation. Already there is not an error.
    pub async fn ensure_dns_policy(&self, project: &str, body: &Value) -> Result<()> {
        let url = format!("{}/projects/{project}/policies", self.endpoints.dns);
        match self.call(Method::POST, &url, Some(body)).await {
            Err(e) if !is_already_exists(&e) => Err(e),
            _ => Ok(()),
        }
    }

    /// Delete a Cloud DNS policy. One still bound to a network cannot be,
    /// so it is unbound first; one already gone is not an error.
    pub async fn remove_dns_policy(&self, project: &str, name: &str) -> Result<()> {
        let url = format!("{}/projects/{project}/policies/{name}", self.endpoints.dns);
        match self
            .call(
                Method::PATCH,
                &url,
                Some(&serde_json::json!({ "networks": [] })),
            )
            .await
        {
            Err(e) if is_not_found(&e) => return Ok(()),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        match self.call(Method::DELETE, &url, None).await {
            Err(e) if !is_not_found(&e) => Err(e),
            _ => Ok(()),
        }
    }

    /// Which of `permissions` the identity holds on `project` — the
    /// preflight's question, asked by name before anything is created.
    pub async fn test_permissions(
        &self,
        project: &str,
        permissions: &[&str],
    ) -> Result<Vec<String>> {
        let url = format!(
            "{}/projects/{project}:testIamPermissions",
            self.endpoints.resource_manager
        );
        let answer = self
            .call(
                Method::POST,
                &url,
                Some(&serde_json::json!({ "permissions": permissions })),
            )
            .await?;
        Ok(answer["permissions"]
            .as_array()
            .map(|held| {
                held.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

fn api_error(e: &anyhow::Error) -> Option<&ApiError> {
    e.downcast_ref::<ApiError>()
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    api_error(e).is_some_and(ApiError::is_not_found)
}

pub fn is_unavailable_machine(e: &anyhow::Error) -> bool {
    api_error(e).is_some_and(ApiError::is_unavailable_machine)
}

pub fn is_already_exists(e: &anyhow::Error) -> bool {
    api_error(e).is_some_and(ApiError::is_already_exists)
}

#[cfg(test)]
pub(crate) mod mock {
    //! A loopback stand-in for Google's APIs: answers each request from a
    //! list of `(method, path, status, body)` in order, and records what it
    //! was asked.

    use super::*;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    pub struct Asked {
        pub method: String,
        pub path: String,
        pub authorization: Option<String>,
        pub body: Option<Value>,
    }

    pub struct Mock {
        pub endpoints: Endpoints,
        pub asked: Arc<Mutex<Vec<Asked>>>,
    }

    pub async fn serve(script: Vec<(&'static str, &'static str, u16, Value)>) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter()));
        let record = asked.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let record = record.clone();
                let script = script.clone();
                tokio::spawn(async move {
                    let service =
                        service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                            let record = record.clone();
                            let script = script.clone();
                            async move {
                                let method = request.method().to_string();
                                let path = request
                                    .uri()
                                    .path_and_query()
                                    .map(|p| p.to_string())
                                    .unwrap_or_default();
                                let authorization = request
                                    .headers()
                                    .get(http::header::AUTHORIZATION)
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_string);
                                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                                record.lock().unwrap().push(Asked {
                                    method: method.clone(),
                                    path: path.clone(),
                                    authorization,
                                    body: serde_json::from_slice(&bytes).ok(),
                                });
                                let (want_method, want_path, status, body) = script
                                    .lock()
                                    .unwrap()
                                    .next()
                                    .unwrap_or_else(|| panic!("unexpected {method} {path}"));
                                assert_eq!(method, want_method, "{path}");
                                assert!(
                                    path.ends_with(want_path),
                                    "asked {path}, expected …{want_path}"
                                );
                                let mut response =
                                    hyper::Response::new(Full::new(Bytes::from(body.to_string())));
                                *response.status_mut() =
                                    hyper::StatusCode::from_u16(status).unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        Mock {
            endpoints: Endpoints {
                compute: format!("{base}/compute/v1"),
                dns: format!("{base}/dns/v1"),
                resource_manager: format!("{base}/v1"),
            },
            asked,
        }
    }

    pub fn gcp(mock: &Mock) -> Gcp {
        Gcp::new(
            Arc::new(TokenSource::fixed("ya29.test")),
            mock.endpoints.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{gcp, serve};
    use super::*;
    use serde_json::json;

    fn operation(status: &str, name: &str) -> Value {
        json!({
            "name": name,
            "status": status,
            "selfLink": format!("https://www.googleapis.com/compute/v1/projects/p/global/operations/{name}"),
        })
    }

    #[tokio::test]
    async fn a_create_is_waited_to_its_end_with_the_token() {
        let mock = serve(vec![
            (
                "POST",
                "/compute/v1/projects/p/global/networks",
                200,
                operation("RUNNING", "op-1"),
            ),
            (
                "POST",
                "/compute/v1/projects/p/global/operations/op-1/wait",
                200,
                operation("RUNNING", "op-1"),
            ),
            (
                "POST",
                "/compute/v1/projects/p/global/operations/op-1/wait",
                200,
                operation("DONE", "op-1"),
            ),
        ])
        .await;
        gcp(&mock)
            .ensure_compute("projects/p/global/networks", &json!({ "name": "n" }))
            .await
            .unwrap();
        let asked = mock.asked.lock().unwrap();
        assert_eq!(asked.len(), 3);
        assert!(asked.iter().all(|a| a.method == "POST"));
        assert_eq!(asked[0].body, Some(json!({ "name": "n" })));
        assert!(asked
            .iter()
            .all(|a| a.authorization.as_deref() == Some("Bearer ya29.test")));
    }

    #[tokio::test]
    async fn creating_what_exists_is_not_an_error() {
        let mock = serve(vec![(
            "POST",
            "/compute/v1/projects/p/global/networks",
            409,
            json!({ "error": { "code": 409, "status": "ALREADY_EXISTS", "message": "The resource 'n' already exists",
                               "errors": [{ "reason": "alreadyExists" }] } }),
        )])
        .await;
        gcp(&mock)
            .ensure_compute("projects/p/global/networks", &json!({ "name": "n" }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_operation_that_fails_fails_with_its_own_reason() {
        let mut failed = operation("DONE", "op-2");
        failed["error"] = json!({ "errors": [{
            "code": "ZONE_RESOURCE_POOL_EXHAUSTED",
            "message": "The zone does not have enough resources available to fulfill the request.",
        }] });
        let mock = serve(vec![(
            "POST",
            "/compute/v1/projects/p/zones/z/instances",
            200,
            failed,
        )])
        .await;
        let error = gcp(&mock)
            .ensure_compute("projects/p/zones/z/instances", &json!({ "name": "i" }))
            .await
            .unwrap_err();
        let api = error.downcast_ref::<ApiError>().unwrap();
        assert_eq!(api.reason.as_deref(), Some("ZONE_RESOURCE_POOL_EXHAUSTED"));
        assert!(error.to_string().contains("enough resources"), "{error}");
    }

    #[tokio::test]
    async fn removing_what_is_gone_is_not_an_error_and_a_missing_get_is_none() {
        let not_found =
            json!({ "error": { "code": 404, "status": "NOT_FOUND", "message": "not found" } });
        let mock = serve(vec![
            (
                "DELETE",
                "/compute/v1/projects/p/global/networks/n",
                404,
                not_found.clone(),
            ),
            (
                "GET",
                "/compute/v1/projects/p/global/networks/n",
                404,
                not_found,
            ),
        ])
        .await;
        let gcp = gcp(&mock);
        gcp.remove_compute("projects/p/global/networks/n")
            .await
            .unwrap();
        assert!(gcp
            .get_compute("projects/p/global/networks/n")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_preflight_asks_for_permissions_by_name() {
        let mock = serve(vec![(
            "POST",
            "/v1/projects/p:testIamPermissions",
            200,
            json!({ "permissions": ["compute.instances.get"] }),
        )])
        .await;
        let held = gcp(&mock)
            .test_permissions("p", &["compute.instances.get", "compute.instances.create"])
            .await
            .unwrap();
        assert_eq!(held, vec!["compute.instances.get"]);
        let asked = mock.asked.lock().unwrap();
        assert_eq!(
            asked[0].body,
            Some(json!({ "permissions": ["compute.instances.get", "compute.instances.create"] }))
        );
    }

    #[test]
    fn a_token_is_stale_inside_the_margin_and_never_printed() {
        let now = SystemTime::now();
        let token = AccessToken {
            token: "ya29.secret".into(),
            expires_at: now + EXPIRY_MARGIN,
        };
        assert!(!token.is_fresh(now));
        assert!(token.is_fresh(now - Duration::from_secs(1)));
        assert!(!format!("{token:?}").contains("secret"));
    }

    #[tokio::test]
    async fn a_write_with_nothing_in_it_still_states_its_length() {
        // The mock is plain HTTP/1.1 like Google's front end in this
        // respect: it is the request that carries the header, so read it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            let n = tcp.read(&mut buffer).await.unwrap();
            let head = String::from_utf8_lossy(&buffer[..n]).to_lowercase();
            tcp.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                .await
                .unwrap();
            head
        });
        let gcp = Gcp::new(Arc::new(TokenSource::fixed("t")), Endpoints::default());
        gcp.call(Method::POST, &format!("http://{address}/x/start"), None)
            .await
            .unwrap();
        assert!(seen.await.unwrap().contains("content-length: 0"));
    }

    #[test]
    fn a_stockout_is_told_from_a_fault() {
        let stockout = ApiError::from_operation(&json!({ "errors": [{
            "code": "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
            "message": "The zone 'projects/p/zones/z' does not have enough resources available to fulfill the request.",
        }] }));
        assert!(stockout.is_unavailable_machine());
        let fault = ApiError::from_operation(&json!({ "errors": [{
            "code": "QUOTA_EXCEEDED",
            "message": "Quota 'N4_CPUS' exceeded.",
        }] }));
        assert!(!fault.is_unavailable_machine());
    }

    #[test]
    fn an_unreadable_error_still_says_what_came_back() {
        let error = ApiError::from_body(502, b"<html>bad gateway</html>");
        assert_eq!(error.message, "Google answered 502");
        assert!(error.code.is_none());
    }
}
