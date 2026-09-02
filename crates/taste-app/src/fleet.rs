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
//! console renders what comes out; so does gadget mode, and so does the
//! varlink service — three surfaces, one derivation, rather than each
//! re-deriving from six sources of its own, which is how two surfaces end
//! up disagreeing about what an environment is called. [`snapshot`] is the
//! one place a row becomes something outside this process can read.
//!
//! Everything here is pure: the GTK side gathers [`EnvFacts`] off the main
//! thread (git walks, podman calls, directory walks) and hands them in.

use std::collections::BTreeMap;

use taste_core::environment::EnvironmentId;
use taste_core::state::WorkspaceState;
use taste_core::ConfigAuthority;
use taste_core::ReviewState;
use taste_devcontainer::{DiskUsage, SupervisorState};

/// The chat bound to an environment, as a row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBinding {
    /// The agent's name — what the user sees at the head of the chat.
    pub label: String,
    /// A turn is in flight.
    pub busy: bool,
    /// The chat is stopped on something only the user can answer — a
    /// permission request, a sign-in ([`crate::chat::ChatPane::awaits_user`]).
    /// Not the opposite of `busy`: a chat waiting on a person is still
    /// mid-turn, and is the one row in a fleet that will not move again on
    /// its own. The one fact about a chat that is *urgent* rather than
    /// informative, and the reason an environment nobody is looking at
    /// still gets a marker on its row.
    pub awaits_user: bool,
    /// This chat is the workspace's orchestrator: its environment's MCP
    /// socket serves the orchestration tools, and no other does.
    pub orchestrator: bool,
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

    /// Tokens either way. What "spent" means when one number has to
    /// stand for a row's draw on the pool.
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The subscription pool, and who has been drawing on it.
///
/// Assembled once by the console beside the fleet rows, from the same
/// facts, and handed to everything that renders it — the panel's header
/// gauge and every chat's utilization tab. Two halves that answer
/// different questions and come from different places: the account's own
/// limit state is observed on responses, while the breakdown is the
/// IDE's own accounting and covers only what went through this IDE.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PoolFacts {
    pub quota: taste_core::quota::QuotaSnapshot,
    /// Environment display name and tokens through the proxy, biggest
    /// first, zero-spend rows dropped.
    pub spenders: Vec<(String, u64)>,
}

impl PoolFacts {
    /// Tokens the whole fleet has drawn through this IDE.
    pub fn total(&self) -> u64 {
        self.spenders.iter().map(|(_, tokens)| tokens).sum()
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
    /// Whose config the running container was built from. The second half
    /// of the mode: `Running` alone no longer means container mode, because
    /// safe mode is a container too now.
    pub authority: ConfigAuthority,
    pub pending_rebuild: bool,
    pub chat: Option<ChatBinding>,
    /// `None` until the git pass has run for this environment.
    pub git: Option<EnvGit>,
    /// `None` until someone asked for the footprint: it is a directory
    /// walk, so it is never computed as a side effect of rendering.
    pub disk: Option<DiskUsage>,
    pub spend: Spend,
    /// Live shells attached to this environment — the user's terminals,
    /// the agent's, `ide_exec` jobs, and the build's own lifecycle stream
    /// ([`taste_core::ShellRoster::list`]). An in-memory count, cheap
    /// enough for a render, and the monitor's answer to "is anything
    /// happening in there".
    pub shells: usize,
    /// Where this environment stands in the review arc
    /// ([`taste_core::ReviewState`]) — working, waiting on the user, or
    /// settled and safe to destroy.
    pub review: ReviewState,
    /// The issues this environment has claimed: what it is working ON,
    /// as opposed to what it is doing. Read from the issues ref in the
    /// same off-thread pass as the git facts, so it is `Vec::new()` until
    /// that has run.
    pub working_on: Vec<taste_git::Claim>,
}

/// An environment's status at traffic-light resolution.
///
/// Three lights and an honest fourth, and the three are chosen by what the
/// user would DO rather than by what the supervisor is doing — which is why
/// this is a mapping over [`SupervisorState`] and not a parallel copy of
/// it. Seven supervisor states, one chat flag and a rebuild flag collapse
/// to: work is happening here, this wants you, or nothing can run here.
///
/// It lives beside [`assemble`] because a second surface deriving its own
/// colours from the same seven states is how two surfaces come to disagree
/// about whether an environment is healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    /// Container mode: the environment is up and work runs in it. Busy or
    /// idle alike — idle is not a fault, and a fleet where idleness reads
    /// as a warning is a fleet nobody watches.
    Green,
    /// Wants attention. Either it is on its way somewhere (building,
    /// starting), or it is stopped on a person: an unanswered permission
    /// request, a sign-in, a config that has drifted from the container
    /// running it.
    Amber,
    /// Nothing can run here — failed, stopped, or never configured. Safe
    /// mode is this: no exec target at all.
    Red,
    /// The fleet has not said yet. Not a status; the absence of one.
    /// Produced by callers with no row in hand, never by [`FleetRow::light`].
    Unknown,
}

