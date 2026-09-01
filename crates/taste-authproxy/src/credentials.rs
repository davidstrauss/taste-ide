//! Where the real credential comes from, and how it becomes headers.
//!
//! Three shapes, in the order the IDE looks for them:
//!
//! 1. `ANTHROPIC_API_KEY` in the IDE's own environment — a plain key.
//! 2. The agent home volume's `.claude/.credentials.json`, host-side. This
//!    is where the existing agent-side login flow writes, and reading it
//!    from the host is the bootstrap ENVIRONMENTS.md describes: sign-in
//!    stays agent-side for now, the IDE just takes custody of the result.
//! 3. `$HOME/.claude/.credentials.json` — the user's own Claude Code
//!    login, for a developer running the IDE from a checkout.
//!
//! File-backed sources re-read on mtime change, so a re-login lands
//! without restarting the IDE, and on demand after an upstream 401.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use http::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;

/// Header carrying a plain Anthropic API key.
pub const X_API_KEY: &str = "x-api-key";

/// Treat a token as expired this long before it actually is, so a request
/// does not race the clock on the way to the API.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// What the proxy puts on an outbound request in place of the placeholder.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A plain API key: `x-api-key`.
    ApiKey(String),
    /// A Claude Code OAuth access token: `Authorization: Bearer`.
    OAuth(String),
}

impl std::fmt::Debug for Credential {
    /// Never let a credential reach a log through `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::ApiKey(_) => f.write_str("Credential::ApiKey(<redacted>)"),
            Credential::OAuth(_) => f.write_str("Credential::OAuth(<redacted>)"),
        }
    }
}

impl Credential {
    /// Replace whatever authentication the agent sent with the real thing.
    ///
    /// Both auth headers are removed first: the placeholder arrives in one
    /// of them, and leaving the other in place would let an agent smuggle
    /// a second credential upstream.
    pub fn apply(&self, headers: &mut HeaderMap) {
        headers.remove(AUTHORIZATION);
        headers.remove(X_API_KEY);
        match self {
            Credential::ApiKey(key) => {
                if let Ok(value) = HeaderValue::from_str(key) {
                    let mut value = value;
                    value.set_sensitive(true);
                    headers.insert(X_API_KEY, value);
                }
            }
            Credential::OAuth(token) => {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
                    let mut value = value;
                    value.set_sensitive(true);
                    headers.insert(AUTHORIZATION, value);
                }
            }
        }
    }
}

/// A boxed future, so [`CredentialSource`] can be async without pulling in
/// an async-trait macro (a refresh implementation has to make a network
/// call; a file read should not block a runtime worker).
pub type CredentialFuture<'a> = Pin<Box<dyn std::future::Future<Output = Result<Credential>> + Send + 'a>>;

/// Where the proxy gets the credential it substitutes.
pub trait CredentialSource: Send + Sync + 'static {
    /// The credential to use for the next request.
    fn credential(&self) -> CredentialFuture<'_>;

    /// Drop any cache. Called when the upstream answers 401, so a token
    /// rotated behind our back (a re-login, an agent-side refresh) is
    /// picked up on the next attempt rather than at the next restart.
    fn invalidate(&self) {}
}

/// A credential that never changes: an API key from the environment.
pub struct StaticKey(Credential);

impl StaticKey {
    pub fn api_key(key: impl Into<String>) -> Self {
        Self(Credential::ApiKey(key.into()))
    }

    pub fn oauth(token: impl Into<String>) -> Self {
        Self(Credential::OAuth(token.into()))
    }
}

impl CredentialSource for StaticKey {
    fn credential(&self) -> CredentialFuture<'_> {
        let credential = self.0.clone();
        Box::pin(async move { Ok(credential) })
    }
}

/// The on-disk shape Claude Code writes. Verified against a real
/// `~/.claude/.credentials.json` (2026-08): a single `claudeAiOauth`
/// object holding `accessToken` (`sk-ant-oat01-…`), `refreshToken`
/// (`sk-ant-ort01-…`), `expiresAt` and `refreshTokenExpiresAt` as
/// milliseconds since the epoch, plus `scopes`, `subscriptionType` and
/// `rateLimitTier` which the proxy has no use for.
#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthBlob>,
}

#[derive(Debug, Clone, Deserialize)]
struct OauthBlob {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Present in real material, unused until IDE-side refresh lands.
    #[serde(rename = "refreshToken", default)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
    /// Milliseconds since the epoch.
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<i64>,
}

struct Cached {
    mtime: Option<SystemTime>,
    len: u64,
    blob: OauthBlob,
}

