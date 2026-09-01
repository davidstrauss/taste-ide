//! The fleet as data: one row per environment, assembled from the four
//! places an environment's facts actually live.
//!
//! ENVIRONMENTS.md → "Supervision: fleet view": name, mode, container
//! state, bound chat, current branch, published-branch count, disk
//! footprint. Those come from the registry (state), the workspace state
//! file (the human name), the chat strip (the binding), git (branches and
//! unpublished work), podman and the filesystem (disk), and the auth proxy
//! (spend) — six sources, none of which knows about the others.
//!
//! **This module is the assembly, and it has no widgets in it.** The
//! console renders what comes out; gadget mode (5b) and the varlink read
//! model will render the same rows rather than each re-deriving them from
//! six sources of their own, which is how two surfaces end up disagreeing
//! about what an environment is called.
//!
//! Everything here is pure: the GTK side gathers [`EnvFacts`] off the main
//! thread (git walks, podman calls, directory walks) and hands them in.

use std::collections::BTreeMap;

use taste_core::environment::EnvironmentId;
use taste_core::state::WorkspaceState;
use taste_devcontainer::{DiskUsage, SupervisorState};

/// The chat bound to an environment, as a row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBinding {
    /// The tab's title — what the user sees in the chat strip.
    pub label: String,
    /// A turn is in flight.
    pub busy: bool,
}

/// What one environment has spent through the auth proxy.
///
/// Mirrored rather than re-exported: `taste_authproxy::Spend` is the
/// proxy's own accounting, and a row wants three numbers of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spend {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Spend {
    pub fn is_zero(&self) -> bool {
        *self == Spend::default()
    }
}

/// Git facts about one environment's own checkout, computed off-thread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvGit {
    pub branch: Option<String>,
    /// Branches in this environment's clone that the main checkout has
    /// never seen — what destroying it would cost.
    pub unpublished: usize,
    /// Modified-but-uncommitted files in its working tree.
    pub dirty: usize,
}

/// Everything the GTK side gathered about one environment.
#[derive(Debug, Clone)]
pub struct EnvFacts {
    pub env: EnvironmentId,
    pub state: SupervisorState,
    pub pending_rebuild: bool,
    pub chat: Option<ChatBinding>,
    /// `None` until the git pass has run for this environment.
    pub git: Option<EnvGit>,
    /// `None` until someone asked for the footprint: it is a directory
    /// walk, so it is never computed as a side effect of rendering.
    pub disk: Option<DiskUsage>,
    pub spend: Spend,
}

/// One environment, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetRow {
    pub env: EnvironmentId,
    pub primary: bool,
    /// What to call it: the human name if there is one, else the slug.
    pub name: String,
    /// Whether `name` came from the user. The rename entry starts empty
    /// when it did not, rather than pre-typed with a slug to delete.
    pub named: bool,
    pub state: SupervisorState,
    pub pending_rebuild: bool,
    pub chat: Option<ChatBinding>,
    pub git: Option<EnvGit>,
    /// `agents/<this env>/*` branches waiting in the user's checkout.
    pub published: usize,
    pub disk: Option<DiskUsage>,
    pub spend: Spend,
}

impl FleetRow {
    /// Container mode, the only real working mode. Everything else is safe
    /// mode — including a container that is still building.
    pub fn container_mode(&self) -> bool {
        matches!(self.state, SupervisorState::Running { .. })
    }

    /// The row's state, in words. Two facts in one line, because they are
    /// read together: which mode the environment is in, and what its
    /// container is doing.
    pub fn state_text(&self) -> String {
        let detail = match &self.state {
            SupervisorState::NoConfig => "no devcontainer configuration".to_string(),
            SupervisorState::ConfigDetected => "configured, not started".to_string(),
            SupervisorState::Building => "building…".to_string(),
            SupervisorState::Starting => "starting…".to_string(),
            SupervisorState::Running { .. } => {
                if self.pending_rebuild {
                    "running · needs rebuild".to_string()
                } else {
                    "running".to_string()
                }
            }
            SupervisorState::Failed { message } => format!("failed: {}", first_line(message)),
            SupervisorState::Stopped => "stopped".to_string(),
        };
        format!("{} · {detail}", self.mode_text())
    }