impl Light {
    /// The CSS class that colours it (see the `.env-dot` rules in main.rs).
    pub fn css(self) -> &'static str {
        match self {
            Light::Green => "green",
            Light::Amber => "amber",
            Light::Red => "red",
            Light::Unknown => "unknown",
        }
    }
}

/// How an environment's place in the review arc marks it in a list.
///
/// Deliberately its own vocabulary rather than a fourth [`Light`]. The
/// light answers "can work happen here", and a flagged environment's honest
/// answer to that is *no* — its container was stopped because it is done.
/// That is not the same fact as "this wants your judgment", and folding the
/// second into the first would either lie about the container or spend the
/// one hue this UI reserves for "you are the blocker" on an environment
/// that is not blocking anything.
///
/// So it is a second mark on the same row, and it is a mark rather than a
/// re-ordering: a row that jumped to the top when an agent finished would
/// move the list under the pointer of the person reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMark {
    /// Working. Nothing to say — the ordinary case wears no mark.
    None,
    /// Waiting on the user. The unmissable one.
    Flagged,
    /// Merged or rejected: the user has ruled, and what is left is
    /// history. Quieter than flagged, and still worth saying, because it
    /// is the difference between a fleet that drains and one that
    /// accumulates.
    Settled,
}

impl ReviewMark {
    pub fn of(state: ReviewState) -> Self {
        match state {
            ReviewState::Working => ReviewMark::None,
            ReviewState::FlaggedForReview => ReviewMark::Flagged,
            ReviewState::Merged | ReviewState::Rejected => ReviewMark::Settled,
        }
    }

    /// The class on the row (see the `.review-*` rules in main.rs), or
    /// `None` for the ordinary case.
    pub fn css(self) -> Option<&'static str> {
        match self {
            ReviewMark::None => None,
            ReviewMark::Flagged => Some("review-flagged"),
            ReviewMark::Settled => Some("review-settled"),
        }
    }

    /// The glyph beside the name. A glyph rather than a fourth dot: the
    /// row's circles are all 8px and all mean "state", and a fourth one in
    /// a fourth colour would read as a fourth traffic light.
    pub fn icon(self) -> Option<&'static str> {
        match self {
            ReviewMark::None => None,
            // An eye: this is asking to be looked at.
            ReviewMark::Flagged => Some("view-reveal-symbolic"),
            ReviewMark::Settled => Some("emblem-ok-symbolic"),
        }
    }
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
    pub authority: ConfigAuthority,
    pub pending_rebuild: bool,
    pub chat: Option<ChatBinding>,
    pub git: Option<EnvGit>,
    /// Whether this environment's branch of record (`agents/<env>`) exists
    /// in the user's checkout — 0 or 1, because an environment has exactly
    /// one branch. Kept as a count rather than a bool so the fleet wire's
    /// shape does not move under the gadget and the varlink clients.
    pub published: usize,
    pub disk: Option<DiskUsage>,
    pub spend: Spend,
    /// Live shells in this environment. See [`EnvFacts::shells`].
    pub shells: usize,
    /// See [`EnvFacts::review`].
    pub review: ReviewState,
    /// See [`EnvFacts::working_on`].
    pub working_on: Vec<taste_git::Claim>,
}

impl FleetRow {
    /// Container mode, the only real working mode. Everything else is safe
    /// mode — including a container that is still building, and including a
    /// running *baseline* container, which is a real place to run things but
    /// not the project's environment.
    pub fn container_mode(&self) -> bool {
        self.container_running() && self.authority == ConfigAuthority::Project
    }

    /// Whether a container is up at all, of either authority.
    ///
    /// Split from [`Self::container_mode`] because the two questions have
    /// different answers under the baseline and different consumers: the
    /// *mode* decides whether the workspace is writable, while *running*
    /// decides whether Stop is a thing the user can press. Conflating them
    /// offered Start for a container that was already up.
    pub fn container_running(&self) -> bool {
        matches!(self.state, SupervisorState::Running { .. })
    }

    /// Whether this environment is running the IDE's baseline rather than
    /// the project's own config — safe mode, with somewhere to run.
    pub fn baseline(&self) -> bool {
        self.container_running() && self.authority == ConfigAuthority::Baseline
    }

