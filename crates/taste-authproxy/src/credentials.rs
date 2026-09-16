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
//!
//! # The credential is the PROJECT's, and there is no fallback
//!
//! Authenticating one project must not authenticate another: work
//! projects use work agent APIs, personal ones a personal account, on one
//! machine, with nothing to remember (David, 2026-09-16: "I don't want to
//! auth my personal projects to certain work agent APIs, for example").
//! So the file is keyed by the workspace root — [`credential_path`] —
//! and [`discover`] consults **no machine-wide file at all** — nor does
//! anything else here, not even to offer one (David, 2026-09-16: "Never
//! offer to import system credentials into a project. Always require
//! project-level creds"). A project with none refuses its first request
//! naming the provisioning step, exactly as an unprovisioned IDE did
//! before; the absence of a default is the point, because a machine-wide
//! default is precisely the mechanism that would let a work key into a
//! personal project.
//!
//! Project-scoped means *keyed by* the checkout's root, never *inside*
//! the checkout. Nothing is written into the working copy, hidden or
//! otherwise, so nothing can be committed (David: "I don't want it
//! actually in the working copy, even as a hidden file, because it risks
//! getting committed"). The file lives beside the workspace's own state
//! file, in the directory `taste_core::state::workspace_state_dir` names
//! by a hash of the root — which has two consequences worth saying out
//! loud: the same repository cloned at two paths is two scopes, each
//! provisioned on its own, and an environment's clone has no file of its
//! own at all, because the clone's proxy is the workspace's proxy and the
//! clone only ever sees a placeholder.

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
///
/// Machine-wide by nature, like the two environment variables above: it
/// names one file for whatever workspace this process opened. That is
/// what "aimed" means, and it is why it is a developer's tool rather
/// than the way anybody provisions a project.
pub const CREDENTIAL_PATH_VAR: &str = "TASTE_ANTHROPIC_CREDENTIALS";

/// What the user is told to run when there is no usable credential. One
/// string so the message cannot drift between the paths that raise it.
///
/// "this project's" rather than "the IDE's" because that is the scope
/// rule as a user meets it: another project on this machine may well be
/// provisioned, and none of its credential reaches here.
const HOW_TO_PROVISION: &str = "provision one: set ANTHROPIC_API_KEY, or run `claude setup-token` \
     and put the token in this project's credential file (see docs/ENVIRONMENTS.md → \
     The auth proxy)";

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

    /// What the user calls this identity — "work", "personal" — if they
    /// named it.
    ///
    /// A **pure read**, and deliberately so: it is what the chat header
    /// and the Utilization tab show, and those are drawn on the GTK
    /// thread, which never waits on a file. So this answers out of
    /// whatever the last read already parsed and is `None` until
    /// something has read the file (`Handle::warm_credentials` arranges
    /// that at start-up). Never the token, and never anything derived
    /// from it.
    fn label(&self) -> Option<String> {
        None
    }
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
///
/// It doubles as the answer to "which header carries this key" for the
/// private upstream ([`crate::private`]), where the words `api_key` and
/// `oauth_token` would be wrong about a llama.cpp server's key — hence the
/// aliases, which are the same two headers under names that fit the other
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A Console API key, sent as `x-api-key`.
    #[serde(alias = "x_api_key")]
    ApiKey,
    /// A `claude setup-token` token, sent as `Authorization: Bearer`.
    #[serde(alias = "bearer")]
    OauthToken,
}

