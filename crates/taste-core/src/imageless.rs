//! The models this project has found cannot read images.
//!
//! A model served by the project's own server may take text alone, and
//! says so only when a request carries an image: the whole turn fails with
//! the server's 400 ("This model does not accept image inputs"), and every
//! turn after it fails the same way, because the image is in the session's
//! history now (2026-10-24). So the chat that meets that answer records the
//! model here, for the project — every environment's chat on that model
//! reads it — and starts its agent again on a fresh session whose
//! orientation says so, and the IDE's own tools hold their images back
//! from an agent on such a model, since one screenshot would poison the new
//! session as the old one was.
//!
//! Keyed by the model as the chat names it (`agent/model`), so a project
//! that changes its model asks the new one afresh. Persisted with the rest
//! of the workspace's state ([`crate::state::WorkspaceState`]), filled from
//! the state the window reads at startup, and written off the main thread,
//! as [`crate::standing`] is.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentId;

/// One model that refused images, as the state file carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagelessEntry {
    /// The chat's name for the model: its agent and the model it runs
    /// (`claude-code-custom/abliterated-model-large-v2`).
    pub model: String,
    /// What the server said, for the settings and the transcript.
    pub said: String,
    /// RFC 3339.
    #[serde(default)]
    pub since: Option<String>,
}

/// Whether an agent's words are a server refusing an image in a request.
/// The phrasings of the Anthropic-shaped servers a project points a custom
/// model at, which say it in a sentence rather than a code.
pub fn refuses_images(text: &str) -> bool {
    const SAID: [&str; 6] = [
        "does not accept image",
        "does not support image",
        "doesn't support image",
        "image input is not supported",
        "image inputs are not supported",
        "does not support vision",
    ];
    let text = text.to_ascii_lowercase();
    SAID.iter().any(|said| text.contains(said))
}

/// The line the orientation carries for an agent on such a model, and what
/// stands in for an image a tool would have returned to it.
pub const ORIENTATION: &str = "Images: the model you run on cannot read them, and a request \
     that carries one fails the whole turn. Do not call ide_screenshot or issue_attachment, do \
     not read images or PDF pages with ide_read_file, and attach no images. Check UI work with \
     ide_widget_geometry and ide_app_log instead.";
pub const WITHHELD: &str = "[image withheld: the model you run on cannot read images. Use \
     ide_widget_geometry or ide_app_log to check UI work.]";

/// The project's image-blind models, and the environments whose agent runs
/// on one now: one handle every chat and the MCP server share.
#[derive(Clone)]
pub struct ImagelessModels {
    inner: Arc<Mutex<Book>>,
}

struct Book {
    root: PathBuf,
    /// State-file directory override, for tests.
    base: Option<PathBuf>,
    /// Filled from the state the window read, or from the file on first
    /// use when nothing filled it.
    models: Option<BTreeMap<String, ImagelessEntry>>,
    /// In memory only: what each chat said about the session it runs.
    blind: HashSet<EnvironmentId>,
}

