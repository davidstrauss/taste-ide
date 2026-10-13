//! The model ids each agent has advertised in this workspace: the values a
//! session's model option takes, as the last session of that agent to say
//! so said them.
//!
//! A model is a value in one session's advertised list, so `issue_start`
//! could not check one until the environment it made had a session, and a
//! coordinator asked for ids it had no way to see: `claude-opus-5-5[1m]`,
//! a model's full name, where Claude Code advertises `opus[1m]` (David,
//! 2026-10-13: "Can we help the coordinating agent ask for valid models?").
//! But the coordinator's own chat, and every other chat of the same agent,
//! is a session that has already said. So the IDE keeps what each agent
//! last advertised, here, for the tool list to show and for a start to
//! check before it gives an environment its task.
//!
//! Per workspace, because the list is the credential's: a project has its
//! own, and with it its own plan.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct AdvertisedModels {
    by_agent: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
}

impl AdvertisedModels {
    /// What a session of `agent` advertises, now.
    pub fn record(&self, agent: &str, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        self.by_agent.lock().unwrap().insert(agent.to_string(), ids);
    }

    /// What `agent` last advertised here, if a session of it has said.
    pub fn of(&self, agent: &str) -> Option<Vec<String>> {
        self.by_agent.lock().unwrap().get(agent).cloned()
    }

    /// Every agent's, by agent id.
    pub fn all(&self) -> BTreeMap<String, Vec<String>> {
        self.by_agent.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_list_an_agent_advertised_is_kept_and_an_empty_one_is_not() {
        let models = AdvertisedModels::default();
        assert_eq!(models.of("claude-code"), None);
        models.record("claude-code", vec!["opus[1m]".into(), "sonnet".into()]);
        models.record("claude-code", vec![]);
        assert_eq!(
            models.of("claude-code"),
            Some(vec!["opus[1m]".to_string(), "sonnet".to_string()])
        );
        let clone = models.clone();
        clone.record("gemini", vec!["gemini-2.5-pro".into()]);
        assert_eq!(models.all().len(), 2, "one handle, shared by its clones");
    }
}