    /// Whether a chat here is stopped on the user. Folded out of the
    /// binding so the two surfaces that ask (the light, the tooltip that
    /// explains it) ask once.
    pub fn awaits_user(&self) -> bool {
        self.chat.as_ref().is_some_and(|chat| chat.awaits_user)
    }

    /// This row at traffic-light resolution ([`Light`]).
    ///
    /// Precedence is severity, and the order matters: a stopped container
    /// whose agent happens to be mid-permission is red, because the
    /// permission is not the thing standing in the way. Amber is reserved
    /// for an environment that *could* work and is waiting — on a build, or
    /// on the user.
    ///
    /// A stopped container is red even though stopping is often routine
    /// (the idle sweep does it). The panel's question is "can work happen
    /// in there", and the answer is no; softening that to a neutral colour
    /// would make the one honest signal — nothing runs here — the quietest
    /// thing on the row.
    pub fn light(&self) -> Light {
        match self.state {
            // Nothing to run in: broken, never configured, or down.
            SupervisorState::Failed { .. }
            | SupervisorState::NoConfig
            | SupervisorState::ConfigDetected
            | SupervisorState::Stopped => Light::Red,
            // On its way.
            SupervisorState::Building | SupervisorState::Starting => Light::Amber,
            SupervisorState::Running { .. } => {
                // Up, but wanting something from the user: an unanswered
                // question, a config the container no longer matches, or a
                // baseline standing in because the project's own config is
                // missing or broken.
                //
                // The baseline is amber even though its container is
                // green-healthy inside, and that is the honest reading:
                // amber means "this could work and is waiting on you",
                // which is exactly a repo whose environment has not been
                // written yet. Green would claim the project's environment
                // is up when what is up is the IDE's stand-in.
                if self.pending_rebuild || self.awaits_user() || self.baseline() {
                    Light::Amber
                } else {
                    Light::Green
                }
            }
        }
    }

    /// The row's state, in words: what its container is doing, prefixed by
    /// the mode when the mode is worth saying ([`Self::mode_text`]).
    pub fn state_text(&self) -> String {
        let detail = match &self.state {
            SupervisorState::NoConfig => "not configured".to_string(),
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
        match self.mode_text() {
            Some(mode) => format!("{mode} · {detail}"),
            None => detail,
        }
    }

    /// What to call the mode — and `None` for the ordinary case.
    ///
    /// "Container mode" was retired with the baseline: every environment
    /// that is up is a container now, so the word distinguished nothing and
    /// spent the first half of every status line saying so. What survives
    /// is the ladder's other two rungs, and both are *departures* from the
    /// normal case, which is what a label is for:
    ///
    /// - the project's own configuration in force → nothing to say;
    /// - the IDE's baseline standing in → "safe mode", the name the docs
    ///   and the write policy already use;
    /// - nothing running at all → "no environment", which is the honest
    ///   reading of [`taste_core::ExecContext::has_exec_target`] being
    ///   false: not a third mode, the absence of both.
    ///
    /// The baseline's old "safe mode (baseline)" parenthetical is gone with
    /// it. It existed to separate "safe mode with a container up" from
    /// "safe mode with nothing running", and the third rung now has its own
    /// name, so the two can no longer be confused.
    pub fn mode_text(&self) -> Option<&'static str> {
        if self.container_mode() {
            None
        } else if self.baseline() {
            Some("safe mode")
        } else {
            Some("no environment")
        }
    }

