//! **Reinstantiating an environment on updated versions.**
//!
//! Two things age under an environment that nothing updates in place, and
//! each is fixed by making the environment again rather than patching it
//! (David, 2026-09-22):
//!
//! - **the guest** ([`Kind::Guest`]): a VM keeps the release it was built
//!   from for its whole life (`crate::guest`), so when the stable stream
//!   moves on, the environments in an older VM move to a newer one —
//!   through snapshot and restore, their conversation carried in their
//!   home volume;
//! - **the packages** ([`Kind::Packages`]): an image's layers are cached,
//!   so its packages are what they were the first time it was built. Once
//!   a day ([`PACKAGES_CHECK_EVERY`]) its package manager is asked, in a
//!   running container, whether anything has an update — seconds, where a
//!   rebuild is minutes — and only a yes (or a manager nobody can ask, or a
//!   week since the last build from nothing, [`PACKAGES_BACKSTOP`]) makes a
//!   refresh pending. The refresh is per image, not per environment: the
//!   first environment on an image rebuilds it from nothing — base pulled,
//!   no cache — and the others restart on what it built (David,
//!   2026-09-22: "minimize the work while starting fresh frequently").
//!
//! Either restarts the environment's container under its agent, so it is
//! the agent's to time and the coordinator's to approve, with the same
//! rules for both ("with the same notifications and constraints"):
//!
//! - the environment's agent is told when one is pending, and again every
//!   [`NUDGE_EVERY`] until it asks (`environment_reinstantiate_request`);
//! - the coordinator approves (`environment_reinstantiate`), and is told
//!   once when an environment has waited [`TELL_COORDINATOR_AFTER`] on it;
//! - at [`FORCE_AFTER`] from when it became pending it happens anyway.
//!
//! A pending move takes the place of a package refresh, since a new VM
//! holds no cache and builds the image from nothing anyway. An environment
//! with nothing running has nobody to ask and moves at once. This module is
//! the clock and the record; the work is the registry's
//! (`EnvironmentRegistry::relocate`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How often the environment's agent is told, until it asks.
pub const NUDGE_EVERY: Duration = Duration::from_secs(10 * 60);
/// How long a move waits on approval before the coordinator is told.
pub const TELL_COORDINATOR_AFTER: Duration = Duration::from_secs(60 * 60);
/// How long after it became pending a move is forced.
pub const FORCE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);
/// How long a move that failed waits before it is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);
/// How often an image's package manager is asked whether it has updates,
/// and how recent another environment's build from nothing must be for a
/// refresh to reuse it rather than build again.
pub const PACKAGES_CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The longest an image goes without a build from nothing, whatever the
/// check says: what a package manager does not see — a tool fetched by
/// `curl`, a `pip install` — is only refreshed by a rebuild.
pub const PACKAGES_BACKSTOP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What is out of date, and so what reinstantiating does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The VM is behind the stream: move to a VM on the current release.
    #[default]
    Guest,
    /// The image's packages have updates, or are a week old: rebuild it
    /// from nothing, or restart on another environment's rebuild of it.
    Packages,
}

/// One environment's pending move, as recorded beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migration {
    /// What is out of date. Records from before package refreshes are
    /// guest moves.
    #[serde(default)]
    pub kind: Kind,
    /// The VM it is in, and the release that VM runs.
    pub from_vm: String,
    pub from_release: String,
    /// The release it moves to.
    pub to_release: String,
    /// Unix seconds: when the move became pending.
    pub pending_since: u64,
    /// When the environment's agent asked for it.
    #[serde(default)]
    pub requested_at: Option<u64>,
    /// When the agent was last told.
    #[serde(default)]
    pub last_nudge: Option<u64>,
    /// Whether the coordinator has been told it has waited.
    #[serde(default)]
    pub coordinator_told: bool,
    /// When a move was last tried and failed, so a forced one is not tried
    /// again every minute.
    #[serde(default)]
    pub failed_at: Option<u64>,
}

