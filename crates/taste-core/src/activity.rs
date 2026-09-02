//! Per-environment activity, as a shape rather than a number.
//!
//! ENVIRONMENTS.md → "Watching an environment": the environment panel at
//! the foot of the file tree shows one row per environment, and a row that
//! says only "running" cannot answer the question a person actually asks of
//! a fleet — *is anything happening in there?* A container can be up and
//! idle for an hour; another can be up and hammering. The state dot cannot
//! tell them apart, because that is not what a state is.
//!
//! So: a count of env-tagged activity events per time bucket, over a fixed
//! recent window. **Counts, never payloads.** The bus already refuses to
//! carry terminal bytes to every subscriber
//! ([`crate::shells`]) and this refuses to hoard them: what a sparkline
//! needs is how much happened and when, and one `u16` per five seconds
//! answers that in 120 bytes per environment.
//!
//! Everything above [`Activity`] is pure integer arithmetic with no clock
//! in it: [`Series`] is addressed by bucket index, and the one place an
//! [`Instant`] becomes an index is [`Activity::bucket`]. That is what makes
//! rotation, saturation and the empty case testable without sleeping.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::environment::EnvironmentId;
use crate::event::Event;

/// How much time one bucket covers.
///
/// Five seconds is the coarsest bucket a 1 Hz redraw can use without the
/// line visibly stepping, and fine enough that a `cargo build`'s output
/// reads as a burst rather than a plateau. Below about two seconds the
/// series turns into noise at the widths this draws at — a sparkline in a
/// file-tree flank is under 60 px, so a bucket is under one pixel either
/// way, and the pixel should average something.
pub const BUCKET: Duration = Duration::from_secs(5);

/// How many buckets are kept. 60 × [`BUCKET`] is the last five minutes,
/// which is the horizon over which "is anything happening in there" has an
/// answer: a turn is seconds to minutes, a container build is minutes. An
/// hour of history would be a chart, and a chart does not belong in a
/// sidebar row.
pub const BUCKETS: usize = 60;

/// The whole window one series covers.
pub const WINDOW: Duration = Duration::from_secs(BUCKET.as_secs() * BUCKETS as u64);

/// One bucket's count. Saturating rather than wrapping, and 16 bits
/// because the drawn height is a ratio: a bucket that took 65_535 events
/// in five seconds and one that took 200_000 draw at the same full height,
/// so the extra bytes would buy a distinction nothing can see.
pub type Count = u16;

/// The floor under a sparkline's vertical scale.
///
/// Without it, every series normalises to its own peak and a single stray
/// event draws as a full-height spike — an idle environment that logged
/// one line would look busier than a build, which is the one thing this
/// must never do.
///
/// It was eight, and eight was too low to do the job it was there for. A
/// container emitting four to eleven events per bucket — a build, ticking
/// over — cleared it, became its own reference, and drew at *full height*:
/// a row of maximum-amplitude spikes for an environment that was barely
/// doing anything. Quiet did not read as quiet, because past the floor
/// nothing about the drawing is absolute.
///
/// Twenty-four is where an agent mid-turn sits, so that is where the
/// height stops being relative: below it a row draws at a fraction of the
/// span and *looks* like a fraction, and rows can be compared to each
/// other by height rather than only by shape. Above it a busy series is
/// still normalised against its own peak, which is the only way a burst
/// inside a busy window stays visible.
pub const MIN_SCALE: Count = 24;

/// A fixed-size ring of bucket counts for one environment.
///
/// Addressed by *bucket index* — a monotonic count of [`BUCKET`]s since
/// some origin — so this type holds no clock and no floats. `newest` is
/// the index the head slot covers; everything behind it walks backwards
/// through the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    counts: [Count; BUCKETS],
    /// The bucket index `counts[head]` covers.
    newest: u64,
    head: usize,
}

impl Series {
    /// An empty series whose newest bucket is `bucket`.
    pub fn new(bucket: u64) -> Self {
        Self {
            counts: [0; BUCKETS],
            newest: bucket,
            head: 0,
        }
    }

