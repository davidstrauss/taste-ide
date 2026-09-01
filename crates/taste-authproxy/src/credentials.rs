//! Where the real credential comes from, and how it becomes headers.
//!
//! # The rule this module exists to enforce
//!
//! **The IDE holds a credential the user deliberately gave it, stored
//! where the IDE keeps its own state.** It does not read, parse, or reuse
//! any other program's credential storage. Claude Code's
//! `~/.claude/.credentials.json` is that program's private storage — a
//! file it manages through `/login` and `/logout`, not an interface —
//! and an earlier version of this module parsed it. That was wrong on
//! two counts: it coupled taste-ide to an undocumented on-disk shape that
//! is free to change, and it made the IDE a second consumer of a grant
//! issued to a different client. Both are gone.
//!
//! # The two intended surfaces
//!
//! Anthropic documents exactly two credentials a program may be *given*
//! for programmatic use, and this module accepts those and nothing else:
//!
//! 1. **An API key** — a Console key, sent as `x-api-key`. Never expires
//!    on a clock, so nothing here has to refresh it.
//! 2. **A long-lived OAuth token** from `claude setup-token`, which the
//!    Claude Code docs describe as "a one-year OAuth token" for "CI
//!    pipelines, scripts, or other environments where interactive browser
//!    login isn't available". It prints to the terminal and is *not*
//!    saved anywhere by Claude Code — the user pastes it where it is
//!    wanted, which is precisely the act of provisioning this IDE.
//!
//! # Why there is no refresh code
//!
//! A one-year token and a non-expiring key both outlive any session, so
//! the refresh problem dissolves rather than being solved. There is no
//! token endpoint here, no client id, no refresh grant. When a credential
//! does eventually stop working, the answer is the same as at setup: the
//! user provisions a new one, and the error message says so by name.
//!
//! # The other half of the arrangement
//!
//! What the *agent* gets is also documented, and is what makes the proxy
//! idiomatic rather than a trick: `ANTHROPIC_BASE_URL` ("to route
//! requests through a custom API endpoint") plus `ANTHROPIC_AUTH_TOKEN`,
//! whose documented purpose is "routing through an LLM gateway or proxy
//! that authenticates with bearer tokens". The IDE is that gateway.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use http::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};

/// Header carrying a plain Anthropic API key.
pub const X_API_KEY: &str = "x-api-key";

/// Environment variable holding a Console API key.
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";

/// Environment variable holding a `claude setup-token` long-lived token.
pub const CLAUDE_CODE_OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Points the proxy at a credential file somewhere other than IDE state.
/// How the live test and a developer aim it at provisioned material.
pub const CREDENTIAL_PATH_VAR: &str = "TASTE_ANTHROPIC_CREDENTIALS";

/// What the user is told to run when there is no usable credential. One
/// string so the message cannot drift between the paths that raise it.
const HOW_TO_PROVISION: &str = "provision one: set ANTHROPIC_API_KEY, or run `claude setup-token` \
     and put the token in the IDE's credential file (see docs/ENVIRONMENTS.md → The auth proxy)";

/// Treat a token as expired this long before it actually is, so a request
/// does not race the clock on the way to the API.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// What the proxy puts on an outbound request in place of the placeholder.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A Console API key: `x-api-key`.
    ApiKey(String),
    /// A long-lived OAuth token: `Authorization: Bearer`.
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
/// an async-trait macro (reading a file should not block a runtime worker).
pub type CredentialFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = Result<Credential>> + Send + 'a>>;

/// Where the proxy gets the credential it substitutes.
pub trait CredentialSource: Send + Sync + 'static {
    /// The credential to use for the next request.
    fn credential(&self) -> CredentialFuture<'_>;

    /// Drop any cache. Called when the upstream answers 401, so a
    /// credential the user has just re-provisioned is picked up on the
    /// next attempt rather than at the next IDE restart.
    fn invalidate(&self) {}
}

/// A credential that never changes: one read from the IDE's environment.
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

/// Which of the two intended credentials the IDE was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A Console API key, sent as `x-api-key`.
    ApiKey,
    /// A `claude setup-token` token, sent as `Authorization: Bearer`.
    OauthToken,
}