impl CredentialKind {
    /// This kind of key, ready to be applied to a request's headers.
    pub fn credential(self, token: impl Into<String>) -> Credential {
        match self {
            CredentialKind::ApiKey => Credential::ApiKey(token.into()),
            CredentialKind::OauthToken => Credential::OAuth(token.into()),
        }
    }
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
    /// What the user calls this identity: "work", "personal".
    ///
    /// Optional, and worth nothing to the proxy — it is never sent
    /// anywhere and never affects which credential is used. It exists so
    /// that a person with a work account and a personal one can see,
    /// where their spend is shown, which of them this project is on. A
    /// file with no label is the ordinary case and says nothing in the
    /// header (see `taste_app::chat` → the Plan slot): the label's whole
    /// purpose is to tell two identities apart, and a user with one has
    /// nothing to tell apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl StoredCredential {
    fn as_credential(&self) -> Credential {
        self.kind.credential(self.token.clone())
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

    /// What the last successful read said this identity is called.
    ///
    /// The pure read [`CredentialSource::label`] promises: no disk, no
    /// waiting, and `None` until something has read the file.
    fn cached_label(&self) -> Option<String> {
        let cache = self.cache.lock().ok()?;
        cache.as_ref()?.stored.label.clone()
    }

    /// The cached credential, if the file on disk still matches what
    /// produced it.
    fn cached_if_fresh(&self, mtime: Option<SystemTime>, len: u64) -> Option<StoredCredential> {
        let cache = self.cache.lock().ok()?;
        let cached = cache.as_ref()?;
        (cached.mtime == mtime && cached.len == len).then(|| cached.stored.clone())
    }

    fn parse(bytes: &[u8], path: &Path) -> Result<StoredCredential> {
        let stored: StoredCredential =
            serde_json::from_slice(bytes).with_context(|| format!("parsing {}", path.display()))?;
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

    fn label(&self) -> Option<String> {
        self.cached_label()
    }
}

/// A source that works out *which* source to be on first use.
///
/// Resolution touches the filesystem, and agent spawns are composed on the
/// GTK main thread, which never waits on IO. Deferring to the first
/// proxied request puts the work on a runtime worker.
///
/// It is built with the workspace root because the credential is the
/// *project's* — see this module's header. One proxy per IDE process and
/// one process per workspace means one of these per workspace, so the
/// root is known once, at the top, and nothing downstream has to carry
/// it.
pub struct IdeCredentials {
    workspace_root: PathBuf,
    resolved: tokio::sync::OnceCell<Arc<dyn CredentialSource>>,
}

impl IdeCredentials {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            resolved: tokio::sync::OnceCell::new(),
        }
    }

    /// Which project's credential this is.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
}

impl CredentialSource for IdeCredentials {
    fn credential(&self) -> CredentialFuture<'_> {
        Box::pin(async move {
            let inner = self
                .resolved
                .get_or_try_init(|| async { discover(&self.workspace_root).await })
                .await?;
            inner.credential().await
        })
    }

    fn invalidate(&self) {
        if let Some(inner) = self.resolved.get() {
            inner.invalidate();
        }
    }

    fn label(&self) -> Option<String> {
        self.resolved.get()?.label()
    }
}

/// This project's credential file: `anthropic.json` in the workspace's own
/// state directory (`taste_core::state::workspace_state_dir`), which is
/// `$XDG_STATE_HOME/taste-ide/workspaces/<name>-<hash of the root>/`.
///
/// IDE-owned state, beside the rest of this workspace's — never another
/// program's directory, and **never inside the checkout**: the key is the
/// root's hash, so nothing lands in the working copy where it could be
/// committed.
pub fn credential_path(workspace_root: &Path) -> PathBuf {
    taste_core::state::workspace_state_dir(workspace_root).join("anthropic.json")
}

/// Find the credential the user gave this IDE **for this project**.
///
/// Order is "most explicit wins": an aimed-at file, then either documented
/// environment variable, then this project's file, then **nothing**.
/// Nothing here searches another program's storage, and nothing here
/// falls back to a machine-wide file — a project nobody provisioned is
/// unprovisioned, however many others on this machine are not. See the
/// module header for why that absence is the feature.
pub async fn discover(workspace_root: &Path) -> Result<Arc<dyn CredentialSource>> {
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

    let path = credential_path(workspace_root);
    if path.exists() {
        return Ok(Arc::new(FileCredentials::new(path)));
    }
    anyhow::bail!(
        "no Anthropic credential for this project ({} does not exist); {HOW_TO_PROVISION}",
        path.display()
    )
}