    /// Count one event in `bucket`.
    ///
    /// O(1) amortised: the only loop is the rotation, which is bounded by
    /// [`BUCKETS`] however long the gap was — an environment idle for a
    /// week costs the same 60 writes as one idle for a minute.
    pub fn record(&mut self, bucket: u64) {
        self.advance(bucket);
        // An event timestamped *before* the newest bucket (a caller with a
        // stale `now`) lands on the head rather than being dropped: it did
        // happen, and refusing it would lose activity to a rounding edge.
        let slot = self.slot(bucket).unwrap_or(self.head);
        self.counts[slot] = self.counts[slot].saturating_add(1);
    }

    /// Roll the window forward to `bucket`, zeroing what it moved past.
    /// Going backwards does nothing: time is the caller's, and a clock
    /// that stepped back must not erase history.
    fn advance(&mut self, bucket: u64) {
        if bucket <= self.newest {
            return;
        }
        let steps = (bucket - self.newest).min(BUCKETS as u64) as usize;
        for _ in 0..steps {
            self.head = (self.head + 1) % BUCKETS;
            self.counts[self.head] = 0;
        }
        self.newest = bucket;
    }

    /// Where `bucket` lives in the ring, if it is still in the window.
    fn slot(&self, bucket: u64) -> Option<usize> {
        let back = self.newest.checked_sub(bucket)?;
        if back >= BUCKETS as u64 {
            return None;
        }
        Some((self.head + BUCKETS - back as usize) % BUCKETS)
    }

    /// The window as of `bucket`, oldest first — the order a sparkline
    /// draws left to right.
    ///
    /// A read, not a rotation: an idle environment's line decays as `now`
    /// advances without anything having to tick the series. Buckets that
    /// have fallen out of the window, and ones the series has not reached
    /// yet, read as zero.
    pub fn samples(&self, bucket: u64) -> [Count; BUCKETS] {
        let mut out = [0; BUCKETS];
        for (age, slot) in out.iter_mut().rev().enumerate() {
            let Some(index) = bucket.checked_sub(age as u64) else {
                break; // before the origin: nothing older to show
            };
            if let Some(from) = self.slot(index) {
                *slot = self.counts[from];
            }
        }
        out
    }
}

/// The vertical scale a series draws against: its own peak, floored at
/// [`MIN_SCALE`] so quiet does not draw as busy.
pub fn scale(samples: &[Count]) -> Count {
    samples.iter().copied().max().unwrap_or(0).max(MIN_SCALE)
}

/// Whether a series has anything in it at all. An environment that has
/// done nothing in five minutes draws no line rather than a flat one at
/// the baseline, which would read as a measurement of zero instead of an
/// absence of activity.
pub fn is_silent(samples: &[Count]) -> bool {
    samples.iter().all(|count| *count == 0)
}

#[derive(Default)]
struct Inner {
    origin: Option<Instant>,
    series: BTreeMap<EnvironmentId, Series>,
}

/// Every environment's recent activity.
///
/// A handle, like [`crate::ShellRoster`] next to it on the
/// [`crate::Workspace`]: cheap to clone, safe to hold anywhere, no GTK and
/// no blocking IO. Written from wherever activity happens — the roster's
/// output path on a tokio thread, the event pump and the chat panes on the
/// main thread — and read by the render, which is why it is a mutex and
/// not a `RefCell`.
///
/// The lock is held for a map lookup and an increment and nothing else. It
/// is never held across an await, a render, or another lock.
#[derive(Clone, Default)]
pub struct Activity {
    inner: Arc<Mutex<Inner>>,
}