/// The IDE's own credential file — **its** format, not anyone else's.
///
/// Deliberately minimal and self-describing: the user pastes one token in
/// and says which kind it is. `expires_at_ms` is optional because
/// `claude setup-token` prints a bare token with no expiry metadata; when
/// the user knows the date (a year out) recording it buys a clear error
/// slightly before the API starts refusing, and omitting it costs only
/// that head start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    pub kind: CredentialKind,
    pub token: String,
    /// Milliseconds since the epoch, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl StoredCredential {
    fn as_credential(&self) -> Credential {
        match self.kind {
            CredentialKind::ApiKey => Credential::ApiKey(self.token.clone()),
            CredentialKind::OauthToken => Credential::OAuth(self.token.clone()),
        }
    }
}

struct Cached {
    mtime: Option<SystemTime>,
    len: u64,
    stored: StoredCredential,
}

/// A credential read from the IDE's credential file, re-read whenever the
/// file's mtime or length changes so a re-provision lands without a
/// restart.
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

    /// The cached credential, if the file on disk still matches what
    /// produced it.
    fn cached_if_fresh(&self, mtime: Option<SystemTime>, len: u64) -> Option<StoredCredential> {
        let cache = self.cache.lock().ok()?;
        let cached = cache.as_ref()?;
        (cached.mtime == mtime && cached.len == len).then(|| cached.stored.clone())
    }

    fn parse(bytes: &[u8], path: &Path) -> Result<StoredCredential> {
        let stored: StoredCredential = serde_json::from_slice(bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            !stored.token.trim().is_empty(),
            "{} holds an empty token; {HOW_TO_PROVISION}",
            path.display()
        );
        Ok(stored)
    }

    fn check_expiry(stored: &StoredCredential, path: &Path) -> Result<()> {
        let Some(expires_at) = stored.expires_at_ms else {
            return Ok(());
        };
        let deadline = SystemTime::now() + EXPIRY_SKEW;
        let deadline_ms = deadline
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        anyhow::ensure!(
            expires_at > deadline_ms,
            "the Anthropic credential in {} has expired; {HOW_TO_PROVISION}",
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

            if let Some(stored) = self.cached_if_fresh(mtime, len) {
                Self::check_expiry(&stored, &self.path)?;
                return Ok(stored.as_credential());
            }

            let bytes = tokio::fs::read(&self.path)
                .await
                .with_context(|| format!("reading {}", self.path.display()))?;
            let stored = Self::parse(&bytes, &self.path)?;
            if let Ok(mut cache) = self.cache.lock() {
                *cache = Some(Cached {
                    mtime,
                    len,
                    stored: stored.clone(),
                });
            }
            Self::check_expiry(&stored, &self.path)?;
            Ok(stored.as_credential())
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
/// Resolution touches the filesystem, and agent spawns are composed on the
/// GTK main thread, which never waits on IO. Deferring to the first
/// proxied request puts the work on a runtime worker.
#[derive(Default)]
pub struct IdeCredentials {
    resolved: tokio::sync::OnceCell<Arc<dyn CredentialSource>>,
}

impl IdeCredentials {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialSource for IdeCredentials {
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

/// The IDE's credential file: `$XDG_STATE_HOME/taste-ide/anthropic.json`.
///
/// IDE-owned state, beside the rest of it — never another program's
/// directory.
pub fn credential_path() -> Option<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(state.join("taste-ide/anthropic.json"))
}

/// Find the credential the user gave this IDE.
///
/// Order is "most explicit wins": an aimed-at file, then either documented
/// environment variable, then IDE state. Nothing here searches another
/// program's storage.
pub async fn discover() -> Result<Arc<dyn CredentialSource>> {
    if let Some(path) = std::env::var_os(CREDENTIAL_PATH_VAR) {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            path.exists(),
            "{CREDENTIAL_PATH_VAR} points at {} which does not exist",
            path.display()
        );
        return Ok(Arc::new(FileCredentials::new(path)));
    }

    // Documented, and the shape CI already uses. An API key first: it is
    // the one credential with no expiry story at all.
    if let Ok(key) = std::env::var(ANTHROPIC_API_KEY) {
        if !key.trim().is_empty() {
            return Ok(Arc::new(StaticKey::api_key(key)));
        }
    }
    if let Ok(token) = std::env::var(CLAUDE_CODE_OAUTH_TOKEN) {
        if !token.trim().is_empty() {
            return Ok(Arc::new(StaticKey::oauth(token)));
        }
    }

    if let Some(path) = credential_path() {
        if path.exists() {
            return Ok(Arc::new(FileCredentials::new(path)));
        }
        anyhow::bail!(
            "no Anthropic credential for the IDE ({} does not exist); {HOW_TO_PROVISION}",
            path.display()
        );
    }

    anyhow::bail!("no Anthropic credential for the IDE; {HOW_TO_PROVISION}")
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
        let rendered = format!("{:?}", Credential::OAuth("a-secret-token".into()));
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn the_ide_credential_file_round_trips() {
        let stored = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "provisioned-token".into(),
            expires_at_ms: Some(1_788_250_887_800),
        };
        let json = serde_json::to_vec(&stored).unwrap();
        let parsed = FileCredentials::parse(&json, Path::new("test")).unwrap();
        assert_eq!(parsed.kind, CredentialKind::OauthToken);
        assert_eq!(parsed.token, "provisioned-token");
        assert_eq!(parsed.expires_at_ms, Some(1_788_250_887_800));

        // A setup-token has no expiry metadata to record, so the field is
        // optional on the way in and absent on the way out.
        let bare = br#"{"kind":"api_key","token":"sk-key"}"#;
        let parsed = FileCredentials::parse(bare, Path::new("test")).unwrap();
        assert_eq!(parsed.kind, CredentialKind::ApiKey);
        assert_eq!(parsed.expires_at_ms, None);
        let round = String::from_utf8(serde_json::to_vec(&parsed).unwrap()).unwrap();
        assert!(!round.contains("expires_at_ms"), "{round}");
    }

    #[test]
    fn each_kind_becomes_the_header_the_docs_specify() {
        let key = StoredCredential {
            kind: CredentialKind::ApiKey,
            token: "k".into(),
            expires_at_ms: None,
        };
        assert_eq!(key.as_credential(), Credential::ApiKey("k".into()));

        let oauth = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "t".into(),
            expires_at_ms: None,
        };
        assert_eq!(oauth.as_credential(), Credential::OAuth("t".into()));
    }

    #[test]
    fn an_empty_token_is_refused_with_the_fix() {
        let err = FileCredentials::parse(
            br#"{"kind":"api_key","token":"   "}"#,
            Path::new("creds"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("setup-token"), "{err}");
    }

    #[test]
    fn a_known_expiry_is_refused_rather_than_sent_and_says_how_to_fix_it() {
        let past = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "stale".into(),
            expires_at_ms: Some(1),
        };
        let err = FileCredentials::check_expiry(&past, Path::new("creds")).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        assert!(err.to_string().contains("setup-token"), "{err}");

        // A year-long token, and one with no recorded expiry, both pass.
        let far_future = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "fresh".into(),
            expires_at_ms: Some(i64::MAX),
        };
        assert!(FileCredentials::check_expiry(&far_future, Path::new("creds")).is_ok());
        let unknown = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "fresh".into(),
            expires_at_ms: None,
        };
        assert!(FileCredentials::check_expiry(&unknown, Path::new("creds")).is_ok());
    }

    #[tokio::test]
    async fn a_re_provisioned_file_is_picked_up_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anthropic.json");
        let write = |token: &str| {
            std::fs::write(
                &path,
                format!(r#"{{"kind":"oauth_token","token":"{token}"}}"#),
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

        // mtime resolution is coarse on some filesystems, so the length
        // changes too — either is enough.
        std::thread::sleep(Duration::from_millis(10));
        write("second-and-longer");
        assert_eq!(
            source.credential().await.unwrap(),
            Credential::OAuth("second-and-longer".into())
        );
    }

    #[tokio::test]
    async fn a_missing_file_says_which_one() {
        let source = FileCredentials::new("/nonexistent/anthropic.json");
        let err = source.credential().await.unwrap_err();
        assert!(err.to_string().contains("anthropic.json"), "{err}");
    }

    /// The point of the rewrite, as an assertion: nothing in this module
    /// knows where any other program keeps its credentials.
    ///
    /// Comments are excluded on purpose — the module header *discusses*
    /// the storage it deliberately stopped reading, and explaining the
    /// rule must not trip it. Only executable code is checked.
    #[test]
    fn no_other_programs_credential_storage_is_referenced() {
        let source = include_str!("credentials.rs");
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("module body precedes its tests");
        let code: String = body
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for forbidden in ["claudeAiOauth", ".credentials.json", ".claude"] {
            assert!(
                !code.contains(forbidden),
                "module code references {forbidden}, which belongs to another program"
            );
        }
    }
}
