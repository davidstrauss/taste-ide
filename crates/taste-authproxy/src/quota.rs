//! Reading the account's limit state off responses we were already carrying.
//!
//! # Why this is passive, and stays passive
//!
//! Claude Code's own `/usage` asks an endpoint. That endpoint is not
//! documented, so it is not ours to call: per CLAUDE.md the IDE speaks
//! documented interfaces or none, and "the other client does it" is not a
//! specification. What *is* ours is the response to a request we made:
//! the proxy terminates every Anthropic response the fleet provokes, and
//! reading the rate-limit headers on it is reading our own mail.
//!
//! The cost of that choice is honest and permanent: **there is no reading
//! without traffic.** A quiet fleet has no fresh numbers, and nothing here
//! will ever manufacture one — no synthetic request to "refresh" a gauge,
//! which would spend the user's quota to describe their quota. Every
//! snapshot is stamped with when it was taken and the UI says so.
//!
//! # What is documented, and what is merely observed
//!
//! Documented, and parsed into named fields
//! (<https://platform.claude.com/docs/en/api/rate-limits#response-headers>):
//!
//! - `anthropic-ratelimit-requests-{limit,remaining,reset}`
//! - `anthropic-ratelimit-tokens-{limit,remaining,reset}`
//! - `anthropic-ratelimit-input-tokens-{limit,remaining,reset}`
//! - `anthropic-ratelimit-output-tokens-{limit,remaining,reset}`
//! - `retry-after` on a 429
//!
//! Those describe the per-minute API rate limits. They are *not* the
//! five-hour and weekly windows a subscription is metered against, and the
//! docs describe no header that is. So the subscription half is written to
//! **discover** rather than to assume: any `anthropic-ratelimit-*` header
//! this proxy does not recognise is kept verbatim in
//! [`QuotaSnapshot::other`], and names that identify themselves as a
//! unified or plan window are mapped onto the session and weekly windows
//! by shape alone. If a subscription reports nothing, the UI says nothing
//! — see the empty state. Inventing a plausible percentage would be worse
//! than an empty gauge, because a wrong quota number is one the user acts
//! on.
//!
//! # The one authoritative signal
//!
//! Utilization headers are a description of headroom. A 429 is the account
//! itself declining to serve, and it carries `retry-after`: that is the
//! reading that needs no interpretation, and it is recorded as
//! [`Exhaustion`] whatever the headers did or did not say.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::header::HeaderMap;
use http::StatusCode;
use taste_core::quota::{Exhaustion, QuotaSnapshot, Window};

/// How many unrecognised rate-limit headers to keep. Enough to see a new
/// family arrive; not enough for a misbehaving upstream to grow the
/// snapshot without bound.
const MAX_OTHER: usize = 16;

/// The most of a refusal body worth reading for its message.
pub const MAX_REFUSAL_BODY: usize = 2048;

/// Read whatever one response says about the account's limits.
///
/// `None` when it said nothing at all — which is the common case for a
/// non-Messages request, and must not be allowed to overwrite a good
/// snapshot with an empty one.
pub fn harvest(
    status: StatusCode,
    headers: &HeaderMap,
    observed_at: SystemTime,
    environment: &str,
) -> Option<QuotaSnapshot> {
    let mut snapshot = QuotaSnapshot {
        observed_at: Some(observed_at),
        observed_for: Some(environment.to_string()),
        ..Default::default()
    };

    for (name, value) in headers.iter() {
        let name = name.as_str().to_ascii_lowercase();
        let Ok(value) = value.to_str() else {
            continue;
        };
        let value = value.trim();
        let Some(rest) = name.strip_prefix("anthropic-ratelimit-") else {
            continue;
        };
        if !absorb(&mut snapshot, rest, value, observed_at) && snapshot.other.len() < MAX_OTHER {
            snapshot.other.push((name, value.to_string()));
        }
    }

    let retry_after = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_retry_after(v.trim(), observed_at));

    // A quota refusal. 429 is the rate-limit and plan-limit case; the
    // spend-cap variant is also a 429 but carries no `retry-after`, which
    // is why `until` is optional rather than assumed.
    if status == StatusCode::TOO_MANY_REQUESTS {
        let until = retry_after
            .map(|after| observed_at + after)
            .or_else(|| soonest_reset(&snapshot));
        snapshot.exhausted = Some(Exhaustion {
            observed_at: Some(observed_at),
            retry_after,
            until,
            message: None,
        });
    }

    snapshot.says_anything().then_some(snapshot)
}

