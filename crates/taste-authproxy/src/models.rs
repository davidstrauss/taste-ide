//! What the credential can run, asked of the documented Models API.
//!
//! The proxy holds the account's credential and the agent holds a
//! placeholder, so the agent never learns which models the account has
//! beyond Claude Code's built-in picker — a subscription's Fable row is
//! reported to a Claude Code that holds the login, and this one does not
//! (see `taste_acp::authproxy::spawn_env`). The proxy can ask on its
//! behalf: `GET /v1/models` lists the models a credential may use, by id,
//! name, age and context window, and that listing is the generic answer
//! to "which Fable" — the newest one the account can run, spelled as the
//! API spells it, rather than a release name compiled into the IDE.
//!
//! The listing is cached in the IDE's own state, so the second launch has
//! it before the first chat spawns and the first launch has it from the
//! second spawn on. A cache is a cache: a version mismatch or a parse
//! failure discards it (alpha rule — no migration, ever).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One row of the Models API's listing — the fields this crate reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelListing {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    /// RFC 3339, as the API gives it; compared as text, which sorts it.
    #[serde(default)]
    pub created_at: String,
    /// The context window, when the API reports one (it has since 2026-03).
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
}

impl ModelListing {
    /// Whether this model takes a million tokens of context — the fact the
    /// `[1m]` hint carries in Claude Code's own model spellings.
    pub fn has_1m_context(&self) -> bool {
        self.max_input_tokens
            .is_some_and(|tokens| tokens >= 1_000_000)
    }
}

/// The families above Opus, best first. A row is offered for the first of
/// these the account has; Claude Code's built-in picker already carries
/// everything below.
const TOP_TIERS: &[&str] = &["mythos", "fable"];

/// Parse a Models API response body (`{"data": [...]}`).
pub fn parse_models(body: &[u8]) -> Result<Vec<ModelListing>> {
    #[derive(Deserialize)]
    struct Page {
        data: Vec<ModelListing>,
    }
    let page: Page = serde_json::from_slice(body).context("Models API response")?;
    Ok(page.data)
}

/// The most capable model the account can run above Opus: the best family
/// present, and within it the newest. `None` when the listing has nothing
/// above Opus, which is the honest answer for most accounts.
pub fn top_tier(models: &[ModelListing]) -> Option<&ModelListing> {
    TOP_TIERS.iter().find_map(|family| {
        models
            .iter()
            .filter(|model| model.id.to_ascii_lowercase().contains(family))
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            })
    })
}

/// The cache's on-disk shape. `version` is the alpha rule's whole
/// compatibility story: a different number is a different file.
#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    models: Vec<ModelListing>,
}

const CACHE_VERSION: u32 = 1;

/// `$XDG_STATE_HOME/taste-ide/models.json`, beside the credential file.
pub fn cache_path() -> Option<PathBuf> {
    Some(crate::credentials::credential_path()?.with_file_name("models.json"))
}

/// The cached listing, or nothing — a missing, stale-versioned or
/// unreadable cache is the same absence.
pub fn load_cached(path: &Path) -> Option<Vec<ModelListing>> {
    let text = std::fs::read(path).ok()?;
    let cache: Cache = serde_json::from_slice(&text).ok()?;
    (cache.version == CACHE_VERSION).then_some(cache.models)
}

pub fn store_cached(path: &Path, models: &[ModelListing]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let cache = Cache {
        version: CACHE_VERSION,
        models: models.to_vec(),
    };
    std::fs::write(path, serde_json::to_vec_pretty(&cache)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, created: &str, window: u64) -> ModelListing {
        ModelListing {
            id: id.into(),
            display_name: id.into(),
            created_at: created.into(),
            max_input_tokens: Some(window),
        }
    }

    #[test]
    fn the_listing_parses_as_the_api_shapes_it() {
        let body = br#"{"data":[{"type":"model","id":"claude-opus-5","display_name":"Claude Opus 5","created_at":"2026-04-01T00:00:00Z","max_input_tokens":1000000,"capabilities":{"x":true}},{"type":"model","id":"claude-haiku-4-5","display_name":"Claude Haiku 4.5","created_at":"2025-10-01T00:00:00Z"}],"has_more":false}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models.len(), 2);
        assert!(models[0].has_1m_context());
        assert!(!models[1].has_1m_context());
    }

    /// The newest Fable wins over an older one, a Mythos wins over any
    /// Fable, and an account with nothing above Opus gets nothing — not
    /// Opus dressed up as a top tier.
    #[test]
    fn the_top_tier_is_the_best_family_then_the_newest() {
        let opus_only = [model("claude-opus-5", "2026-04-01", 1_000_000)];
        assert_eq!(top_tier(&opus_only), None);
        let two_fables = [
            model("claude-fable-5", "2026-06-01", 1_000_000),
            model("claude-opus-5", "2026-04-01", 1_000_000),
            model("claude-fable-5-1", "2026-08-25", 1_000_000),
        ];
        assert_eq!(top_tier(&two_fables).unwrap().id, "claude-fable-5-1");
        let with_mythos = [
            model("claude-fable-5-1", "2026-08-25", 1_000_000),
            model("claude-mythos-5-1", "2026-08-25", 1_000_000),
        ];
        assert_eq!(top_tier(&with_mythos).unwrap().id, "claude-mythos-5-1");
    }

    #[test]
    fn the_cache_round_trips_and_a_foreign_version_is_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/taste-ide/models.json");
        assert_eq!(load_cached(&path), None);
        let models = vec![model("claude-fable-5-1", "2026-08-25", 1_000_000)];
        store_cached(&path, &models).unwrap();
        assert_eq!(load_cached(&path), Some(models));
        std::fs::write(&path, br#"{"version":99,"models":[]}"#).unwrap();
        assert_eq!(load_cached(&path), None);
    }
}