    pub fn mode_text(&self) -> &'static str {
        if self.container_mode() {
            "container mode"
        } else {
            "safe mode"
        }
    }

    /// Whether this environment can be destroyed. The primary cannot: it
    /// is the user's checkout, not a clone the IDE made.
    pub fn destroyable(&self) -> bool {
        !self.primary
    }

    /// Whether destroying it would cost work nobody else has a copy of.
    pub fn has_unpublished_work(&self) -> bool {
        self.git
            .as_ref()
            .is_some_and(|git| git.unpublished > 0 || git.dirty > 0)
    }

    /// The footprint column: a size, or an honest dash.
    pub fn disk_text(&self) -> String {
        match &self.disk {
            None => "—".to_string(),
            Some(disk) => {
                let size = format_bytes(disk.total_bytes());
                if disk.partial() {
                    format!("{size}+")
                } else {
                    size
                }
            }
        }
    }

    /// The spend column. Tokens, because that is what a subscription is
    /// spent in; requests live in the tooltip.
    pub fn spend_text(&self) -> String {
        if self.spend.is_zero() {
            return "—".to_string();
        }
        format!(
            "{} in / {} out",
            compact(self.spend.input_tokens),
            compact(self.spend.output_tokens)
        )
    }
}

/// Assemble the fleet: registry facts + the state file's names + the
/// user's published branches → rows, primary first.
///
/// `published` is the branch list of the MAIN checkout (`agents/*`),
/// because that is where publishing lands — an environment's own clone
/// knows nothing about what it has handed over.
pub fn assemble(
    facts: Vec<EnvFacts>,
    state: &WorkspaceState,
    published: &[String],
) -> Vec<FleetRow> {
    let counts = published_by_environment(published);
    let mut rows: Vec<FleetRow> = facts
        .into_iter()
        .map(|facts| {
            let named = state.environment_name(&facts.env);
            FleetRow {
                primary: facts.env.is_primary(),
                name: named
                    .map(str::to_string)
                    .unwrap_or_else(|| facts.env.to_string()),
                named: named.is_some(),
                published: counts.get(facts.env.as_str()).copied().unwrap_or(0),
                env: facts.env,
                state: facts.state,
                pending_rebuild: facts.pending_rebuild,
                chat: facts.chat,
                git: facts.git,
                disk: facts.disk,
                spend: facts.spend,
            }
        })
        .collect();
    // The primary leads — it is the user's own checkout, and the row they
    // return to. The rest sort by what they are CALLED, so renaming one
    // moves it where the user would look for it.
    rows.sort_by(|a, b| {
        b.primary
            .cmp(&a.primary)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.env.as_str().cmp(b.env.as_str()))
    });
    rows
}

/// Attribute published branches to the environments that published them.
///
/// The convention is `agents/<env>/<topic>` (`taste_git::AGENT_BRANCH_PREFIX`
/// plus the environment that published it). Anything that does not fit —
/// a branch a user made called `agents/wip`, or one with no topic — is
/// counted for nobody rather than guessed at.
pub fn published_by_environment(branches: &[String]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for branch in branches {
        let Some(rest) = branch.strip_prefix(taste_git::AGENT_BRANCH_PREFIX) else {
            continue;
        };
        let Some((env, topic)) = rest.split_once('/') else {
            continue;
        };
        if env.is_empty() || topic.is_empty() {
            continue;
        }
        *counts.entry(env.to_string()).or_default() += 1;
    }
    counts
}