/// Attach the message from a refusal body to a snapshot's [`Exhaustion`].
///
/// Only the API's own `error.message` — the sentence that names which
/// window closed and when it reopens, which is the whole value of showing
/// a refusal at all. Nothing else from the body is read or kept.
pub fn attach_refusal_message(snapshot: &mut QuotaSnapshot, body: &[u8]) {
    let Some(exhausted) = snapshot.exhausted.as_mut() else {
        return;
    };
    if exhausted.message.is_some() {
        return;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    let message = json
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .map(str::trim)
        .filter(|message| !message.is_empty());
    if let Some(message) = message {
        exhausted.message = Some(message.chars().take(400).collect());
    }
}

/// One `anthropic-ratelimit-*` header into the snapshot.
///
/// `false` means "not recognised", and the caller keeps it verbatim.
fn absorb(snapshot: &mut QuotaSnapshot, rest: &str, value: &str, now: SystemTime) -> bool {
    if let Some(family) = rest.strip_suffix("-limit") {
        return match window_for(snapshot, family) {
            Some(window) => {
                window.limit = value.parse().ok();
                window.limit.is_some()
            }
            None => plan_field(snapshot, family, Field::Limit, value, now),
        };
    }
    if let Some(family) = rest.strip_suffix("-remaining") {
        return match window_for(snapshot, family) {
            Some(window) => {
                window.remaining = value.parse().ok();
                window.remaining.is_some()
            }
            None => plan_field(snapshot, family, Field::Remaining, value, now),
        };
    }
    if let Some(family) = rest.strip_suffix("-reset") {
        return match window_for(snapshot, family) {
            Some(window) => {
                window.reset = parse_instant(value, now);
                window.reset.is_some()
            }
            None => plan_field(snapshot, family, Field::Reset, value, now),
        };
    }
    if let Some(family) = rest.strip_suffix("-status") {
        return plan_field(snapshot, family, Field::Status, value, now);
    }
    for suffix in ["-utilization", "-used", "-percent", "-usage"] {
        if let Some(family) = rest.strip_suffix(suffix) {
            return plan_field(snapshot, family, Field::Utilization, value, now);
        }
    }
    // A bare `anthropic-ratelimit-unified: <something>` is a name we do
    // not know the shape of; keep it verbatim rather than guess.
    false
}

/// The documented per-minute families, by name.
fn window_for<'a>(snapshot: &'a mut QuotaSnapshot, family: &str) -> Option<&'a mut Window> {
    match family {
        "requests" => Some(&mut snapshot.requests),
        "tokens" => Some(&mut snapshot.tokens),
        "input-tokens" => Some(&mut snapshot.input_tokens),
        "output-tokens" => Some(&mut snapshot.output_tokens),
        _ => None,
    }
}

enum Field {
    Limit,
    Remaining,
    Reset,
    Status,
    Utilization,
}

/// A header naming a subscription window rather than a per-minute limit.
///
/// Recognised by shape, because no documentation describes these: a family
/// that says `unified` or `plan` is the subscription's own accounting, and
/// a `5h`/`7d`-ish token inside it says which window. Anything else is not
/// forced into a slot — it is left for [`QuotaSnapshot::other`].
fn plan_field(
    snapshot: &mut QuotaSnapshot,
    family: &str,
    field: Field,
    value: &str,
    now: SystemTime,
) -> bool {
    let parts: Vec<&str> = family.split('-').filter(|p| !p.is_empty()).collect();
    if !parts
        .iter()
        .any(|part| matches!(*part, "unified" | "plan" | "subscription"))
    {
        return false;
    }
    let weekly = parts.iter().any(|part| {
        matches!(*part, "7d" | "week" | "weekly")
            || part.ends_with('d') && part.trim_end_matches('d').parse::<u32>().is_ok()
    });
    let plan = if weekly {
        &mut snapshot.weekly
    } else {
        &mut snapshot.session
    };
    if plan.label.is_none() {
        plan.label = Some(family.to_string());
    }
    match field {
        Field::Limit => {
            plan.window.limit = value.parse().ok();
            plan.window.limit.is_some()
        }
        Field::Remaining => {
            plan.window.remaining = value.parse().ok();
            plan.window.remaining.is_some()
        }
        Field::Reset => {
            plan.window.reset = parse_instant(value, now);
            plan.window.reset.is_some()
        }
        Field::Status => {
            plan.status = Some(value.to_string());
            true
        }
        Field::Utilization => match parse_fraction(value) {
            Some(fraction) => {
                plan.utilization = Some(fraction);
                true
            }
            None => false,
        },
    }
}

/// A utilization as a share of one, from either "0.42" or "42".
///
/// Ambiguous only at exactly 1, which both spellings agree means the
/// window is full either way.
fn parse_fraction(value: &str) -> Option<f64> {
    let value: f64 = value.trim_end_matches('%').trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let fraction = if value > 1.0 { value / 100.0 } else { value };
    Some(fraction.clamp(0.0, 1.0))
}

