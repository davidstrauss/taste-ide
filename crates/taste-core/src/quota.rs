//! What the account's limits looked like the last time the API said so.
//!
//! The subscription behind the IDE's credential is one pool: the agent
//! fleet and the user's own interactive Claude use draw on the same
//! rolling windows. This is the shape of that pool as observed — never
//! polled, never asked for. The auth proxy terminates every Anthropic
//! response the fleet provokes, so it reads the rate-limit headers on
//! traffic it was already carrying and files what they said.
//!
//! **Everything here is as of a moment that has passed.** A snapshot
//! carries [`QuotaSnapshot::observed_at`] for exactly that reason: the
//! honest presentation is "38% as of four minutes ago", never "38%". A
//! quiet fleet has stale numbers and the UI must say so rather than imply
//! a liveness no passive observer can have.
//!
//! Per-environment spend stays where it is — in the proxy's own counters,
//! the breakdown under this total. This is the pool; that is who drew on
//! it.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// A snapshot older than this is too stale to show as a gauge.
///
/// Rate-limit windows are minutes long and subscription windows hours, so
/// an hour-old reading says nothing useful about now. The UI keeps saying
/// *when* it was taken; past this it stops drawing a bar.
pub const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// One replenishing limit, as a response header reported it.
///
/// Every field is optional because the API sends the family it considers
/// relevant to that request and no more — a missing header is "not said",
/// which is not the same as zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// The ceiling for this window.
    pub limit: Option<u64>,
    /// What is left of it.
    pub remaining: Option<u64>,
    /// When it is fully replenished.
    pub reset: Option<SystemTime>,
}

impl Window {
    /// Nothing was said about this window at all.
    pub fn is_silent(&self) -> bool {
        self.limit.is_none() && self.remaining.is_none() && self.reset.is_none()
    }

    /// The share of the window consumed, 0.0 to 1.0.
    ///
    /// `None` when the headers did not carry both halves — a remaining
    /// count with no limit is a number, not a fraction, and guessing a
    /// denominator would be inventing data.
    pub fn utilization(&self) -> Option<f64> {
        let limit = self.limit?;
        let remaining = self.remaining?;
        if limit == 0 {
            return None;
        }
        let used = limit.saturating_sub(remaining) as f64 / limit as f64;
        Some(used.clamp(0.0, 1.0))
    }

    /// How long until this window is whole again, as of `now`.
    ///
    /// `None` once the reset is in the past: a window that has already
    /// replenished has no countdown, and a negative one would read as a
    /// countdown to nothing.
    pub fn resets_in(&self, now: SystemTime) -> Option<Duration> {
        self.reset?
            .duration_since(now)
            .ok()
            .filter(|d| !d.is_zero())
    }
}

/// A subscription window — the five-hour or weekly allowance a Pro/Max
/// plan is metered against, rather than a per-minute API rate limit.
///
/// Distinguished from [`Window`] because the API may report it as a
/// utilization directly (a percentage of the allowance consumed) with no
/// token counts attached, and because it is the one a subscriber
/// recognises: this is the number that stops their work.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanWindow {
    /// What the API called this window, verbatim where it named one.
    pub label: Option<String>,
    /// The share consumed, 0.0 to 1.0, when reported as a utilization.
    pub utilization: Option<f64>,
    /// Counts, when reported as counts instead.
    pub window: Window,
    /// A status word the API attached, verbatim. Never interpreted here:
    /// an unknown value is shown as it arrived rather than mapped onto a
    /// meaning we invented.
    pub status: Option<String>,
}

impl PlanWindow {
    /// The share consumed, however it was reported.
    pub fn used(&self) -> Option<f64> {
        self.utilization.or_else(|| self.window.utilization())
    }

    pub fn resets_in(&self, now: SystemTime) -> Option<Duration> {
        self.window.resets_in(now)
    }

    fn is_silent(&self) -> bool {
        self.label.is_none()
            && self.utilization.is_none()
            && self.status.is_none()
            && self.window.is_silent()
    }
}