/// Bytes as a person reads them. Binary units, one decimal, no more
/// precision than a footprint deserves.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Token counts, compactly: a fleet row has no room for eight digits.
fn compact(value: u64) -> String {
    match value {
        0..=9_999 => value.to_string(),
        10_000..=999_999 => format!("{:.0}k", value as f64 / 1000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn facts(slug: &str, state: SupervisorState) -> EnvFacts {
        EnvFacts {
            env: env(slug),
            state,
            pending_rebuild: false,
            chat: None,
            git: None,
            disk: None,
            spend: Spend::default(),
        }
    }

    fn running() -> SupervisorState {
        SupervisorState::Running {
            container_id: "abc123".into(),
        }
    }

    /// The whole row model in one pass: six sources in, one ordered list
    /// out, with the primary leading and every environment carrying only
    /// what belongs to it.
    #[test]
    fn a_row_is_assembled_from_the_places_its_facts_live() {
        let mut state = WorkspaceState::default();
        state.set_environment_name(&env("spry-2"), Some("the refactor"));

        let published = vec![
            "agents/calm-1/inbox-filter".to_string(),
            "agents/calm-1/second-topic".to_string(),
            "agents/spry-2/docs".to_string(),
            // Not an environment's publish: no topic, and a plain branch.
            "agents/loose".to_string(),
            "main".to_string(),
        ];

        let rows = assemble(
            vec![
                facts("spry-2", SupervisorState::Stopped),
                EnvFacts {
                    chat: Some(ChatBinding {
                        label: "Claude 2".into(),
                        busy: true,
                    }),
                    git: Some(EnvGit {
                        branch: Some("topic/inbox".into()),
                        unpublished: 1,
                        dirty: 3,
                    }),
                    spend: Spend {
                        requests: 12,
                        input_tokens: 41_000,
                        output_tokens: 3_500,
                    },
                    ..facts("calm-1", running())
                },
                facts("primary", running()),
            ],
            &state,
            &published,
        );

        // Primary first, then by the name the user reads.
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["primary", "calm-1", "the refactor"]
        );
        assert!(rows[0].primary && !rows[1].primary);
        assert!(!rows[0].destroyable(), "the checkout is never destroyed");
        assert!(rows[1].destroyable());

        let calm = &rows[1];
        assert!(!calm.named, "an unnamed environment falls back to its slug");
        assert_eq!(calm.published, 2, "its own published branches, no others");
        assert_eq!(calm.chat.as_ref().unwrap().label, "Claude 2");
        assert!(calm.chat.as_ref().unwrap().busy);
        assert_eq!(
            calm.git.as_ref().unwrap().branch.as_deref(),
            Some("topic/inbox")
        );
        assert!(calm.has_unpublished_work());
        assert!(calm.container_mode());
        assert_eq!(calm.state_text(), "container mode · running");
        assert_eq!(calm.spend_text(), "41k in / 3500 out");
        assert_eq!(calm.disk_text(), "—", "not walked yet, and not guessed");

        let refactor = &rows[2];
        assert!(refactor.named);
        assert_eq!(refactor.env, env("spry-2"), "renaming changes no identity");
        assert_eq!(refactor.published, 1);
        assert!(!refactor.container_mode());
        assert_eq!(refactor.state_text(), "safe mode · stopped");
        assert!(!refactor.has_unpublished_work(), "not computed is not zero");
    }

    #[test]
    fn published_branches_are_attributed_only_when_the_convention_fits() {
        let counts = published_by_environment(&[
            "agents/calm-1/a".into(),
            "agents/calm-1/b/c".into(), // topics may contain slashes
            "agents//empty".into(),
            "agents/calm-1/".into(),
            "agents/".into(),
            "feature/x".into(),
        ]);
        assert_eq!(counts.get("calm-1"), Some(&2));
        assert_eq!(counts.len(), 1);
    }

    /// A row must say what a state means, including the two that are easy
    /// to misread: building is not container mode, and a running container
    /// whose config moved says so.
    #[test]
    fn state_text_never_promises_container_mode_it_does_not_have() {
        let state = WorkspaceState::default();
        let row = |supervisor, pending| {
            let mut facts = facts("calm-1", supervisor);
            facts.pending_rebuild = pending;
            assemble(vec![facts], &state, &[]).remove(0)
        };
        assert_eq!(
            row(SupervisorState::Building, false).state_text(),
            "safe mode · building…"
        );
        assert_eq!(
            row(SupervisorState::NoConfig, false).state_text(),
            "safe mode · no devcontainer configuration"
        );
        assert_eq!(
            row(running(), true).state_text(),
            "container mode · running · needs rebuild"
        );
        assert_eq!(
            row(
                SupervisorState::Failed {
                    message: "podman build: no such image\nstack trace…".into()
                },
                false
            )
            .state_text(),
            "safe mode · failed: podman build: no such image"
        );
    }

    #[test]
    fn a_footprint_says_when_it_is_incomplete() {
        let state = WorkspaceState::default();
        let with = |disk| {
            let mut facts = facts("calm-1", running());
            facts.disk = Some(disk);
            assemble(vec![facts], &state, &[]).remove(0)
        };
        assert_eq!(
            with(DiskUsage {
                checkout_bytes: 1024 * 1024 * 3,
                volume_bytes: 1024 * 1024,
                volumes_measured: 1,
                volumes_unmeasured: 0,
            })
            .disk_text(),
            "4.0 MiB"
        );
        assert_eq!(
            with(DiskUsage {
                checkout_bytes: 512,
                volumes_unmeasured: 2,
                ..Default::default()
            })
            .disk_text(),
            "512 B+",
            "a footprint missing volumes must not read as complete"
        );
    }
}
