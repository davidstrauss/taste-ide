//! The second upstream: a private, Anthropic-compatible model of the user's.
//!
//! A llama.cpp server on a machine of theirs speaks the Messages API, so a
//! private model is not a new integration — it is a different *upstream*
//! for the one hop the IDE already owns. The agent, the permission cards,
//! and the transcript are unchanged; the containers still reach nothing on
//! the LAN, because the host-side proxy is what dials it (CLAUDE.md → "The
//! boundary is the host, not the agent").
//!
//! # Where the setting lives, and why
//!
//! `private-model.json` in the workspace's own state directory, beside
//! the Anthropic credential and read the same way — IDE state, never the
//! checkout, and never an environment variable the agent sees. It holds a
//! key, so it belongs on the IDE's side of the line for exactly the
//! reason [`crate::credentials`] gives for the Anthropic one, and an
//! agent that could write it could aim the IDE's own requests at a host
//! of its choosing. The user provisions it the way they provision the
//! Anthropic credential; nothing here reads any other program's storage.
//!
//! It is **per project** for the same reason, and with the same absence
//! of a fallback: a server on the user's own hardware is a thing they
//! chose for this work, one project's key is not another's, and a
//! machine-wide default would decide for a project nobody provisioned.
//! Most workspaces have no private model at all, and that stays the
//! ordinary case rather than an error (see [`discover`]).
//!
//! ```json
//! {
//!   "base_url": "http://tower.lan:9931",
//!   "kind": "api_key",
//!   "token": "…",
//!   "model": "gpt-oss-20b",
//!   "label": "gpt-oss-20b",
//!   "context_tokens": 65536
//! }
//! ```
//!
//! `kind` is how the key is sent: `api_key` for `x-api-key` (what the
//! README's own `curl` uses), or `bearer` for `Authorization: Bearer`. It
//! is the user's to say rather than the IDE's to sniff, because
//! `llama-server --api-key` is one flag whose accepted header has changed
//! across releases, and trying both in turn would mean sending the key to
//! a server that already refused it. `model`, `label`, and
//! `context_tokens` are all optional: the first two are what the settings
//! row calls this thing, and the third is the server's `-c`, so
//! the context gauge measures against the window that actually exists
//! rather than assuming Anthropic's 200k.
//!
//! # Two agents, one adapter
//!
//! **The private model is not a model of Claude Code's; it is a second
//! place for Claude Code to send its requests.** So it is offered where
//! agents are offered: "Claude Code (Private)" is a second entry in the
//! agent registry (`taste_acp::registry`), the same pinned adapter with
//! the same home, whose spawn mints a placeholder for the private upstream
//! ([`crate::Route::Private`]) instead of the API. A chat opened as it
//! spends on the user's hardware for the whole of its life; a chat opened
//! as plain "Claude Code" spends on the account; and one environment can
//! hold both at once, which is the point — real Claude Code for the work
//! that matters, the private one for what it is good enough for.
//!
//! The agent is told nothing about it, and needs nothing. Its own `model`
//! session-config option is meaningless on the private variant — the
//! server serves the one model it loaded whatever name the request
//! carries — so the chat's model drop-down is not shown there; what is
//! shown in its place is this file's contents, which is the only choice
//! there is to make.
//!
//! It used to be a row in the model drop-down that flipped a
//! per-environment route on the proxy. That made the private model look
//! like a model of the agent's, tangled the drop-down's remembered value
//! with a value no agent advertised, and — because the route was keyed by
//! environment — could not put two chats of one environment on two hosts.
//!
//! # What the private upstream does and does not carry
//!
//! Its stream is put in the documented block order on the way through —
//! see [`crate::sse`] for the fault that corrects, and why it is done on
//! this route alone.
//!
//! Spend still lands in the environment's counters: the fleet's breakdown
//! is about who drew, and an environment that spent its afternoon on the
//! free rung is worth being able to see. The account's **quota** is not
//! harvested from it, and that is not an omission — a private
//! server's response says nothing about the subscription, and a turn it
//! served is not evidence that a closed Anthropic window has reopened.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use http::Uri;
use serde::{Deserialize, Serialize};

use crate::credentials::{Credential, CredentialKind};

/// Points the proxy at a private-model file somewhere other than IDE
/// state. How a test, and a developer with two servers, aim it.
pub const PRIVATE_MODEL_PATH_VAR: &str = "TASTE_PRIVATE_MODEL";