/// A reset time: RFC 3339 as documented, or a bare epoch or delta if that
/// is what turns up instead.
fn parse_instant(value: &str, now: SystemTime) -> Option<SystemTime> {
    if let Some(time) = parse_rfc3339(value) {
        return Some(time);
    }
    let number: f64 = value.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    // Under a decade of seconds is a delay, not a date. Nothing sends
    // absolute epoch times that small, and nothing sends a delay that
    // large.
    if number < 315_000_000.0 {
        Some(now + Duration::from_secs_f64(number))
    } else {
        Some(UNIX_EPOCH + Duration::from_secs_f64(number))
    }
}

/// `retry-after`, in either documented spelling: seconds, or an HTTP date.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // An HTTP-date is IMF-fixdate: "Wed, 21 Oct 2026 07:28:00 GMT".
    let when = parse_imf_fixdate(value)?;
    when.duration_since(now).ok()
}

fn soonest_reset(snapshot: &QuotaSnapshot) -> Option<SystemTime> {
    [
        snapshot.session.window.reset,
        snapshot.weekly.window.reset,
        snapshot.requests.reset,
        snapshot.tokens.reset,
        snapshot.input_tokens.reset,
        snapshot.output_tokens.reset,
    ]
    .into_iter()
    .flatten()
    .min()
}

/// RFC 3339, tolerant of a fractional second and of an offset.
fn parse_rfc3339(value: &str) -> Option<SystemTime> {
    let value = value.trim();
    let (date, rest) = value.split_once(['T', 't', ' '])?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // The offset, and what is left is the clock time.
    let (clock, offset) = split_offset(rest)?;
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next().unwrap_or("0").parse().ok()?;
    // Fractional seconds are dropped: a reset is a wall clock a human
    // reads, and sub-second precision on an hours-long window is noise.
    let second: i64 = clock
        .next()
        .unwrap_or("0")
        .split('.')
        .next()?
        .parse()
        .ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    (seconds >= 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

/// The trailing zone designator, as seconds east of UTC.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        return Some((clock, 0));
    }
    // `+01:00` / `-0500`, never the '-' inside a date (already consumed).
    let at = rest.rfind(['+', '-'])?;
    let (clock, zone) = rest.split_at(at);
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let zone = &zone[1..];
    let (hours, minutes) = match zone.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if zone.len() == 4 => zone.split_at(2),
        None => (zone, "0"),
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    Some((clock, sign * (hours * 3_600 + minutes * 60)))
}