/// A refusal for want of quota — the authoritative "closed until" signal.
///
/// Utilization headers describe headroom; this describes its absence, and
/// it is the one reading that needs no inference. A 429 the upstream sent
/// is the account saying it will not serve until `until`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exhaustion {
    /// When the refusal was seen.
    pub observed_at: Option<SystemTime>,
    /// `retry-after`, as sent.
    pub retry_after: Option<Duration>,
    /// When the window reopens: `retry-after` applied to the moment of
    /// refusal, or a reset header if one came with it.
    pub until: Option<SystemTime>,
    /// The API's own error message, for the tooltip. Never a token, never
    /// a body beyond this string.
    pub message: Option<String>,
}

impl Exhaustion {
    /// Whether this refusal still stands as of `now`.
    pub fn is_current(&self, now: SystemTime) -> bool {
        match self.until {
            Some(until) => until > now,
            // A refusal with no stated reopening is only worth showing
            // while it is fresh; otherwise it would sit there forever.
            None => match self.observed_at {
                Some(at) => now
                    .duration_since(at)
                    .map(|d| d < STALE_AFTER)
                    .unwrap_or(false),
                None => false,
            },
        }
    }
}

/// Which window a headline reading came from, so the UI can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Meter {
    /// The subscription's rolling session window.
    Session,
    /// The subscription's weekly window.
    Weekly,
    /// A per-minute API rate limit — requests.
    Requests,
    /// A per-minute API rate limit — tokens.
    Tokens,
}

impl Meter {
    pub fn label(self) -> &'static str {
        match self {
            Meter::Session => "session window",
            Meter::Weekly => "weekly window",
            Meter::Requests => "requests per minute",
            Meter::Tokens => "tokens per minute",
        }
    }
}

/// The one number a gauge shows, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Headline {
    pub meter: Meter,
    /// Share consumed, 0.0 to 1.0.
    pub used: f64,
    pub resets_in: Option<Duration>,
}

/// Everything the last observed response said about the account's limits.
///
/// Workspace-global on purpose: the subscription is one pool, so there is
/// one snapshot no matter which environment provoked the response that
/// carried it.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// When these numbers were read off a response. The whole point.
    pub observed_at: Option<SystemTime>,
    /// The environment whose turn happened to carry the reading. Not an
    /// attribution of the usage — the pool is shared — only provenance.
    pub observed_for: Option<String>,
    /// The subscription's rolling session allowance.
    pub session: PlanWindow,
    /// The subscription's weekly allowance, where it is distinguishable.
    pub weekly: PlanWindow,
    /// Documented per-minute API rate limits.
    pub requests: Window,
    pub tokens: Window,
    pub input_tokens: Window,
    pub output_tokens: Window,
    /// A quota refusal, while one stands.
    pub exhausted: Option<Exhaustion>,
    /// Rate-limit headers that arrived and are not modelled above, name
    /// and value as received.
    ///
    /// Kept deliberately: what an OAuth subscription actually sends is an
    /// observation, not a promise, and a snapshot that silently dropped
    /// the parts it did not recognise would make the next person guess.
    /// Bounded, and shown only where raw detail belongs.
    pub other: Vec<(String, String)>,
}

impl QuotaSnapshot {
    /// Nothing has been observed yet.
    pub fn is_empty(&self) -> bool {
        self.observed_at.is_none()
    }