/// A credential read from a Claude Code credentials file, re-read whenever
/// the file's mtime or length changes.
///
/// # Refresh
///
/// The file carries a refresh token, and the access token is short-lived
/// (hours). This source therefore *detects* expiry and fails with a clear
/// message rather than sending a token the API will reject.
///
/// It does not refresh. Doing so means posting to Anthropic's OAuth token
/// endpoint with a client id, and neither can be shipped on a guess: a
/// wrong endpoint or client id turns every expired-token case into a
/// confusing network error, and a *right* one written from memory is still
/// unverified. Until they are confirmed against real material, the
/// recovery path is the one that already works — the agent-side login
/// flow rewrites the file, [`CredentialSource::invalidate`] drops the
/// cache on the resulting 401, and the next request picks it up.
///
/// TODO(auth-proxy): IDE-owned OAuth (ENVIRONMENTS.md → "The auth proxy",
/// bootstrap pragmatics) subsumes this — sign-in, refresh, and write-back
/// all move here, with concurrent refreshes serialized behind one lock.
pub struct FileCredentials {
    path: PathBuf,
    cache: Mutex<Option<Cached>>,
}

impl FileCredentials {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cache: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cached blob, if the file on disk still matches what produced it.
    fn cached_if_fresh(&self, mtime: Option<SystemTime>, len: u64) -> Option<OauthBlob> {
        let cache = self.cache.lock().ok()?;
        let cached = cache.as_ref()?;
        (cached.mtime == mtime && cached.len == len).then(|| cached.blob.clone())
    }

    fn parse(bytes: &[u8], path: &Path) -> Result<OauthBlob> {
        let parsed: CredentialsFile = serde_json::from_slice(bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        parsed.claude_ai_oauth.with_context(|| {
            format!(
                "{} has no claudeAiOauth block: the agent has not signed in",
                path.display()
            )
        })
    }

    fn check_expiry(blob: &OauthBlob, path: &Path) -> Result<()> {
        let Some(expires_at) = blob.expires_at else {
            return Ok(());
        };
        let deadline = SystemTime::now() + EXPIRY_SKEW;
        let deadline_ms = deadline
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        anyhow::ensure!(
            expires_at > deadline_ms,
            "the Anthropic access token in {} expired; sign in again in the agent's console tab \
             (IDE-owned refresh is not implemented yet)",
            path.display()
        );
        Ok(())
    }
}

impl CredentialSource for FileCredentials {
    fn credential(&self) -> CredentialFuture<'_> {
        Box::pin(async move {
            let meta = tokio::fs::metadata(&self.path)
                .await
                .with_context(|| format!("reading {}", self.path.display()))?;
            let mtime = meta.modified().ok();
            let len = meta.len();

            if let Some(blob) = self.cached_if_fresh(mtime, len) {
                Self::check_expiry(&blob, &self.path)?;
                return Ok(Credential::OAuth(blob.access_token));
            }

            let bytes = tokio::fs::read(&self.path)
                .await
                .with_context(|| format!("reading {}", self.path.display()))?;
            let blob = Self::parse(&bytes, &self.path)?;
            if let Ok(mut cache) = self.cache.lock() {
                *cache = Some(Cached {
                    mtime,
                    len,
                    blob: blob.clone(),
                });
            }
            Self::check_expiry(&blob, &self.path)?;
            Ok(Credential::OAuth(blob.access_token))
        })
    }

    fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }
}

/// A source that works out *which* source to be on first use.
///
/// Discovery runs a podman command, so it cannot happen on the GTK main
/// thread where agent spawns are composed. Deferring it to the first
/// proxied request puts it on a runtime worker and off the critical path
/// of starting a chat.
#[derive(Default)]
pub struct DiscoveredCredentials {
    resolved: tokio::sync::OnceCell<Arc<dyn CredentialSource>>,
}

impl DiscoveredCredentials {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialSource for DiscoveredCredentials {
    fn credential(&self) -> CredentialFuture<'_> {
        Box::pin(async move {
            let inner = self
                .resolved
                .get_or_try_init(|| async { discover().await })
                .await?;
            inner.credential().await
        })
    }

    fn invalidate(&self) {
        if let Some(inner) = self.resolved.get() {
            inner.invalidate();
        }
    }
}