// There is deliberately nothing here about the machine-wide file this IDE
// read before credentials were the project's
// (`$XDG_STATE_HOME/taste-ide/anthropic.json`). It is not read, not even
// to offer it: an offer to copy a machine's credential into a project is
// still a machine-wide credential reaching a project, one click at a time
// (David, 2026-09-16: "Never offer to import system credentials into a
// project. Always require project-level creds"). A project is provisioned
// with its own file or it is unprovisioned, and the refusal names the
// file to write.

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
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
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
            label: Some("work".into()),
        };
        let json = serde_json::to_vec(&stored).unwrap();
        let parsed = FileCredentials::parse(&json, Path::new("test")).unwrap();
        assert_eq!(parsed.kind, CredentialKind::OauthToken);
        assert_eq!(parsed.token, "provisioned-token");
        assert_eq!(parsed.expires_at_ms, Some(1_788_250_887_800));
        assert_eq!(parsed.label.as_deref(), Some("work"));

        // A setup-token has no expiry metadata to record, and most people
        // have one account to name, so both fields are optional on the way
        // in and absent on the way out.
        let bare = br#"{"kind":"api_key","token":"sk-key"}"#;
        let parsed = FileCredentials::parse(bare, Path::new("test")).unwrap();
        assert_eq!(parsed.kind, CredentialKind::ApiKey);
        assert_eq!(parsed.expires_at_ms, None);
        assert_eq!(parsed.label, None);
        let round = String::from_utf8(serde_json::to_vec(&parsed).unwrap()).unwrap();
        assert!(!round.contains("expires_at_ms"), "{round}");
        assert!(!round.contains("label"), "{round}");
    }

    /// The label is a pure read of what was already parsed, so the header
    /// can ask for it on the GTK thread — and it is `None` until something
    /// has read the file, which is what the start-up warm read is for.
    #[tokio::test]
    async fn the_label_is_readable_without_touching_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anthropic.json");
        std::fs::write(
            &path,
            r#"{"kind":"api_key","token":"k","label":"personal"}"#,
        )
        .unwrap();

        let source = FileCredentials::new(&path);
        assert_eq!(source.label(), None, "nothing read yet, nothing to say");
        source.credential().await.unwrap();
        assert_eq!(source.label().as_deref(), Some("personal"));

        // An environment variable carries no identity to name: it is one
        // credential for whatever this process opened.
        assert_eq!(StaticKey::api_key("k").label(), None);
    }

    #[test]
    fn each_kind_becomes_the_header_the_docs_specify() {
        let key = StoredCredential {
            kind: CredentialKind::ApiKey,
            token: "k".into(),
            expires_at_ms: None,
            label: None,
        };
        assert_eq!(key.as_credential(), Credential::ApiKey("k".into()));

        let oauth = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "t".into(),
            expires_at_ms: None,
            label: None,
        };
        assert_eq!(oauth.as_credential(), Credential::OAuth("t".into()));
    }

    #[test]
    fn an_empty_token_is_refused_with_the_fix() {
        let err =
            FileCredentials::parse(br#"{"kind":"api_key","token":"   "}"#, Path::new("creds"))
                .unwrap_err();
        assert!(err.to_string().contains("setup-token"), "{err}");
    }

    #[test]
    fn a_known_expiry_is_refused_rather_than_sent_and_says_how_to_fix_it() {
        let past = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "stale".into(),
            expires_at_ms: Some(1),
            label: None,
        };
        let err = FileCredentials::check_expiry(&past, Path::new("creds")).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        assert!(err.to_string().contains("setup-token"), "{err}");

        // A year-long token, and one with no recorded expiry, both pass.
        let far_future = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "fresh".into(),
            expires_at_ms: Some(i64::MAX),
            label: None,
        };
        assert!(FileCredentials::check_expiry(&far_future, Path::new("creds")).is_ok());
        let unknown = StoredCredential {
            kind: CredentialKind::OauthToken,
            token: "fresh".into(),
            expires_at_ms: None,
            label: None,
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

    /// The invariant the whole scope rests on: the credential is keyed BY
    /// the checkout's root and lives nowhere near it. A path computed
    /// under the working copy — hidden or not — is a secret one `git add
    /// -A` away from a commit.
    #[test]
    fn the_credential_is_keyed_by_the_root_and_never_inside_it() {
        let root = Path::new("/work/project");
        let path = credential_path(root);
        assert_eq!(path.file_name().unwrap().to_str(), Some("anthropic.json"));
        assert!(
            !path.starts_with(root),
            "{} is in the checkout",
            path.display()
        );
        assert!(
            path.starts_with(taste_core::state::workspace_state_dir(root)),
            "{}",
            path.display()
        );

        // Two projects are two scopes — including two clones of one
        // repository, which is why the key is the path and not its name.
        let elsewhere = Path::new("/elsewhere/project");
        assert_ne!(credential_path(root), credential_path(elsewhere));
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