    /// How long ago this was read.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        now.duration_since(self.observed_at?).ok()
    }

    /// Too old to draw as a gauge, though still worth stating as history.
    pub fn is_stale(&self, now: SystemTime) -> bool {
        self.age(now).map(|age| age > STALE_AFTER).unwrap_or(true)
    }

    /// The refusal in force, if one is.
    pub fn current_exhaustion(&self, now: SystemTime) -> Option<&Exhaustion> {
        self.exhausted.as_ref().filter(|e| e.is_current(now))
    }

    /// The single reading a compact gauge should show.
    ///
    /// Subscription windows outrank per-minute limits: a subscriber's
    /// question is "how much of my plan is left", and an ITPM bucket that
    /// refills in sixty seconds is not an answer to it. Between the two
    /// subscription windows, whichever is fuller — that is the one that
    /// will stop the work first. On a tie, the session window: it is the
    /// one that closes soonest, and on a fresh account both windows sit
    /// at the same low number, where naming the weekly one would be
    /// technically true and quietly alarming.
    pub fn headline(&self, now: SystemTime) -> Option<Headline> {
        // Weekly first, so `max_by` — which keeps the last of equals —
        // settles a tie on the session window.
        let plans = [
            (Meter::Weekly, &self.weekly),
            (Meter::Session, &self.session),
        ];
        let plan = plans
            .into_iter()
            .filter_map(|(meter, plan)| {
                plan.used().map(|used| Headline {
                    meter,
                    used,
                    resets_in: plan.resets_in(now),
                })
            })
            .max_by(|a, b| a.used.total_cmp(&b.used));
        if let Some(plan) = plan {
            return Some(plan);
        }

        // No subscription window was reported. The per-minute limits are
        // a true statement about headroom, so show the tightest of them
        // rather than nothing — labelled as what it is.
        let rates = [
            (Meter::Tokens, &self.tokens),
            (Meter::Tokens, &self.input_tokens),
            (Meter::Tokens, &self.output_tokens),
            (Meter::Requests, &self.requests),
        ];
        rates
            .into_iter()
            .filter_map(|(meter, window)| {
                window.utilization().map(|used| Headline {
                    meter,
                    used,
                    resets_in: window.resets_in(now),
                })
            })
            .max_by(|a, b| a.used.total_cmp(&b.used))
    }

    /// Fold a fresh reading into the standing one.
    ///
    /// Merged rather than replaced because a response carries the header
    /// families the API considered relevant to *that* request: a reply
    /// that mentions only per-minute buckets must not erase what the last
    /// one said about the plan window. A window is only overwritten by a
    /// window that says something.
    ///
    /// `served` — whether the response was one the account actually
    /// answered — is what clears a refusal. That is the only honest way
    /// to learn a window reopened without asking: traffic went through.
    pub fn observe(&mut self, fresh: Option<QuotaSnapshot>, served: bool) {
        if served {
            self.exhausted = None;
        }
        let Some(fresh) = fresh else {
            return;
        };
        self.observed_at = fresh.observed_at;
        self.observed_for = fresh.observed_for;
        if !fresh.session.is_silent() {
            self.session = fresh.session;
        }
        if !fresh.weekly.is_silent() {
            self.weekly = fresh.weekly;
        }
        for (mine, theirs) in [
            (&mut self.requests, fresh.requests),
            (&mut self.tokens, fresh.tokens),
            (&mut self.input_tokens, fresh.input_tokens),
            (&mut self.output_tokens, fresh.output_tokens),
        ] {
            if !theirs.is_silent() {
                *mine = theirs;
            }
        }
        if fresh.exhausted.is_some() {
            self.exhausted = fresh.exhausted;
        }
        if !fresh.other.is_empty() {
            self.other = fresh.other;
        }
    }

    /// Whether anything beyond the observation time was actually learned.
    ///
    /// A response can carry no rate-limit headers at all; saying so is
    /// better than drawing an empty gauge.
    pub fn says_anything(&self) -> bool {
        !self.session.is_silent()
            || !self.weekly.is_silent()
            || !self.requests.is_silent()
            || !self.tokens.is_silent()
            || !self.input_tokens.is_silent()
            || !self.output_tokens.is_silent()
            || self.exhausted.is_some()
            || !self.other.is_empty()
    }
}

/// "4 min ago", "just now" — one phrasing for every place a snapshot age
/// is shown, so the panel and the transcript never disagree about how old
/// the same reading is.
pub fn describe_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=44 => "just now".to_string(),
        45..=5399 => {
            let mins = ((secs as f64) / 60.0).round().max(1.0) as u64;
            format!("{mins} min ago")
        }
        _ => {
            let hours = ((secs as f64) / 3600.0).round().max(1.0) as u64;
            format!("{hours} h ago")
        }
    }
}