/// What the user is told when the private upstream is asked for and
/// nothing is provisioned. One string, so the message cannot drift between the paths
/// that raise it.
const HOW_TO_PROVISION: &str = "write the endpoint and its key into the IDE's private-model file \
     (see docs/ENVIRONMENTS.md → The auth proxy → A private model)";

/// The IDE's private-model file — **its** format, like the credential
/// file beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPrivateModel {
    /// Where the server is: scheme, host, and port, with an optional path
    /// prefix, exactly as `ANTHROPIC_BASE_URL` would carry it.
    pub base_url: String,
    /// How the key is sent. Defaults to `x-api-key`, which is what the
    /// README's measurement step uses.
    #[serde(default = "default_kind")]
    pub kind: CredentialKind,
    pub token: String,
    /// The model name to show, when the user wants one. The server serves
    /// what it loaded regardless, so this is a label and never a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the settings row calls it. Defaults to `model`, then to the
    /// endpoint's authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The server's context window (`llama-server -c`), so the context
    /// gauge measures against the window that exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
}

fn default_kind() -> CredentialKind {
    CredentialKind::ApiKey
}

/// What the proxy needs in order to forward one request privately.
#[derive(Debug, Clone)]
pub struct PrivateUpstream {
    pub uri: Uri,
    pub credential: Credential,
}

/// What a surface may say about the private server without holding its
/// key: a name for the settings row, the endpoint for a tooltip, and the
/// window for a gauge. Deliberately free of the token —
/// this is the value that crosses into GTK code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateFacts {
    /// Scheme, host, and port, as the file gave them.
    pub endpoint: String,
    /// Which header carries the key — a fact about the server, not the
    /// key, so the settings form can show the choice that is in force.
    pub kind: CredentialKind,
    /// The model name the user recorded, if any.
    pub model: Option<String>,
    /// What to call it in a list.
    pub label: String,
    /// The server's context window, if the user recorded it.
    pub context_tokens: Option<u64>,
}

impl PrivateFacts {
    /// A sentence for a tooltip: what this is, and where it lives.
    pub fn describe(&self) -> String {
        match &self.model {
            Some(model) => format!("{model} on {}", self.endpoint),
            None => self.endpoint.clone(),
        }
    }
}

/// A boxed future, matching [`crate::credentials::CredentialFuture`]: the
/// file is read off the request path's own runtime worker, never on a
/// thread that draws.
pub type PrivateFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = Result<PrivateUpstream>> + Send + 'a>>;

struct Cached {
    mtime: Option<SystemTime>,
    len: u64,
    upstream: PrivateUpstream,
    facts: PrivateFacts,
}

/// The private upstream as the IDE's state file describes it, re-read
/// whenever that file's mtime or length changes.
///
/// The same shape as [`crate::credentials::FileCredentials`], and for the
/// same reason: a user who moves the server, or rotates its key, should
/// see it take effect on the next request rather than at the next launch.
pub struct FilePrivateUpstream {
    path: PathBuf,
    cache: Mutex<Option<Cached>>,
}