    /// The same three rungs at tooltip length: what the mode *means* for
    /// what can be run and written here. Beside [`Self::mode_text`] so the
    /// short form and the long form can never drift apart.
    pub fn mode_explainer(&self) -> &'static str {
        if self.container_mode() {
            "This environment runs the project's own devcontainer configuration."
        } else if self.baseline() {
            "The IDE's baseline environment is standing in, so commands run — but \
             writes are confined to devcontainer setup until the project's own \
             configuration builds."
        } else {
            "Nothing is running here: no shell, and writes confined to devcontainer \
             setup. Repairs only."
        }
    }

    /// The mode, as a token a machine matches on rather than reads.
    ///
    /// Deliberately still two values. A baseline environment *is* in safe
    /// mode — its checkout is read-only and its write scope is the config —
    /// so emitting a third token across the varlink boundary would make
    /// every client that matches `"safe"` quietly miss it. The distinction
    /// is human-facing, and it lives in [`Self::mode_text`].
    pub fn mode_slug(&self) -> &'static str {
        if self.container_mode() {
            "container"
        } else {
            "safe"
        }
    }

    /// The lifecycle state, as a stable token. This one crosses the
    /// varlink boundary, so it is spelled here and only here: a client
    /// that switches on `"building"` must keep working when
    /// [`SupervisorState`] grows a variant, which is why the mapping is
    /// explicit and not a `Debug` string.
    pub fn state_slug(&self) -> &'static str {
        match self.state {
            SupervisorState::NoConfig => "no-config",
            SupervisorState::ConfigDetected => "config-detected",
            SupervisorState::Building => "building",
            SupervisorState::Starting => "starting",
            SupervisorState::Running { .. } => "running",
            SupervisorState::Failed { .. } => "failed",
            SupervisorState::Stopped => "stopped",
        }
    }

    /// Whether this environment can be destroyed. The primary cannot: it
    /// is the user's checkout, not a clone the IDE made.
    pub fn destroyable(&self) -> bool {
        !self.primary
    }

    /// Whether destroying it would cost work nobody else has a copy of.
    ///
    /// Never true of the primary: its uncommitted files are the user's own
    /// working tree, which is not "unpublished work at risk" — it is what
    /// they are doing right now, and nothing here can destroy it.
    ///
    /// Never true of a SETTLED environment either. Once the user has merged
    /// or rejected it they have looked at its branch and ruled on it, so
    /// the leftovers in its clone are not work at risk — they are what the
    /// user already decided against, and warning about them again would
    /// make the one warning that matters look like noise.
    pub fn has_unpublished_work(&self) -> bool {
        !self.primary
            && !self.review.settled()
            && self
                .git
                .as_ref()
                .is_some_and(|git| git.unpublished > 0 || git.dirty > 0)
    }

    /// Where this row stands in the review arc, as a list marks it.
    pub fn review_mark(&self) -> ReviewMark {
        ReviewMark::of(self.review)
    }

    /// The one-line answer to "what is this environment working on", or
    /// `None` when nothing has been claimed for it.
    ///
    /// Rendered by the console's environment header, beside what the
    /// environment is *doing* — which is a different question, and the
    /// reason both are on screen at once.
    pub fn working_on_text(&self) -> Option<String> {
        let first = self.working_on.first()?;
        Some(match self.working_on.len() {
            1 => format!("{} — {}", first.id, first.title),
            n => format!("{} — {} (+{} more)", first.id, first.title, n - 1),
        })
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
                authority: facts.authority,
                pending_rebuild: facts.pending_rebuild,
                chat: facts.chat,
                git: facts.git,
                disk: facts.disk,
                spend: facts.spend,
                shells: facts.shells,
                review: facts.review,
                working_on: facts.working_on,
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

/// The rows, as everything outside the console reads them.
///
/// One conversion, two consumers: the varlink service publishes this, and
/// gadget mode renders it. That is deliberate — the compact card is a
/// *render* of the fleet, not a second model of it, and the surest way to
/// keep it one is to give it the same struct a stranger on a socket gets.
///
/// Nothing is computed here that is not already in a row. The aggregates
/// the card shows — how many environments are waiting on a judgment, the
/// fuel gauge — are sums the snapshot itself takes
/// ([`taste_fleetlink::Snapshot::flagged_for_review`],
/// [`taste_fleetlink::Snapshot::spend`]), so the number in the card and
/// the number on the wire cannot differ.
///
/// `open_issues` is the exception, and is passed in rather than derived:
/// the queue lives on one ref for the whole workspace, and an unclaimed
/// issue belongs to no environment, so no sum over these rows could find
/// it. It joins the projection anyway, because a fourth surface with its
/// own count of the same ref is exactly the drift this function exists to
/// prevent.
pub fn snapshot(
    rows: &[FleetRow],
    workspace: &str,
    open_issues: usize,
) -> taste_fleetlink::Snapshot {
    taste_fleetlink::Snapshot {
        workspace: workspace.to_string(),
        open_issues: open_issues as u64,
        rows: rows
            .iter()
            .map(|row| taste_fleetlink::Row {
                environment: row.env.to_string(),
                name: row.name.clone(),
                named: row.named,
                primary: row.primary,
                mode: row.mode_slug().to_string(),
                state: row.state_slug().to_string(),
                detail: row.state_text(),
                pending_rebuild: row.pending_rebuild,
                chat: row.chat.as_ref().map(|chat| taste_fleetlink::Chat {
                    label: chat.label.clone(),
                    busy: chat.busy,
                    orchestrator: chat.orchestrator,
                }),
                branch: row.git.as_ref().and_then(|git| git.branch.clone()),
                unpublished: row.git.as_ref().map(|git| git.unpublished).unwrap_or(0) as u64,
                dirty: row.git.as_ref().map(|git| git.dirty).unwrap_or(0) as u64,
                // Not computed is not zero, and a client cannot tell the
                // difference from the numbers alone.
                git_known: row.git.is_some(),
                published: row.published as u64,
                shells: row.shells as u64,
                disk_bytes: row.disk.as_ref().map(|disk| disk.total_bytes()),
                spend: taste_fleetlink::Spend {
                    requests: row.spend.requests,
                    input_tokens: row.spend.input_tokens,
                    output_tokens: row.spend.output_tokens,
                },
                // The arc, as the token `ReviewState` already spells. A
                // second spelling on the wire would be a second thing to
                // keep in agreement with the state file.
                review: row.review.as_str().to_string(),
                working_on: row
                    .working_on
                    .iter()
                    .map(|claim| taste_fleetlink::Claim {
                        id: claim.id.clone(),
                        title: claim.title.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Attribute published branches to the environments that published them.
///
/// One branch per environment (`agents/<env>`), so every count here is 0 or
/// 1 — that is the model, not a coincidence. Anything that does not fit,
/// including the dead `agents/<env>/<topic>` generation, is counted for
/// nobody rather than guessed at; `taste_git::GitWorkspace::
/// dead_generation_branches` is what reports those.
pub fn published_by_environment(branches: &[String]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for branch in branches {
        let Some(env) = taste_git::env_of_branch(branch) else {
            continue;
        };
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
            authority: ConfigAuthority::Project,
            pending_rebuild: false,
            chat: None,
            git: None,
            disk: None,
            spend: Spend::default(),
            shells: 0,
            review: taste_core::ReviewState::Working,
            working_on: Vec::new(),
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
            "agents/calm-1".to_string(),
            "agents/spry-2".to_string(),
            // The dead generation belongs to no environment.
            "agents/calm-1/old-topic".to_string(),
            "main".to_string(),
        ];

        let rows = assemble(
            vec![
                facts("spry-2", SupervisorState::Stopped),
                EnvFacts {
                    chat: Some(ChatBinding {
                        label: "Claude 2".into(),
                        busy: true,
                        awaits_user: false,
                        orchestrator: false,
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
        assert_eq!(
            calm.published, 1,
            "one branch of record, however many times it published"
        );
        assert_eq!(calm.chat.as_ref().unwrap().label, "Claude 2");
        assert!(calm.chat.as_ref().unwrap().busy);
        assert_eq!(
            calm.git.as_ref().unwrap().branch.as_deref(),
            Some("topic/inbox")
        );
        assert!(calm.has_unpublished_work());
        assert!(calm.container_mode());
        assert_eq!(
            calm.state_text(),
            "running",
            "the ordinary case wears no mode word"
        );
        assert_eq!(calm.spend_text(), "41k in / 3500 out");
        assert_eq!(calm.disk_text(), "—", "not walked yet, and not guessed");

        let refactor = &rows[2];
        assert!(refactor.named);
        assert_eq!(refactor.env, env("spry-2"), "renaming changes no identity");
        assert_eq!(refactor.published, 1);
        assert!(!refactor.container_mode());
        assert_eq!(refactor.state_text(), "no environment · stopped");
        assert!(!refactor.has_unpublished_work(), "not computed is not zero");
    }

    /// One branch per environment, so every count is 0 or 1 — and the
    /// dead `agents/<env>/<topic>` generation is attributed to nobody
    /// rather than folded back into the environment whose name it starts
    /// with.
    #[test]
    fn only_a_branch_of_record_is_attributed_to_an_environment() {
        let counts = published_by_environment(&[
            "agents/calm-1".into(),
            "agents/spry-2".into(),
            "agents/calm-1/a".into(),
            "agents/calm-1/b/c".into(),
            "agents/".into(),
            "feature/x".into(),
        ]);
        assert_eq!(counts.get("calm-1"), Some(&1));
        assert_eq!(counts.get("spry-2"), Some(&1));
        assert_eq!(counts.len(), 2);
    }

    /// The review arc, as the row reports it: a settled environment is
    /// destroyable with nothing to warn about, even holding work its clone
    /// never published — the user already ruled on it.
    #[test]
    fn a_settled_environment_has_nothing_left_to_warn_about() {
        let state = WorkspaceState::default();
        let with = |review| {
            let mut facts = facts("calm-1", running());
            facts.review = review;
            facts.git = Some(EnvGit {
                branch: Some("work".into()),
                unpublished: 2,
                dirty: 3,
            });
            assemble(vec![facts], &state, &[]).remove(0)
        };
        let working = with(taste_core::ReviewState::Working);
        assert!(working.has_unpublished_work());
        assert!(working.destroyable());
        assert!(with(taste_core::ReviewState::FlaggedForReview).has_unpublished_work());
        for settled in [
            taste_core::ReviewState::Merged,
            taste_core::ReviewState::Rejected,
        ] {
            let row = with(settled);
            assert!(row.destroyable());
            assert!(
                !row.has_unpublished_work(),
                "{settled:?} means the user has already looked"
            );
        }
    }

    /// The review arc as a list marks it: three states of mark over four
    /// states of arc, with the two settled ones deliberately sharing one.
    /// The user has ruled either way, and a list does not need to shout the
    /// difference — the console's detail says which way it went.
    #[test]
    fn the_review_arc_marks_a_row_without_moving_it() {
        let state = WorkspaceState::default();
        let mark = |review| {
            let mut facts = facts("calm-1", running());
            facts.review = review;
            assemble(vec![facts], &state, &[]).remove(0).review_mark()
        };
        assert_eq!(mark(ReviewState::Working), ReviewMark::None);
        assert_eq!(mark(ReviewState::FlaggedForReview), ReviewMark::Flagged);
        assert_eq!(mark(ReviewState::Merged), ReviewMark::Settled);
        assert_eq!(mark(ReviewState::Rejected), ReviewMark::Settled);

        // The ordinary case wears nothing at all: a mark on every row is
        // not a mark.
        assert_eq!(ReviewMark::None.css(), None);
        assert_eq!(ReviewMark::None.icon(), None);
        // ...and the two that do wear something wear different things, or
        // the mark says nothing.
        assert_ne!(ReviewMark::Flagged.css(), ReviewMark::Settled.css());
        assert_ne!(ReviewMark::Flagged.icon(), ReviewMark::Settled.icon());

        // The mark is NOT the light. A flagged environment's container was
        // stopped because it is done, so the light is honestly red — and
        // red is not what "this wants your judgment" looks like, which is
        // the whole reason there are two marks and not one.
        let mut facts = facts("calm-1", SupervisorState::Stopped);
        facts.review = ReviewState::FlaggedForReview;
        let row = assemble(vec![facts], &state, &[]).remove(0);
        assert_eq!(row.light(), Light::Red);
        assert_eq!(row.review_mark(), ReviewMark::Flagged);
    }

    /// What an environment is working ON, as one line.
    #[test]
    fn the_row_says_which_issue_an_environment_claimed() {
        let state = WorkspaceState::default();
        let claim = |id: &str, title: &str| taste_git::Claim {
            id: id.into(),
            title: title.into(),
        };
        let with = |held: Vec<taste_git::Claim>| {
            let mut facts = facts("calm-1", running());
            facts.working_on = held;
            assemble(vec![facts], &state, &[]).remove(0)
        };
        assert_eq!(with(Vec::new()).working_on_text(), None);
        assert_eq!(
            with(vec![claim("i-0003", "The parser drops commas")]).working_on_text(),
            Some("i-0003 — The parser drops commas".to_string())
        );
        assert_eq!(
            with(vec![
                claim("i-0003", "The parser drops commas"),
                claim("i-0009", "And the lexer"),
            ])
            .working_on_text(),
            Some("i-0003 — The parser drops commas (+1 more)".to_string())
        );
    }

    /// A row must say what a state means, including the two that are easy
    /// to misread: a container mid-build is not a place to run anything
    /// yet, and a running container whose config moved says so.
    #[test]
    fn state_text_never_promises_an_environment_it_does_not_have() {
        let state = WorkspaceState::default();
        let row = |supervisor, pending| {
            let mut facts = facts("calm-1", supervisor);
            facts.pending_rebuild = pending;
            assemble(vec![facts], &state, &[]).remove(0)
        };
        assert_eq!(
            row(SupervisorState::Building, false).state_text(),
            "no environment · building…"
        );
        assert_eq!(
            row(SupervisorState::NoConfig, false).state_text(),
            "no environment · not configured"
        );
        assert_eq!(
            row(running(), true).state_text(),
            "running · needs rebuild",
            "a drifted config is still the project's config in force"
        );
        assert_eq!(
            row(
                SupervisorState::Failed {
                    message: "podman build: no such image\nstack trace…".into()
                },
                false
            )
            .state_text(),
            "no environment · failed: podman build: no such image"
        );
    }

    /// The wire snapshot is a projection of the rows and nothing more.
    /// Gadget mode and the varlink service both read it, so anything it
    /// invents is something two surfaces can disagree about.
    #[test]
    fn the_snapshot_projects_the_rows_and_derives_its_totals_from_them() {
        let mut state = WorkspaceState::default();
        state.set_environment_name(&env("spry-2"), Some("the refactor"));
        let rows = assemble(
            vec![
                facts("primary", running()),
                EnvFacts {
                    chat: Some(ChatBinding {
                        label: "Claude 2".into(),
                        busy: true,
                        awaits_user: false,
                        orchestrator: false,
                    }),
                    git: Some(EnvGit {
                        branch: Some("topic/inbox".into()),
                        unpublished: 1,
                        dirty: 3,
                    }),
                    shells: 2,
                    spend: Spend {
                        requests: 12,
                        input_tokens: 41_000,
                        output_tokens: 3_500,
                    },
                    ..facts("calm-1", running())
                },
                EnvFacts {
                    spend: Spend {
                        requests: 1,
                        input_tokens: 1_000,
                        output_tokens: 500,
                    },
                    ..facts("spry-2", SupervisorState::Building)
                },
            ],
            &state,
            &["agents/calm-1".into(), "agents/spry-2".into()],
        );
        let snapshot = super::snapshot(&rows, "taste-ide", 5);

        assert_eq!(snapshot.workspace, "taste-ide");
        // The queue is the workspace's: it rides the projection, but no
        // sum over these rows could produce it — an unclaimed issue has no
        // environment to be counted under.
        assert_eq!(snapshot.open_issues, 5);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .map(|r| &r.environment)
                .collect::<Vec<_>>(),
            ["primary", "calm-1", "spry-2"],
            "the order the rows were assembled in survives the projection"
        );
        // The aggregates the gadget's header and gauge show are sums the
        // snapshot takes, not numbers this function made up.
        assert_eq!(
            snapshot.flagged_for_review(),
            0,
            "publishing is a checkpoint, not a request for judgment"
        );
        assert_eq!(snapshot.spend().input_tokens, 42_000);
        assert_eq!(snapshot.spend().requests, 13);
        assert_eq!(snapshot.running(), 2, "building is not running");
        assert_eq!(snapshot.busy(), 1);

        let calm = &snapshot.rows[1];
        assert_eq!(calm.mode, "container");
        assert_eq!(calm.state, "running");
        assert_eq!(calm.detail, "running");
        assert_eq!(calm.chat.as_ref().unwrap().label, "Claude 2");
        assert!(calm.git_known && calm.branch.as_deref() == Some("topic/inbox"));
        assert_eq!((calm.unpublished, calm.dirty, calm.shells), (1, 3, 2));
        assert_eq!(calm.disk_bytes, None, "not walked, and not guessed");

        let spry = &snapshot.rows[2];
        assert_eq!(spry.name, "the refactor");
        assert_eq!(spry.environment, "spry-2", "renaming changes no identity");
        assert!(spry.named);
        assert_eq!(
            (spry.mode.as_str(), spry.state.as_str()),
            ("safe", "building")
        );
        assert!(
            !spry.git_known && spry.unpublished == 0,
            "a client must be able to tell unknown from zero"
        );
    }

    /// Seven supervisor states plus two flags collapse to three lights,
    /// and every collapse is asserted — this is the mapping the
    /// environment panel colours a dot with, so a wrong arm here is a
    /// green light on an environment nothing can run in.
    #[test]
    fn the_traffic_light_says_whether_work_can_happen_here() {
        let state = WorkspaceState::default();
        let light =
            |supervisor| assemble(vec![facts("calm-1", supervisor)], &state, &[])[0].light();
        assert_eq!(light(running()), Light::Green, "up and working");
        assert_eq!(light(SupervisorState::Building), Light::Amber);
        assert_eq!(light(SupervisorState::Starting), Light::Amber);
        // Down is down, however routine the reason.
        assert_eq!(light(SupervisorState::Stopped), Light::Red);
        assert_eq!(light(SupervisorState::NoConfig), Light::Red);
        assert_eq!(light(SupervisorState::ConfigDetected), Light::Red);
        assert_eq!(
            light(SupervisorState::Failed {
                message: "boom".into()
            }),
            Light::Red
        );
    }

    /// A baseline container is up and healthy inside, and still wants the
    /// user: its project has no usable devcontainer. Amber, in the one
    /// mapping — a second surface deriving its own colour here is how two
    /// surfaces come to disagree about whether an environment is fine.
    #[test]
    fn a_baseline_environment_is_amber_and_says_so_in_words() {
        let state = WorkspaceState::default();
        let mut facts = facts("calm-1", running());
        facts.authority = ConfigAuthority::Baseline;
        let row = assemble(vec![facts], &state, &[]).remove(0);

        assert_eq!(
            row.light(),
            Light::Amber,
            "green would claim the project's environment is up"
        );
        assert!(row.baseline());
        assert!(row.container_running(), "there IS a container");
        assert!(
            !row.container_mode(),
            "but it is not the project's, so the workspace stays locked"
        );
        assert_eq!(row.mode_text(), Some("safe mode"));
        assert_eq!(
            row.mode_slug(),
            "safe",
            "a client matching \"safe\" must not miss a baseline environment"
        );
        assert_eq!(
            row.state_text(),
            "safe mode · running",
            "the baseline is the one rung that says \"safe mode\" and means a \n             container is up"
        );
    }

    /// The same row under the project's own config is the working mode —
    /// the control for the test above, so the amber is attributable to the
    /// authority and not to something else on the row.
    #[test]
    fn the_same_row_under_the_projects_config_is_green_and_unlabelled() {
        let state = WorkspaceState::default();
        let mut facts = facts("calm-1", running());
        facts.authority = ConfigAuthority::Project;
        let row = assemble(vec![facts], &state, &[]).remove(0);

        assert_eq!(row.light(), Light::Green);
        assert!(row.container_mode() && row.container_running());
        assert!(!row.baseline());
        assert_eq!(row.mode_text(), None, "the normal case names no mode");
    }

    /// The two ways a running environment still wants you — and the one
    /// case where wanting you is not the problem.
    #[test]
    fn a_running_environment_turns_amber_when_it_is_waiting_on_a_person() {
        let state = WorkspaceState::default();
        let row = |supervisor, pending, awaits_user| {
            let mut facts = facts("calm-1", supervisor);
            facts.pending_rebuild = pending;
            facts.chat = Some(ChatBinding {
                label: "Claude 2".into(),
                busy: true,
                awaits_user,
                orchestrator: false,
            });
            assemble(vec![facts], &state, &[]).remove(0)
        };
        assert_eq!(
            row(running(), false, false).light(),
            Light::Green,
            "a busy chat is work happening, not a warning"
        );
        assert_eq!(
            row(running(), false, true).light(),
            Light::Amber,
            "an unanswered question is the user's turn"
        );
        assert!(row(running(), false, true).awaits_user());
        assert_eq!(
            row(running(), true, false).light(),
            Light::Amber,
            "a container that no longer matches its config wants applying"
        );
        assert_eq!(
            row(SupervisorState::Stopped, false, true).light(),
            Light::Red,
            "severity wins: the permission is not what stands in the way"
        );
    }

    /// `Unknown` is for callers with no row. A row always has a state, so
    /// it always has one of the three.
    #[test]
    fn a_row_never_reports_an_unknown_light() {
        let state = WorkspaceState::default();
        for supervisor in [
            SupervisorState::NoConfig,
            SupervisorState::ConfigDetected,
            SupervisorState::Building,
            SupervisorState::Starting,
            running(),
            SupervisorState::Failed {
                message: "boom".into(),
            },
            SupervisorState::Stopped,
        ] {
            let row = assemble(vec![facts("calm-1", supervisor)], &state, &[]).remove(0);
            assert_ne!(row.light(), Light::Unknown);
        }
        // And the four lights keep four distinct classes, because the CSS
        // is what actually colours the dot.
        let mut classes = [
            Light::Green.css(),
            Light::Amber.css(),
            Light::Red.css(),
            Light::Unknown.css(),
        ];
        classes.sort_unstable();
        let unique = {
            let mut all = classes.to_vec();
            all.dedup();
            all.len()
        };
        assert_eq!(unique, 4, "two lights sharing a colour");
    }

    /// Every supervisor state gets a token of its own, because a client
    /// switching on the string is the point of having one.
    #[test]
    fn state_slugs_are_distinct_and_stable() {
        let state = WorkspaceState::default();
        let slug =
            |supervisor| assemble(vec![facts("calm-1", supervisor)], &state, &[])[0].state_slug();
        let all = [
            slug(SupervisorState::NoConfig),
            slug(SupervisorState::ConfigDetected),
            slug(SupervisorState::Building),
            slug(SupervisorState::Starting),
            slug(running()),
            slug(SupervisorState::Failed {
                message: "boom".into(),
            }),
            slug(SupervisorState::Stopped),
        ];
        let mut unique = all.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len(), "two states sharing a token");
        assert_eq!(slug(running()), "running");
        assert_eq!(slug(SupervisorState::NoConfig), "no-config");
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