impl Activity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one thing happening in `env`, now.
    pub fn record(&self, env: &EnvironmentId) {
        self.record_at(env, Instant::now());
    }

    /// Count one thing happening in `env` at `at` — the seam the tests use.
    pub fn record_at(&self, env: &EnvironmentId, at: Instant) {
        let mut inner = self.inner.lock().unwrap();
        let bucket = Self::bucket(&mut inner, at);
        match inner.series.get_mut(env) {
            Some(series) => series.record(bucket),
            None => {
                let mut series = Series::new(bucket);
                series.record(bucket);
                inner.series.insert(env.clone(), series);
            }
        }
    }

    /// Count the activity an event is evidence of, if it is evidence of
    /// any ([`Event::activity_env`]).
    pub fn record_event(&self, event: &Event) {
        if let Some(env) = event.activity_env() {
            self.record(env);
        }
    }

    /// `env`'s window, oldest first. An environment nothing has been
    /// recorded for reads as silent rather than as missing: a row that has
    /// just appeared has no history, which is a true statement about it.
    pub fn samples(&self, env: &EnvironmentId) -> [Count; BUCKETS] {
        self.samples_at(env, Instant::now())
    }

    pub fn samples_at(&self, env: &EnvironmentId, at: Instant) -> [Count; BUCKETS] {
        let mut inner = self.inner.lock().unwrap();
        let bucket = Self::bucket(&mut inner, at);
        inner
            .series
            .get(env)
            .map(|series| series.samples(bucket))
            .unwrap_or([0; BUCKETS])
    }

    /// Forget every environment not in `keep` — called with the fleet, so
    /// a destroyed environment's ring goes with it. Bounded memory needs
    /// this: nothing else ever removes a key, and an environment that has
    /// been nuked is not coming back to claim its history.
    pub fn retain(&self, keep: &[EnvironmentId]) {
        let mut inner = self.inner.lock().unwrap();
        inner.series.retain(|env, _| keep.contains(env));
    }

    /// How many environments have history. Diagnostics and tests; a caller
    /// that wants to render something should ask for [`Activity::samples`].
    pub fn tracked(&self) -> usize {
        self.inner.lock().unwrap().series.len()
    }

    /// `at` as a bucket index, anchored on the first instant this handle
    /// was ever asked about.
    ///
    /// The origin is lazy rather than set at construction because a handle
    /// is built when the workspace opens and first used whenever something
    /// first happens; anchoring on construction would only make the first
    /// index larger. An instant *before* the origin (possible only if two
    /// callers race the very first record) clamps to bucket zero.
    fn bucket(inner: &mut Inner, at: Instant) -> u64 {
        let origin = *inner.origin.get_or_insert(at);
        at.saturating_duration_since(origin).as_secs() / BUCKET.as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    /// The empty case, which is most rows most of the time: no history is
    /// zeros, and zeros are silent rather than a flat measured line.
    #[test]
    fn a_series_with_nothing_in_it_is_silent_and_still_scales() {
        let series = Series::new(0);
        let samples = series.samples(0);
        assert_eq!(samples.len(), BUCKETS);
        assert!(is_silent(&samples));
        assert_eq!(
            scale(&samples),
            MIN_SCALE,
            "an empty series must not scale to zero, or the draw divides by it"
        );
        let sampler = Activity::new();
        assert!(is_silent(&sampler.samples(&env("calm-1"))));
        assert_eq!(sampler.tracked(), 0, "asking is not recording");
    }

    /// The newest bucket is the last column, and the columns behind it are
    /// the buckets behind it. This is the whole contract the sparkline
    /// draws against, so it is asserted positionally.
    #[test]
    fn samples_run_oldest_first_with_the_newest_bucket_last() {
        let mut series = Series::new(100);
        series.record(100);
        series.record(100);
        series.record(99);
        let samples = series.samples(100);
        assert_eq!(samples[BUCKETS - 1], 2, "the newest bucket is the last");
        assert_eq!(
            samples[BUCKETS - 2],
            1,
            "one bucket back is one column back"
        );
        assert!(samples[..BUCKETS - 2].iter().all(|c| *c == 0));
    }

    /// Rotation: the window moves, what it moved past is gone, and the gap
    /// costs the same whether it was one bucket or a week.
    #[test]
    fn the_window_rotates_and_forgets_exactly_what_left_it() {
        let mut series = Series::new(0);
        for bucket in 0..BUCKETS as u64 {
            series.record(bucket);
        }
        let full = series.samples(BUCKETS as u64 - 1);
        assert!(full.iter().all(|c| *c == 1), "one event in every bucket");

        // One bucket on: the empty new bucket arrives on the right, every
        // column shifts one left, and the oldest is pushed off the end.
        let next = series.samples(BUCKETS as u64);
        assert_eq!(next[BUCKETS - 1], 0, "the bucket now current is empty");
        assert_eq!(next[BUCKETS - 2], 1, "the previous newest slid one left");
        assert_eq!(
            next.iter().map(|c| u32::from(*c)).sum::<u32>(),
            BUCKETS as u32 - 1,
            "one bucket's worth of history fell off the left edge"
        );

        // A long silence empties it completely, and the rotation that does
        // so is bounded — this is the "idle for a week" case.
        series.record(1_000_000);
        let after = series.samples(1_000_000);
        assert_eq!(after[BUCKETS - 1], 1, "the event that woke it");
        assert!(
            after[..BUCKETS - 1].iter().all(|c| *c == 0),
            "nothing survives a gap wider than the window"
        );
    }

    /// A bucket cannot overflow into a wrap: 65_535 and 200_000 events in
    /// five seconds are the same picture, and the wrong answer here is a
    /// full bar becoming an empty one.
    #[test]
    fn a_bucket_saturates_instead_of_wrapping() {
        let mut series = Series::new(7);
        for _ in 0..Count::MAX as u32 + 500 {
            series.record(7);
        }
        assert_eq!(series.samples(7)[BUCKETS - 1], Count::MAX);
    }

    /// Time going backwards is a clock's problem, not a reason to lose
    /// history: an old timestamp still counts, and it never rewinds the
    /// window.
    #[test]
    fn a_stale_timestamp_counts_without_rewinding_the_window() {
        let mut series = Series::new(100);
        series.record(100);
        series.record(90); // still in the window: counted where it belongs
        series.record(1); // older than the window: counted as now, not dropped
        let samples = series.samples(100);
        assert_eq!(
            samples[BUCKETS - 1],
            2,
            "the current bucket, plus the stray"
        );
        assert_eq!(
            samples[BUCKETS - 11],
            1,
            "ten buckets back is ten columns back"
        );
        assert_eq!(
            samples.iter().map(|c| u32::from(*c)).sum::<u32>(),
            3,
            "three events recorded, three events counted"
        );
    }

    /// The scale is the peak, floored — so a busy series draws against
    /// itself and a nearly-silent one does not draw as full-height.
    #[test]
    fn the_scale_is_the_peak_but_never_below_the_floor() {
        assert_eq!(scale(&[0, 1, 2]), MIN_SCALE, "one event is not a spike");
        assert_eq!(scale(&[0, MIN_SCALE - 1, 0]), MIN_SCALE);
        assert_eq!(scale(&[3, 40, 12]), 40, "a real peak sets its own scale");
        assert_eq!(scale(&[]), MIN_SCALE, "no data still divides safely");
        assert!(!is_silent(&[0, 0, 1]));
    }

    /// The handle: environments do not share a ring, an unknown one reads
    /// silent, and `retain` is what keeps the map from growing forever.
    #[test]
    fn the_sampler_keeps_one_ring_per_environment_and_forgets_on_request() {
        let sampler = Activity::new();
        let now = Instant::now();
        for _ in 0..3 {
            sampler.record_at(&env("calm-1"), now);
        }
        sampler.record_at(&env("spry-2"), now);

        let calm = sampler.samples_at(&env("calm-1"), now);
        let spry = sampler.samples_at(&env("spry-2"), now);
        assert_eq!(calm[BUCKETS - 1], 3);
        assert_eq!(spry[BUCKETS - 1], 1, "a row carries its own activity");
        assert!(is_silent(&sampler.samples_at(&env("wry-4"), now)));
        assert_eq!(
            sampler.tracked(),
            2,
            "and asking about wry-4 did not add it"
        );

        sampler.retain(&[env("spry-2")]);
        assert_eq!(sampler.tracked(), 1);
        assert!(
            is_silent(&sampler.samples_at(&env("calm-1"), now)),
            "a destroyed environment's history goes with it"
        );
    }

    /// The clock seam: real durations become bucket indices, and five
    /// minutes of silence is exactly the window.
    #[test]
    fn instants_land_in_the_bucket_their_offset_names() {
        let sampler = Activity::new();
        let start = Instant::now();
        sampler.record_at(&env("calm-1"), start);
        sampler.record_at(&env("calm-1"), start + BUCKET * 2);
        sampler.record_at(&env("calm-1"), start + BUCKET * 2 + Duration::from_secs(1));

        let samples = sampler.samples_at(&env("calm-1"), start + BUCKET * 2);
        assert_eq!(samples[BUCKETS - 1], 2, "same bucket, one second apart");
        assert_eq!(
            samples[BUCKETS - 3],
            1,
            "two buckets earlier, two columns left"
        );

        // A full window later, the first events have aged out entirely.
        assert!(
            is_silent(&sampler.samples_at(&env("calm-1"), start + WINDOW + BUCKET * 3)),
            "nothing older than WINDOW is still on screen"
        );
    }
}