impl ImagelessModels {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Book {
                root: root.into(),
                base: None,
                models: None,
                blind: HashSet::new(),
            })),
        }
    }

    #[doc(hidden)]
    pub fn with_base_for_tests(base: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        let models = Self::new(root);
        if let Ok(mut inner) = models.inner.lock() {
            inner.base = Some(base.into());
        }
        models
    }

    /// Fill from state the caller has already read, so the first question
    /// asked on the GTK thread is not a file read. Nothing once filled.
    pub fn hydrate_from(&self, state: &crate::state::WorkspaceState) {
        let Ok(mut book) = self.inner.lock() else {
            return;
        };
        if book.models.is_none() {
            book.models = Some(Book::entries(state));
        }
    }

    /// Whether `model` has refused images in this project.
    pub fn refuses(&self, model: &str) -> bool {
        let Ok(mut book) = self.inner.lock() else {
            return false;
        };
        book.hydrate();
        book.models
            .as_ref()
            .is_some_and(|models| models.contains_key(model))
    }

    /// Record that `model` refused images, in `said`'s words. Returns
    /// whether it is new. In memory only: follow it with
    /// [`ImagelessModels::persist`] off the main thread.
    pub fn remember(&self, model: &str, said: &str) -> bool {
        let Ok(mut book) = self.inner.lock() else {
            return false;
        };
        book.hydrate();
        let models = book.models.get_or_insert_with(BTreeMap::new);
        if models.contains_key(model) {
            return false;
        }
        models.insert(
            model.to_string(),
            ImagelessEntry {
                model: model.to_string(),
                said: said.trim().chars().take(300).collect(),
                since: Some(crate::state::now_rfc3339()),
            },
        );
        true
    }

    /// Say whether `env`'s agent runs on an image-blind model now.
    pub fn set_blind(&self, env: &EnvironmentId, blind: bool) {
        if let Ok(mut book) = self.inner.lock() {
            if blind {
                book.blind.insert(env.clone());
            } else {
                book.blind.remove(env);
            }
        }
    }

    /// Whether `env`'s agent runs on an image-blind model, as its chat
    /// last said.
    pub fn blind(&self, env: &EnvironmentId) -> bool {
        self.inner.lock().is_ok_and(|book| book.blind.contains(env))
    }

    /// Write the models to the workspace's state file. **Blocking IO** —
    /// call it from `spawn_blocking`, never from a GTK handler.
    pub fn persist(&self) -> Result<()> {
        let book = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("the image-blind model book is poisoned"))?;
        let entries: Vec<ImagelessEntry> = book
            .models
            .as_ref()
            .map(|models| models.values().cloned().collect())
            .unwrap_or_default();
        let change = |state: &mut crate::state::WorkspaceState| state.imageless_models = entries;
        match &book.base {
            Some(base) => crate::state::update_in(base, &book.root, change),
            None => crate::state::update(&book.root, change),
        }
        .map(drop)
    }
}

impl Book {
    fn hydrate(&mut self) {
        if self.models.is_some() {
            return;
        }
        let state = match &self.base {
            Some(base) => crate::state::load_from(base, &self.root),
            None => crate::state::load(&self.root),
        };
        self.models = Some(Self::entries(&state));
    }

    fn entries(state: &crate::state::WorkspaceState) -> BTreeMap<String, ImagelessEntry> {
        state
            .imageless_models
            .iter()
            .map(|entry| (entry.model.clone(), entry.clone()))
            .collect()
    }
}

impl std::fmt::Debug for ImagelessModels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImagelessModels")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_refusals_are_recognised_and_nothing_else_is() {
        assert!(refuses_images(
            "API Error: 400 This model does not accept image inputs. Send this request \
             without images, or use a model that supports image input."
        ));
        assert!(refuses_images(
            "Error: image input is not supported by this model"
        ));
        assert!(!refuses_images("API Error: 529 Overloaded"));
        assert!(!refuses_images("I looked at the image you sent."));
    }

    /// A refusal is the project's: kept across a reopen, and asked of the
    /// model it was said about, not of the next one.
    #[test]
    fn a_refusal_is_kept_for_the_project_and_its_model() {
        let base = tempfile::tempdir().unwrap();
        let root = std::path::Path::new("/tmp/taste-imageless-project");
        let models = ImagelessModels::with_base_for_tests(base.path(), root);
        assert!(!models.refuses("claude-code-custom/small"));
        assert!(models.remember("claude-code-custom/small", "API Error: 400 …"));
        assert!(!models.remember("claude-code-custom/small", "again"));
        models.persist().unwrap();

        let reopened = ImagelessModels::with_base_for_tests(base.path(), root);
        assert!(reopened.refuses("claude-code-custom/small"));
        assert!(!reopened.refuses("claude-code-custom/large"));

        let env = EnvironmentId::primary();
        assert!(!reopened.blind(&env));
        reopened.set_blind(&env, true);
        assert!(reopened.blind(&env));
        reopened.set_blind(&env, false);
        assert!(!reopened.blind(&env));
    }
}