/// What the clock says is due for a pending move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Tell the environment's agent, again.
    Nudge,
    /// Tell the coordinator the move has waited an hour on its approval.
    TellCoordinator,
    /// Move it now.
    Force,
}

impl Migration {
    pub const FILE: &'static str = "migration.json";

    pub fn new(from_vm: &str, from_release: &str, to_release: &str, now: u64) -> Self {
        Self {
            kind: Kind::Guest,
            from_vm: from_vm.to_string(),
            from_release: from_release.to_string(),
            to_release: to_release.to_string(),
            pending_since: now,
            requested_at: None,
            last_nudge: None,
            coordinator_told: false,
            failed_at: None,
        }
    }

    /// A package refresh for an environment in `vm`, pending because of
    /// `why` — "package updates are available", "a week has passed since
    /// its last build from nothing".
    pub fn packages(vm: &str, release: &str, why: &str, now: u64) -> Self {
        Self {
            kind: Kind::Packages,
            // For a refresh, `to_release` carries why, which is what the
            // words about it say.
            to_release: why.to_string(),
            ..Self::new(vm, release, release, now)
        }
    }

    /// What reinstantiating does, as a fleet row says it.
    pub fn row_words(&self) -> String {
        match self.kind {
            Kind::Guest => format!("moving to a VM on {}", self.to_release),
            Kind::Packages => "rebuilding with updated packages".to_string(),
        }
    }

    /// What is out of date and what fixing it does, in a sentence — the
    /// part of every message that differs by kind.
    fn situation(&self, env: &str) -> String {
        match self.kind {
            Kind::Guest => format!(
                "Environment {env} runs in VM {} on Fedora CoreOS {}; the stable release is now \
                 {}. It must be reinstantiated in a VM on {}: the checkout is snapshotted with \
                 its uncommitted work, the container stops, and both come back in the new VM \
                 with this conversation, in a few minutes.",
                self.from_vm, self.from_release, self.to_release, self.to_release
            ),
            Kind::Packages => format!(
                "Environment {env}'s image is out of date: {}. It must be reinstantiated on \
                 updated packages: the image is rebuilt without the cache — or taken as another \
                 environment already rebuilt it today — and the container restarts on it, in a \
                 few minutes; the checkout, uncommitted work, and this conversation stay as they \
                 are.",
                self.to_release
            ),
        }
    }

    /// When it will be forced, in Unix seconds.
    pub fn forced_at(&self) -> u64 {
        self.pending_since + FORCE_AFTER.as_secs()
    }

    /// What is due at `now`. A forced move is all there is once it is
    /// due; before that the agent is told every [`NUDGE_EVERY`] until it
    /// asks, and the coordinator once, an hour after the wait on it began
    /// — at the agent's request, or at the move becoming pending when the
    /// agent has not asked.
    pub fn due(&self, now: u64) -> Vec<Due> {
        if now >= self.forced_at() {
            let retry = self
                .failed_at
                .is_none_or(|at| now.saturating_sub(at) >= RETRY_AFTER.as_secs());
            return if retry { vec![Due::Force] } else { Vec::new() };
        }
        let mut due = Vec::new();
        if self.requested_at.is_none()
            && self
                .last_nudge
                .is_none_or(|at| now.saturating_sub(at) >= NUDGE_EVERY.as_secs())
        {
            due.push(Due::Nudge);
        }
        let waiting_since = self.requested_at.unwrap_or(self.pending_since);
        if !self.coordinator_told
            && now.saturating_sub(waiting_since) >= TELL_COORDINATOR_AFTER.as_secs()
        {
            due.push(Due::TellCoordinator);
        }
        due
    }