/// "in 2 h 14 min", for a reset countdown.
pub fn describe_countdown(left: Duration) -> String {
    let secs = left.as_secs();
    if secs < 60 {
        return "in under a minute".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("in {mins} min");
    }
    let hours = mins / 60;
    let rest = mins % 60;
    if hours < 24 {
        if rest == 0 {
            format!("in {hours} h")
        } else {
            format!("in {hours} h {rest} min")
        }
    } else {
        let days = hours / 24;
        let rest = hours % 24;
        if rest == 0 {
            format!("in {days} d")
        } else {
            format!("in {days} d {rest} h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn utilization_needs_both_halves() {
        let window = Window {
            limit: Some(100),
            remaining: Some(25),
            reset: None,
        };
        assert_eq!(window.utilization(), Some(0.75));

        // A remaining count with no ceiling is not a fraction.
        let partial = Window {
            limit: None,
            remaining: Some(25),
            reset: None,
        };
        assert_eq!(partial.utilization(), None);
    }

    #[test]
    fn a_past_reset_has_no_countdown() {
        let window = Window {
            limit: None,
            remaining: None,
            reset: Some(at(100)),
        };
        assert_eq!(window.resets_in(at(40)), Some(Duration::from_secs(60)));
        assert_eq!(window.resets_in(at(140)), None);
    }

    #[test]
    fn the_headline_prefers_the_plan_and_then_the_fullest() {
        let mut snapshot = QuotaSnapshot {
            observed_at: Some(at(1_000)),
            tokens: Window {
                limit: Some(100),
                remaining: Some(10),
                reset: Some(at(1_060)),
            },
            ..Default::default()
        };
        // With only per-minute limits, that is what is shown — named as
        // what it is, not as a plan window.
        let headline = snapshot.headline(at(1_000)).unwrap();
        assert_eq!(headline.meter, Meter::Tokens);
        assert!((headline.used - 0.9).abs() < f64::EPSILON);

        // A subscription window outranks it even when less full.
        snapshot.session.utilization = Some(0.4);
        snapshot.weekly.utilization = Some(0.62);
        let headline = snapshot.headline(at(1_000)).unwrap();
        assert_eq!(headline.meter, Meter::Weekly, "the fuller plan window");
        assert!((headline.used - 0.62).abs() < f64::EPSILON);

        // Level, which is what a fresh account looks like: the session
        // window is the one that closes first, so it is the one named.
        snapshot.weekly.utilization = Some(0.4);
        assert_eq!(snapshot.headline(at(1_000)).unwrap().meter, Meter::Session);
    }

    #[test]
    fn an_exhaustion_expires_when_its_window_reopens() {
        let refusal = Exhaustion {
            observed_at: Some(at(500)),
            retry_after: Some(Duration::from_secs(300)),
            until: Some(at(800)),
            message: None,
        };
        assert!(refusal.is_current(at(700)));
        assert!(!refusal.is_current(at(900)));
    }

    #[test]
    fn a_partial_reading_does_not_erase_what_is_known() {
        let mut standing = QuotaSnapshot::default();
        standing.observe(
            Some(QuotaSnapshot {
                observed_at: Some(at(100)),
                session: PlanWindow {
                    utilization: Some(0.5),
                    ..Default::default()
                },
                ..Default::default()
            }),
            true,
        );
        // A later response that mentions only a per-minute bucket.
        standing.observe(
            Some(QuotaSnapshot {
                observed_at: Some(at(160)),
                requests: Window {
                    limit: Some(10),
                    remaining: Some(4),
                    reset: None,
                },
                ..Default::default()
            }),
            true,
        );
        assert_eq!(standing.session.used(), Some(0.5), "the plan window stood");
        assert_eq!(standing.requests.remaining, Some(4));
        assert_eq!(standing.observed_at, Some(at(160)), "freshly observed");
    }

    #[test]
    fn a_served_response_reopens_a_closed_window() {
        let mut standing = QuotaSnapshot {
            exhausted: Some(Exhaustion {
                observed_at: Some(at(10)),
                until: Some(at(6_000)),
                ..Default::default()
            }),
            ..Default::default()
        };
        // A response that says nothing about limits, but was served: the
        // account is answering again, whatever the countdown claimed.
        standing.observe(None, true);
        assert!(standing.exhausted.is_none());
    }

    #[test]
    fn ages_and_countdowns_read_like_english() {
        assert_eq!(describe_age(Duration::from_secs(10)), "just now");
        assert_eq!(describe_age(Duration::from_secs(240)), "4 min ago");
        assert_eq!(describe_age(Duration::from_secs(7_200)), "2 h ago");
        assert_eq!(
            describe_countdown(Duration::from_secs(30)),
            "in under a minute"
        );
        assert_eq!(describe_countdown(Duration::from_secs(600)), "in 10 min");
        assert_eq!(
            describe_countdown(Duration::from_secs(8_040)),
            "in 2 h 14 min"
        );
        assert_eq!(
            describe_countdown(Duration::from_secs(180_000)),
            "in 2 d 2 h"
        );
    }
}