/// "Wed, 21 Oct 2026 07:28:00 GMT".
fn parse_imf_fixdate(value: &str) -> Option<SystemTime> {
    let value = value.trim();
    let rest = value
        .split_once(", ")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    (seconds >= 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    fn epoch(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn the_documented_family_lands_in_named_fields() {
        // Exactly the headers the rate-limits page documents.
        let headers = headers(&[
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "999"),
            ("anthropic-ratelimit-requests-reset", "2026-09-01T12:05:00Z"),
            ("anthropic-ratelimit-input-tokens-limit", "2000000"),
            ("anthropic-ratelimit-input-tokens-remaining", "1400000"),
            (
                "anthropic-ratelimit-input-tokens-reset",
                "2026-09-01T12:01:30Z",
            ),
        ]);
        let now = parse_rfc3339("2026-09-01T12:00:00Z").unwrap();
        let snapshot = harvest(StatusCode::OK, &headers, now, "primary").unwrap();

        assert_eq!(snapshot.requests.limit, Some(1000));
        assert_eq!(snapshot.requests.remaining, Some(999));
        assert_eq!(
            snapshot.requests.resets_in(now),
            Some(Duration::from_secs(300))
        );
        assert_eq!(snapshot.input_tokens.utilization(), Some(0.3));
        assert_eq!(snapshot.observed_for.as_deref(), Some("primary"));
        assert!(snapshot.other.is_empty(), "{:?}", snapshot.other);

        // Per-minute limits are all there is here, so the headline says so
        // rather than dressing them up as a plan window.
        let headline = snapshot.headline(now).unwrap();
        assert_eq!(headline.meter, taste_core::quota::Meter::Tokens);
    }

    #[test]
    fn a_unified_family_becomes_the_plan_windows() {
        // Shape-recognised, not documented: whatever an OAuth subscription
        // sends, a name that says `unified` and `7d` is the weekly window.
        let headers = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "38"),
            ("anthropic-ratelimit-unified-5h-reset", "1756731600"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.62"),
        ]);
        let now = epoch(1_756_728_000);
        let snapshot = harvest(StatusCode::OK, &headers, now, "calm-1").unwrap();

        assert_eq!(snapshot.session.used(), Some(0.38));
        assert_eq!(
            snapshot.session.resets_in(now),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(snapshot.weekly.used(), Some(0.62));
        assert_eq!(snapshot.session.status.as_deref(), Some("allowed_warning"));

        // The weekly window is fuller, so it is what a gauge shows.
        let headline = snapshot.headline(now).unwrap();
        assert_eq!(headline.meter, taste_core::quota::Meter::Weekly);
    }

    #[test]
    fn an_unrecognised_ratelimit_header_is_kept_verbatim() {
        // The point of `other`: a family nobody here has seen must survive
        // to be looked at, not be silently dropped.
        let headers = headers(&[
            ("anthropic-ratelimit-quantum-flux", "sideways"),
            ("anthropic-ratelimit-requests-limit", "50"),
            ("x-not-ours", "ignored"),
        ]);
        let snapshot = harvest(StatusCode::OK, &headers, epoch(10), "primary").unwrap();
        assert_eq!(
            snapshot.other,
            vec![(
                "anthropic-ratelimit-quantum-flux".to_string(),
                "sideways".to_string()
            )]
        );
        assert_eq!(snapshot.requests.limit, Some(50));
    }

    #[test]
    fn a_response_that_says_nothing_yields_nothing() {
        // Must not overwrite a good snapshot with an empty one.
        let headers = headers(&[("content-type", "application/json")]);
        assert!(harvest(StatusCode::OK, &headers, epoch(10), "primary").is_none());
    }

    #[test]
    fn a_429_is_recorded_as_a_closed_window() {
        let headers = headers(&[("retry-after", "1800")]);
        let now = epoch(1_000_000);
        let mut snapshot =
            harvest(StatusCode::TOO_MANY_REQUESTS, &headers, now, "primary").unwrap();
        let refusal = snapshot.exhausted.clone().unwrap();
        assert_eq!(refusal.retry_after, Some(Duration::from_secs(1800)));
        assert_eq!(refusal.until, Some(epoch(1_001_800)));
        assert!(refusal.is_current(now));

        attach_refusal_message(
            &mut snapshot,
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your usage limits. Access resumes at 13:00 UTC."}}"#,
        );
        assert!(snapshot
            .exhausted
            .unwrap()
            .message
            .unwrap()
            .starts_with("You have reached your usage limits"));
    }

    #[test]
    fn the_spend_cap_429_has_no_retry_after_and_still_counts() {
        // Documented: the spend-cap refusal is a 429 with no `retry-after`.
        // It must still register as "closed", just without a countdown.
        let now = epoch(500);
        let snapshot = harvest(
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            now,
            "primary",
        )
        .unwrap();
        let refusal = snapshot.exhausted.unwrap();
        assert_eq!(refusal.retry_after, None);
        assert_eq!(refusal.until, None);
        assert!(refusal.is_current(now), "a fresh refusal still stands");
    }

    #[test]
    fn reset_times_parse_in_every_spelling_seen() {
        let now = epoch(1_756_728_000);
        // RFC 3339 as documented, with and without a fraction and offset.
        assert_eq!(
            parse_instant("2026-09-01T12:00:00Z", now),
            Some(parse_rfc3339("2026-09-01T12:00:00Z").unwrap())
        );
        assert_eq!(
            parse_instant("2026-09-01T12:00:00.532Z", now),
            parse_instant("2026-09-01T12:00:00Z", now)
        );
        assert_eq!(
            parse_instant("2026-09-01T14:00:00+02:00", now),
            parse_instant("2026-09-01T12:00:00Z", now)
        );
        // A bare epoch is an absolute time; a small number is a delay.
        assert_eq!(parse_instant("1756731600", now), Some(epoch(1_756_731_600)));
        assert_eq!(
            parse_instant("60", now),
            Some(now + Duration::from_secs(60))
        );
        assert_eq!(parse_instant("not a time", now), None);
    }

    #[test]
    fn retry_after_takes_an_http_date_too() {
        let now = parse_rfc3339("2026-10-21T07:00:00Z").unwrap();
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", now),
            Some(Duration::from_secs(28 * 60))
        );
    }

    #[test]
    fn a_utilization_reads_as_a_share_either_way() {
        assert_eq!(parse_fraction("42"), Some(0.42));
        assert_eq!(parse_fraction("0.42"), Some(0.42));
        assert_eq!(parse_fraction("42%"), Some(0.42));
        assert_eq!(parse_fraction("140"), Some(1.0));
        assert_eq!(parse_fraction("-1"), None);
        assert_eq!(parse_fraction("many"), None);
    }
}