/// Find the real credential, host-side.
pub async fn discover() -> Result<Arc<dyn CredentialSource>> {
    // An explicit path wins: it is how a test rig or a developer points
    // the proxy at material that is not in either usual place.
    if let Some(path) = std::env::var_os("TASTE_ANTHROPIC_CREDENTIALS") {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            path.exists(),
            "TASTE_ANTHROPIC_CREDENTIALS points at {} which does not exist",
            path.display()
        );
        return Ok(Arc::new(FileCredentials::new(path)));
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(Arc::new(StaticKey::api_key(key)));
        }
    }

    let mut looked = Vec::new();
    if let Some(mountpoint) = agent_home_mountpoint().await {
        let path = mountpoint.join(".claude/.credentials.json");
        if path.exists() {
            return Ok(Arc::new(FileCredentials::new(path)));
        }
        looked.push(path);
    }

    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".claude/.credentials.json");
        if path.exists() {
            return Ok(Arc::new(FileCredentials::new(path)));
        }
        looked.push(path);
    }

    anyhow::bail!(
        "no Anthropic credential found (set ANTHROPIC_API_KEY, or sign in so one of {} exists)",
        looked
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Host-side mountpoint of the volume the agent uses as its home.
///
/// Phase 2 makes this per environment (`taste-env-<hash>-<env>-home`);
/// today there is one machine-global volume.
async fn agent_home_mountpoint() -> Option<PathBuf> {
    const VOLUME: &str = "taste-agent-home";
    // Same host escape as taste-devcontainer's supervisor: inside the
    // Flatpak sandbox podman lives on the host.
    let sandboxed = Path::new("/.flatpak-info").exists();
    let mut command = if sandboxed {
        let mut c = tokio::process::Command::new("flatpak-spawn");
        c.arg("--host").arg("podman");
        c
    } else {
        tokio::process::Command::new("podman")
    };
    let output = command
        .args([
            "volume",
            "inspect",
            "--format",
            "{{.Mountpoint}}",
            VOLUME,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[test]
    fn applying_a_credential_removes_whatever_the_agent_sent() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer placeholder"));
        headers.insert(X_API_KEY, HeaderValue::from_static("smuggled"));

        Credential::OAuth("real-token".into()).apply(&mut headers);
        assert_eq!(
            header_str(&headers, "authorization").as_deref(),
            Some("Bearer real-token")
        );
        assert!(headers.get(X_API_KEY).is_none());

        Credential::ApiKey("real-key".into()).apply(&mut headers);
        assert_eq!(header_str(&headers, X_API_KEY).as_deref(), Some("real-key"));
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn a_credential_does_not_debug_print_itself() {
        let rendered = format!("{:?}", Credential::OAuth("sk-ant-oat01-secret".into()));
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn the_real_file_shape_parses() {
        // The exact shape of a live ~/.claude/.credentials.json, with the
        // token material replaced.
        let json = br#"{"claudeAiOauth":{
            "accessToken":"sk-ant-oat01-aaa",
            "refreshToken":"sk-ant-ort01-bbb",
            "expiresAt":1788250887800,
            "refreshTokenExpiresAt":1789480328800,
            "scopes":["user:inference","user:profile"],
            "subscriptionType":"team",
            "rateLimitTier":"default_claude_max_20x"
        }}"#;
        let blob = FileCredentials::parse(json, Path::new("test")).unwrap();
        assert_eq!(blob.access_token, "sk-ant-oat01-aaa");
        assert_eq!(blob.refresh_token.as_deref(), Some("sk-ant-ort01-bbb"));
        assert_eq!(blob.expires_at, Some(1788250887800));
    }

    #[test]
    fn an_expired_token_is_refused_rather_than_sent() {
        let past = OauthBlob {
            access_token: "stale".into(),
            refresh_token: None,
            expires_at: Some(1),
        };
        let err = FileCredentials::check_expiry(&past, Path::new("creds")).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");

        let far_future = OauthBlob {
            access_token: "fresh".into(),
            refresh_token: None,
            expires_at: Some(i64::MAX),
        };
        assert!(FileCredentials::check_expiry(&far_future, Path::new("creds")).is_ok());
    }

    #[tokio::test]
    async fn a_rewritten_file_is_picked_up_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let write = |token: &str| {
            std::fs::write(
                &path,
                format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":{}}}}}"#,
                    i64::MAX
                ),
            )
            .unwrap();
        };

        write("first");
        let source = FileCredentials::new(&path);
        assert_eq!(
            source.credential().await.unwrap(),
            Credential::OAuth("first".into())
        );
        // Cached: same file, no re-read needed, same answer.
        assert_eq!(
            source.credential().await.unwrap(),
            Credential::OAuth("first".into())
        );

        // A re-login rewrites the file. mtime resolution is coarse on some
        // filesystems, so the length changes too — either is enough.
        std::thread::sleep(Duration::from_millis(10));
        write("second-and-longer");
        assert_eq!(
            source.credential().await.unwrap(),
            Credential::OAuth("second-and-longer".into())
        );
    }

    #[tokio::test]
    async fn a_missing_file_says_which_one() {
        let source = FileCredentials::new("/nonexistent/.credentials.json");
        let err = source.credential().await.unwrap_err();
        assert!(err.to_string().contains(".credentials.json"), "{err}");
    }
}