impl FilePrivateUpstream {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cache: Mutex::new(None),
        }
    }

    /// Build an upstream from the value the IDE just persisted.
    ///
    /// The cache makes the new facts available to the settings row and
    /// the header immediately.
    /// Its impossible file metadata ensures the next request still reads
    /// the file, so another IDE process can replace this setting normally.
    pub fn provisioned(path: impl Into<PathBuf>, stored: &StoredPrivateModel) -> Result<Self> {
        let path = path.into();
        let (upstream, facts) = compose(stored, &path)?;
        Ok(Self {
            path,
            cache: Mutex::new(Some(Cached {
                mtime: None,
                len: 0,
                upstream,
                facts,
            })),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the last successful read said, without touching the disk.
    ///
    /// The pure read the UI needs: a settings row being built on the GTK
    /// thread cannot wait on a file, and what it wants — a label, and a
    /// window — is exactly what the last parse already knows. `None` until
    /// something has read the file, which [`crate::Handle::warm_private_upstream`]
    /// arranges at start-up.
    pub fn facts(&self) -> Option<PrivateFacts> {
        let cache = self.cache.lock().ok()?;
        cache.as_ref().map(|cached| cached.facts.clone())
    }

    /// Drop the cache, so a re-provisioned key is picked up on the next
    /// request rather than at the next change of mtime. Called when the
    /// private server answers 401, which is the one moment we know the key
    /// we hold is not the key it wants.
    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    /// The upstream to forward to, re-reading the file if it has moved.
    pub fn upstream(&self) -> PrivateFuture<'_> {
        Box::pin(async move {
            let meta = tokio::fs::metadata(&self.path)
                .await
                .with_context(|| format!("reading {}", self.path.display()))?;
            let mtime = meta.modified().ok();
            let len = meta.len();
            if let Some(upstream) = self.cached_if_fresh(mtime, len) {
                return Ok(upstream);
            }
            let bytes = tokio::fs::read(&self.path)
                .await
                .with_context(|| format!("reading {}", self.path.display()))?;
            let stored = parse(&bytes, &self.path)?;
            let (upstream, facts) = compose(&stored, &self.path)?;
            if let Ok(mut cache) = self.cache.lock() {
                *cache = Some(Cached {
                    mtime,
                    len,
                    upstream: upstream.clone(),
                    facts,
                });
            }
            Ok(upstream)
        })
    }

    fn cached_if_fresh(&self, mtime: Option<SystemTime>, len: u64) -> Option<PrivateUpstream> {
        let cache = self.cache.lock().ok()?;
        let cached = cache.as_ref()?;
        (cached.mtime == mtime && cached.len == len).then(|| cached.upstream.clone())
    }
}

fn parse(bytes: &[u8], path: &Path) -> Result<StoredPrivateModel> {
    let stored: StoredPrivateModel =
        serde_json::from_slice(bytes).with_context(|| format!("parsing {}", path.display()))?;
    anyhow::ensure!(
        !stored.token.trim().is_empty(),
        "{} holds an empty key; {HOW_TO_PROVISION}",
        path.display()
    );
    Ok(stored)
}

/// Turn the file into the two things the rest of the crate wants: a
/// request's destination and credential, and a sentence about it.
fn compose(stored: &StoredPrivateModel, path: &Path) -> Result<(PrivateUpstream, PrivateFacts)> {
    let uri: Uri = stored
        .base_url
        .trim()
        .parse()
        .with_context(|| format!("{} holds a base_url that is not a URI", path.display()))?;
    anyhow::ensure!(
        uri.authority().is_some(),
        "the private model's base_url {uri} has no host; {HOW_TO_PROVISION}"
    );
    let endpoint = match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}"),
        (None, Some(authority)) => authority.to_string(),
        _ => stored.base_url.clone(),
    };
    let label = stored
        .label
        .clone()
        .or_else(|| stored.model.clone())
        .unwrap_or_else(|| {
            uri.authority()
                .map(|authority| authority.to_string())
                .unwrap_or_else(|| endpoint.clone())
        });
    Ok((
        PrivateUpstream {
            uri,
            credential: stored.kind.credential(stored.token.trim()),
        },
        PrivateFacts {
            endpoint,
            kind: stored.kind,
            model: stored.model.clone(),
            label,
            context_tokens: stored.context_tokens,
        },
    ))
}

/// `private-model.json` in this project's state directory, beside its
/// credential file — IDE-owned state, keyed by the checkout's root and
/// never inside it, never another program's directory.
pub fn private_model_path(workspace_root: &Path) -> PathBuf {
    crate::credentials::credential_path(workspace_root).with_file_name("private-model.json")
}

/// Validate and persist the private upstream for this project, and hand
/// back what was written: the value as stored, and the facts about it.
///
/// The caller is the IDE's own settings surface. Agents never receive this
/// path or its token, and the file remains project-scoped IDE state beside
/// the Anthropic credential.
///
/// **An empty `token` keeps the key already on file.** The form that
/// calls this never holds the key — it is written here and read back
/// only by the proxy — so a user changing the endpoint, the model name,
/// or the window must not have to retype the key to do it. With nothing
/// on file, an empty key is refused with the fix, never written.
pub async fn store(
    workspace_root: &Path,
    stored: StoredPrivateModel,
) -> Result<(StoredPrivateModel, PrivateFacts)> {
    let path = private_model_path(workspace_root);
    store_at(&path, stored).await
}