    pub fn read(env_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(env_dir.join(Self::FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write(&self, env_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(env_dir)?;
        let path = env_dir.join(Self::FILE);
        let part = path.with_extension("json.part");
        std::fs::write(&part, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", part.display()))?;
        std::fs::rename(&part, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn clear(env_dir: &Path) {
        let _ = std::fs::remove_file(env_dir.join(Self::FILE));
    }

    /// What the environment's agent is told: what is pending, what it
    /// costs, what to call, and when it happens regardless — the evidence
    /// in the words, so nothing has to be looked up first.
    pub fn nudge_text(&self, env: &str, now: u64) -> String {
        let left = self.forced_at().saturating_sub(now) / 60;
        format!(
            "{} You resume here afterwards.\n\
             At your next stopping point — nothing half-written, no command running — call \
             environment_reinstantiate_request. The coordinator approves it. If it has not \
             happened in {left} minutes, it happens anyway, wherever you are.",
            self.situation(env)
        )
    }

    /// What the coordinator is told after the wait.
    pub fn coordinator_text(&self, env: &str, now: u64) -> String {
        let left = self.forced_at().saturating_sub(now) / 60;
        let asked = match self.requested_at {
            Some(at) => format!(
                "Its agent asked {} minutes ago and is waiting on your approval.",
                now.saturating_sub(at) / 60
            ),
            None => "Its agent has been told every 10 minutes and has not asked yet.".to_string(),
        };
        format!(
            "{} {asked} Approve it with environment_reinstantiate \
             {{\"environment\": \"{env}\"}} when its work is at a stopping point (the \
             environment tool shows its state). It is forced in {left} minutes if you do not.",
            self.situation(env)
        )
    }

    /// What the coordinator is told when an agent asks.
    pub fn request_text(&self, env: &str, now: u64) -> String {
        let left = self.forced_at().saturating_sub(now) / 60;
        format!(
            "Environment {env}'s agent asks to be reinstantiated now. {} Approve it with \
             environment_reinstantiate {{\"environment\": \"{env}\"}}. It is forced in {left} \
             minutes if you do not.",
            self.situation(env)
        )
    }
}

/// What is known of one image's packages in one VM: the workspace keeps
/// one of these per image, since environments with one config share an
/// image, and one check or one rebuild answers for all of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecord {
    /// Unix seconds: the image's last build from nothing.
    #[serde(default)]
    pub fresh_at: Option<u64>,
    /// When its package manager last said nothing had an update.
    #[serde(default)]
    pub checked_at: Option<u64>,
    /// What the last check found, when it found something: why the image
    /// is due a rebuild. Cleared by the rebuild.
    #[serde(default)]
    pub stale: Option<String>,
}

/// The check an image's package manager is asked, as root in a running
/// container: one word out — `updates`, `none`, or `unknown` for a manager
/// it cannot ask. dnf's `check-update` exits 100 with updates; apt and apk
/// are asked after their indexes are refreshed.
pub const PACKAGE_CHECK_SCRIPT: &str = r#"
if command -v dnf >/dev/null 2>&1; then
    dnf -q check-update >/dev/null 2>&1
    case $? in 100) echo updates ;; 0) echo none ;; *) echo unknown ;; esac
elif command -v apt-get >/dev/null 2>&1; then
    if apt-get update -qq >/dev/null 2>&1; then
        if apt-get -s -qq upgrade 2>/dev/null | grep -q '^Inst '; then echo updates; else echo none; fi
    else
        echo unknown
    fi
elif command -v apk >/dev/null 2>&1; then
    if apk update -q >/dev/null 2>&1; then
        if [ -n "$(apk -u list 2>/dev/null)" ]; then echo updates; else echo none; fi
    else
        echo unknown
    fi
else
    echo unknown
fi
"#;

/// An image's age as the words about it say it: hours under two days,
/// days after.
pub fn age_words(secs: u64) -> String {
    let hours = secs / 3600;
    if hours < 48 {
        format!("{hours} hours")
    } else {
        format!("{} days", hours / 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60;

    fn pending() -> Migration {
        Migration::new("taste-a", "44.20260829.3.1", "44.20260912.3.0", 1_000_000)
    }

    #[test]
    fn the_agent_is_told_at_once_and_every_ten_minutes_until_it_asks() {
        let mut m = pending();
        let t0 = m.pending_since;
        assert_eq!(m.due(t0), [Due::Nudge]);
        m.last_nudge = Some(t0);
        assert!(m.due(t0 + 9 * MIN).is_empty());
        assert_eq!(m.due(t0 + 10 * MIN), [Due::Nudge]);
        m.requested_at = Some(t0 + 12 * MIN);
        assert!(m.due(t0 + 30 * MIN).is_empty(), "no nudges once it asked");
    }

    #[test]
    fn the_coordinator_is_told_once_after_an_hour_waiting_on_it() {
        let mut m = pending();
        let t0 = m.pending_since;
        m.last_nudge = Some(t0 + 55 * MIN);
        assert_eq!(m.due(t0 + 60 * MIN), [Due::TellCoordinator]);
        m.coordinator_told = true;
        assert!(m.due(t0 + 61 * MIN).is_empty());

        // Asked at 30 minutes: the hour runs from the request.
        let mut asked = pending();
        asked.requested_at = Some(t0 + 30 * MIN);
        assert!(asked.due(t0 + 80 * MIN).is_empty());
        assert_eq!(asked.due(t0 + 90 * MIN), [Due::TellCoordinator]);
    }

    #[test]
    fn at_two_hours_it_is_forced_whatever_else_stands() {
        let mut m = pending();
        m.requested_at = Some(m.pending_since + 110 * MIN);
        assert_eq!(m.due(m.pending_since + 120 * MIN), [Due::Force]);
    }

    #[test]
    fn a_forced_move_that_failed_waits_before_it_is_tried_again() {
        let mut m = pending();
        let due = m.pending_since + 120 * MIN;
        m.failed_at = Some(due);
        assert!(m.due(due + 5 * MIN).is_empty());
        assert_eq!(m.due(due + 15 * MIN), [Due::Force]);
    }

    #[test]
    fn a_record_survives_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let m = pending();
        m.write(dir.path()).unwrap();
        assert_eq!(Migration::read(dir.path()), Some(m));
        Migration::clear(dir.path());
        assert_eq!(Migration::read(dir.path()), None);
    }

    #[test]
    fn the_words_name_the_releases_the_tool_and_the_deadline() {
        let m = pending();
        let nudge = m.nudge_text("i-0007", m.pending_since + 20 * MIN);
        assert!(
            nudge.contains("44.20260912.3.0")
                && nudge.contains("environment_reinstantiate_request")
        );
        assert!(nudge.contains("100 minutes"), "{nudge}");
        let told = m.coordinator_text("i-0007", m.pending_since + 60 * MIN);
        assert!(
            told.contains("environment_reinstantiate") && told.contains("60 minutes"),
            "{told}"
        );
        assert_eq!(age_words(30 * 3600), "30 hours");
        assert_eq!(age_words(50 * 3600), "2 days");
        let refresh = Migration::packages(
            "taste-a",
            "44.20260829.3.1",
            "a week has passed since its last build from nothing (9 days)",
            m.pending_since,
        );
        let nudge = refresh.nudge_text("i-0007", refresh.pending_since);
        assert!(
            nudge.contains("9 days") && nudge.contains("without the cache"),
            "{nudge}"
        );
        assert_eq!(refresh.row_words(), "rebuilding with updated packages");
    }

    /// The check runs in whatever shell the image has, so it is plain sh
    /// that parses — and on a host with none of the three managers it says
    /// it cannot ask rather than claiming there is nothing.
    #[test]
    fn the_package_check_is_plain_sh_and_admits_when_it_cannot_ask() {
        let parsed = std::process::Command::new("sh")
            .args(["-n", "-c", PACKAGE_CHECK_SCRIPT])
            .status()
            .unwrap();
        assert!(parsed.success());
        // By its path: a PATH set for the child is where Command looks.
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", PACKAGE_CHECK_SCRIPT])
            .env("PATH", "/nonexistent")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "unknown");
    }

    /// A record written before package refreshes reads as a guest move.
    #[test]
    fn an_older_record_is_a_guest_move() {
        let text =
            r#"{"from_vm":"taste-a","from_release":"44.1","to_release":"44.2","pending_since":5}"#;
        let m: Migration = serde_json::from_str(text).unwrap();
        assert_eq!(m.kind, Kind::Guest);
    }
}