async fn store_at(
    path: &Path,
    mut stored: StoredPrivateModel,
) -> Result<(StoredPrivateModel, PrivateFacts)> {
    if stored.token.trim().is_empty() {
        let existing = match tokio::fs::read(path).await {
            Ok(bytes) => parse(&bytes, path).ok().map(|existing| existing.token),
            Err(_) => None,
        };
        stored.token = existing.context(
            "no API key is stored for this project's private model yet; enter the server's key",
        )?;
    }
    let (_, facts) = compose(&stored, path)?;
    let bytes = serde_json::to_vec_pretty(&stored).context("serializing private model")?;
    let parent = path
        .parent()
        .context("private-model path has no parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    tokio::fs::write(&path, bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("securing {}", path.display()))?;
    Ok((stored, facts))
}

/// The private upstream the user provisioned **for this project**, if they
/// provisioned one.
///
/// `None` is the ordinary case and never an error: most workspaces have no
/// private model, and a proxy with one upstream is what this crate shipped
/// as. An aimed-at path that does not exist is the same absence — the
/// variable is a developer's aim, not an assertion that a file is there.
/// Another project's file is not consulted, exactly as its credential is
/// not.
pub fn discover(workspace_root: &Path) -> Option<FilePrivateUpstream> {
    let path = match std::env::var_os(PRIVATE_MODEL_PATH_VAR) {
        Some(aimed) if !aimed.is_empty() => PathBuf::from(aimed),
        _ => private_model_path(workspace_root),
    };
    path.exists().then(|| FilePrivateUpstream::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_file_says_which_header_carries_the_key() {
        let bearer = parse(
            br#"{"base_url":"http://tower.lan:9931","kind":"bearer","token":"k"}"#,
            Path::new("private-model.json"),
        )
        .unwrap();
        assert_eq!(bearer.kind, CredentialKind::OauthToken);
        // ...and `x-api-key` is what it is when nobody says, because that
        // is the header the README's own measurement step uses.
        let default = parse(
            br#"{"base_url":"http://tower.lan:9931","token":"k"}"#,
            Path::new("private-model.json"),
        )
        .unwrap();
        assert_eq!(default.kind, CredentialKind::ApiKey);
        let (upstream, _) = compose(&default, Path::new("private-model.json")).unwrap();
        assert_eq!(upstream.credential, Credential::ApiKey("k".into()));
    }

    #[test]
    fn the_label_falls_back_to_the_model_then_to_the_host() {
        let named = StoredPrivateModel {
            base_url: "http://tower.lan:9931".into(),
            kind: CredentialKind::ApiKey,
            token: "k".into(),
            model: Some("gpt-oss-20b".into()),
            label: None,
            context_tokens: Some(65_536),
        };
        let (_, facts) = compose(&named, Path::new("p")).unwrap();
        assert_eq!(facts.label, "gpt-oss-20b");
        assert_eq!(facts.endpoint, "http://tower.lan:9931");
        assert_eq!(facts.describe(), "gpt-oss-20b on http://tower.lan:9931");
        assert_eq!(facts.context_tokens, Some(65_536));

        let anonymous = StoredPrivateModel {
            model: None,
            ..named
        };
        let (_, facts) = compose(&anonymous, Path::new("p")).unwrap();
        assert_eq!(facts.label, "tower.lan:9931");
        assert_eq!(facts.describe(), "http://tower.lan:9931");
    }

    #[test]
    fn a_base_url_with_no_host_is_refused_with_the_fix() {
        let stored = StoredPrivateModel {
            base_url: "/v1".into(),
            kind: CredentialKind::ApiKey,
            token: "k".into(),
            model: None,
            label: None,
            context_tokens: None,
        };
        let err = compose(&stored, Path::new("p")).unwrap_err().to_string();
        assert!(err.contains("no host"), "{err}");
        assert!(err.contains("private-model file"), "{err}");
    }

    #[test]
    fn an_empty_key_is_refused_rather_than_sent() {
        let err = parse(
            br#"{"base_url":"http://tower.lan:9931","token":"  "}"#,
            Path::new("private-model.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty key"), "{err}");
    }

    #[tokio::test]
    async fn a_moved_server_is_picked_up_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-model.json");
        let write = |host: &str| {
            std::fs::write(
                &path,
                format!(r#"{{"base_url":"http://{host}","token":"k","model":"gpt-oss-20b"}}"#),
            )
            .unwrap();
        };

        write("tower.lan:9931");
        let source = FilePrivateUpstream::new(&path);
        // Nothing read, nothing to say: the settings row has no name for
        // it until the file has been looked at once.
        assert_eq!(source.facts(), None);
        assert_eq!(
            source.upstream().await.unwrap().uri.to_string(),
            "http://tower.lan:9931/"
        );
        assert_eq!(source.facts().unwrap().label, "gpt-oss-20b");

        // mtime resolution is coarse on some filesystems, so the length
        // changes too — either is enough.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write("workshop.lan:18080");
        assert_eq!(
            source.upstream().await.unwrap().uri.to_string(),
            "http://workshop.lan:18080/"
        );
    }

    #[test]
    fn the_file_sits_beside_the_credential_it_is_not() {
        let root = Path::new("/work/project");
        let credential = crate::credentials::credential_path(root);
        let private = private_model_path(root);
        assert_eq!(private.parent(), credential.parent());
        assert_eq!(
            private.file_name().unwrap().to_str(),
            Some("private-model.json")
        );
        // ...and scopes the same way it does: keyed by the root, outside
        // the checkout, and different for a different project.
        assert!(!private.starts_with(root), "{}", private.display());
        assert_ne!(private, private_model_path(Path::new("/elsewhere/project")));
    }

    #[tokio::test]
    async fn storing_a_private_model_keeps_its_key_in_project_state() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("private-model.json");
        let stored = StoredPrivateModel {
            base_url: "http://tower.lan:9931".into(),
            kind: CredentialKind::ApiKey,
            token: "secret".into(),
            model: Some("gpt-oss-20b".into()),
            label: None,
            context_tokens: Some(65_536),
        };
        let (written, facts) = store_at(&path, stored.clone()).await.unwrap();
        assert_eq!(facts.label, "gpt-oss-20b");
        assert_eq!(facts.kind, CredentialKind::ApiKey);
        assert_eq!(
            tokio::fs::read(&path).await.unwrap(),
            serde_json::to_vec_pretty(&stored).unwrap()
        );
        assert_eq!(written.token, "secret");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// The form never holds the key, so a save that leaves it blank keeps
    /// the one on file — and with none on file, refuses rather than writes
    /// a model nothing can authenticate to.
    #[tokio::test]
    async fn a_blank_key_keeps_the_stored_one_and_is_refused_without_one() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("private-model.json");
        let fresh = StoredPrivateModel {
            base_url: "http://tower.lan:9931".into(),
            kind: CredentialKind::ApiKey,
            token: "  ".into(),
            model: Some("gpt-oss-20b".into()),
            label: None,
            context_tokens: None,
        };
        let err = store_at(&path, fresh.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("enter the server's key"), "{err}");
        assert!(!path.exists(), "nothing was written");

        store_at(
            &path,
            StoredPrivateModel {
                token: "secret".into(),
                ..fresh.clone()
            },
        )
        .await
        .unwrap();
        let (written, facts) = store_at(
            &path,
            StoredPrivateModel {
                base_url: "http://moved.lan:8081".into(),
                kind: CredentialKind::OauthToken,
                ..fresh
            },
        )
        .await
        .unwrap();
        assert_eq!(written.token, "secret");
        assert_eq!(facts.endpoint, "http://moved.lan:8081");
        assert_eq!(facts.kind, CredentialKind::OauthToken);
        let reread = parse(&tokio::fs::read(&path).await.unwrap(), &path).unwrap();
        assert_eq!(reread.token, "secret");
    }

    #[tokio::test]
    async fn a_just_stored_model_is_available_without_rereading_it() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("private-model.json");
        let stored = StoredPrivateModel {
            base_url: "http://tower.lan:9931".into(),
            kind: CredentialKind::ApiKey,
            token: "secret".into(),
            model: Some("gpt-oss-20b".into()),
            label: None,
            context_tokens: Some(65_536),
        };
        let (stored, _) = store_at(&path, stored).await.unwrap();

        let source = FilePrivateUpstream::provisioned(&path, &stored).unwrap();
        assert_eq!(source.facts().unwrap().label, "gpt-oss-20b");
        assert_eq!(
            source.upstream().await.unwrap().uri.to_string(),
            "http://tower.lan:9931/"
        );
    }
}
