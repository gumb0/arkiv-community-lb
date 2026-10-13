//! Hourly buckets, kept in a fixed ring per counter: the stats the
//! dashboard reads, as differences of the monotone counters on the
//! entries and the pool. The rings do no I/O and keep no time; the
//! tick task at the end hands them the hour and the readings.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::pool::Pool;

/// How many hours a ring holds: 31 days. The oldest bucket makes room
/// for the newest; nothing grows and nothing is evicted.
pub const HOURS: usize = 31 * 24;

/// Seconds in an hour. Hours are named by their start, in unix
/// seconds.
pub const HOUR: u64 = 3600;

/// One hour of one counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// The hour's start, unix seconds.
    pub start: u64,
    pub count: u64,
    /// Not a whole hour: the hour in progress, or the hour cut by a
    /// start.
    pub partial: bool,
}

/// The ring of one counter: its last `HOURS` completed hours, and the
/// reading the next bucket is a difference from. A missing hour is
/// absent, not zero: the LB was not running then.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ring {
    /// Oldest at the front, the latest hour at the back.
    buckets: VecDeque<Bucket>,
    /// The reading at the last tick; none before the first tick after
    /// a start, when the counter began at zero. A reading of zero is
    /// not the same: that is a tick that found nothing served. Not
    /// saved: a ring read from the file belongs to a fresh process,
    /// whose counters start at zero again.
    #[serde(skip)]
    last: Option<u64>,
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            buckets: VecDeque::with_capacity(HOURS),
            last: None,
        }
    }
}

impl Ring {
    /// Closes the hour that starts at `start` with what the reading grew by
    /// since the last tick. The first tick after a start closes a
    /// partial hour, since the counter began at zero when the process
    /// did.
    pub fn tick(&mut self, start: u64, reading: u64) {
        self.close(start, reading, false);
    }

    /// Closes the hour in progress at a stop, as a partial bucket, so
    /// what was served since the last tick is not lost with the
    /// process.
    pub fn stop(&mut self, start: u64, reading: u64) {
        self.close(start, reading, true);
    }

    /// A tick or, with `stopping`, a stop. If the newest bucket is the
    /// same hour, which a restart inside the hour leaves behind (the
    /// stop closed it, the first tick after closes it again), the
    /// counts are added to it and it stays partial; otherwise a new
    /// bucket.
    pub fn close(&mut self, start: u64, reading: u64, stopping: bool) {
        let count = reading.saturating_sub(self.last.unwrap_or(0));
        let partial = stopping || self.last.is_none();
        self.last = Some(reading);
        if let Some(newest) = self
            .buckets
            .back_mut()
            .filter(|newest| newest.start == start)
        {
            newest.count += count;
            newest.partial = true;
            return;
        }
        if self.buckets.len() == HOURS {
            self.buckets.pop_front();
        }
        self.buckets.push_back(Bucket {
            start,
            count,
            partial,
        });
    }

    /// The start of the newest completed hour, if any.
    pub fn newest(&self) -> Option<u64> {
        self.buckets.back().map(|bucket| bucket.start)
    }

    /// The hour in progress: what the reading grew by since the last
    /// tick. After a restart inside this hour the newest saved bucket
    /// is this same hour, closed by the stop; its count is added, so
    /// the hour reads as one whole.
    pub fn partial(&self, start: u64, reading: u64) -> Bucket {
        let saved = self
            .buckets
            .back()
            .filter(|newest| newest.start == start)
            .map_or(0, |newest| newest.count);
        Bucket {
            start,
            count: saved + reading.saturating_sub(self.last.unwrap_or(0)),
            partial: true,
        }
    }

    /// The completed buckets from the hour starting at `since` on,
    /// oldest first.
    pub fn since(&self, since: u64) -> impl Iterator<Item = &Bucket> {
        self.buckets
            .iter()
            .filter(move |bucket| bucket.start >= since)
    }
}

/// One provider's rings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderRings {
    pub served: Ring,
    pub transport_failures: Ring,
}

/// The LB's own rings, for the answers it gave itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LbRings {
    pub no_healthy_provider: Ring,
    pub timed_out: Ring,
    pub no_provider_answered: Ring,
}

/// Every ring the LB keeps, behind one lock: written once an hour by
/// the tick, read by the stats route, and saved to a file whole at
/// every tick and at a stop, so a restart keeps the history. The file
/// is a cache of history, never queried: losing it loses old buckets
/// and nothing else.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    /// When this process started, unix seconds: the hour it falls in
    /// is the one a start cut short.
    pub started_at: u64,
    /// By pool id. A provider that left the pool keeps its rings until
    /// its newest hour is older than a ring holds.
    pub providers: HashMap<String, ProviderRings>,
    pub lb: LbRings,
}

impl Stats {
    /// The rings the last process left in `file`, or none; either way
    /// with this process's start. A file that does not parse is
    /// reported and left behind: the next save replaces it.
    pub fn load(file: &Path) -> Self {
        let mut stats = match std::fs::read(file) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(stats) => stats,
                Err(error) => {
                    tracing::warn!(file = %file.display(), %error, "the stats file does not parse: starting without history");
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(file = %file.display(), "no stats file yet: starting without history");
                Self::default()
            }
            Err(error) => {
                tracing::warn!(file = %file.display(), %error, "the stats file cannot be read: starting without history");
                Self::default()
            }
        };
        stats.started_at = now();
        stats
    }

    /// Writes the rings whole, through a temporary file and a rename,
    /// so a crash mid-write leaves the last good file in place.
    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        let temporary = file.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(&temporary, file)
    }

    /// Closes the hour that starts at `start` for every counter, from the
    /// pool as it is now. A provider seen for the first time gets its
    /// rings here, so its first bucket is partial.
    pub fn tick(&mut self, start: u64, pool: &Pool) {
        self.close(start, pool, false);
        // A provider gone for longer than a ring holds has nothing
        // left to show.
        let oldest_kept = start.saturating_sub(HOURS as u64 * HOUR);
        self.providers.retain(|_, rings| {
            rings
                .served
                .newest()
                .is_some_and(|newest| newest >= oldest_kept)
        });
    }

    /// Closes the hour in progress at a stop, for every counter.
    pub fn stop(&mut self, start: u64, pool: &Pool) {
        self.close(start, pool, true);
    }

    fn close(&mut self, start: u64, pool: &Pool, stopping: bool) {
        for provider in pool.snapshot().iter() {
            let rings = self.providers.entry(provider.id.clone()).or_default();
            rings.served.close(
                start,
                provider.served_total.load(Ordering::Relaxed),
                stopping,
            );
            rings.transport_failures.close(
                start,
                provider.transport_failures.load(Ordering::Relaxed),
                stopping,
            );
        }
        let outcomes = &pool.lb_outcomes;
        self.lb.no_healthy_provider.close(
            start,
            outcomes.no_healthy_provider.load(Ordering::Relaxed),
            stopping,
        );
        self.lb
            .timed_out
            .close(start, outcomes.timed_out.load(Ordering::Relaxed), stopping);
        self.lb.no_provider_answered.close(
            start,
            outcomes.no_provider_answered.load(Ordering::Relaxed),
            stopping,
        );
    }
}

/// Unix seconds now.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// The tick task: at every boundary of `period` on the wall clock,
/// closes the bucket that just ended and saves the rings; at a stop
/// it closes the hour in progress and saves once more. The period is
/// an hour, except in tests, and whole seconds either way.
pub async fn run(
    stats: Arc<Mutex<Stats>>,
    pool: Arc<Pool>,
    period: Duration,
    file: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) {
    let period_secs = period.as_secs();
    loop {
        let now = now();
        let next = (now / period_secs + 1) * period_secs;
        let wait = tokio::time::sleep(Duration::from_secs(next - now));
        tokio::select! {
            _ = wait => {}
            _ = shutdown.changed() => {
                let start = now / period_secs * period_secs;
                stats
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .stop(start, &pool);
                save(&stats, &file);
                return;
            }
        }
        let start = next - period_secs;
        stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tick(start, &pool);
        tracing::debug!(start, "stats: the hour closed");
        save(&stats, &file);
    }
}

/// Saves a copy of the rings, taken under the lock and written outside
/// it; a failure is logged, since the next hour tries again.
fn save(stats: &Mutex<Stats>, file: &Path) {
    let snapshot = stats
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Err(error) = snapshot.save(file) {
        tracing::warn!(file = %file.display(), %error, "the stats file could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real-looking time (2025-10-09), rounded down to the start of
    // its hour, the way the tick task's hours are.
    const T0: u64 = 1_760_000_000 - 1_760_000_000 % HOUR;

    fn bucket(start: u64, count: u64, partial: bool) -> Bucket {
        Bucket {
            start,
            count,
            partial,
        }
    }

    fn buckets(ring: &Ring, since: u64) -> Vec<Bucket> {
        ring.since(since).copied().collect()
    }

    #[test]
    fn a_bucket_is_what_the_reading_grew_by_since_the_last_tick() {
        let mut ring = Ring::default();
        ring.tick(T0, 10);
        ring.tick(T0 + HOUR, 25);
        ring.tick(T0 + 2 * HOUR, 25);
        assert_eq!(
            buckets(&ring, 0),
            vec![
                bucket(T0, 10, true),
                bucket(T0 + HOUR, 15, false),
                bucket(T0 + 2 * HOUR, 0, false),
            ]
        );
    }

    #[test]
    fn a_tick_that_found_nothing_is_a_whole_hour_of_nothing() {
        let mut ring = Ring::default();
        ring.tick(T0, 0);
        ring.tick(T0 + HOUR, 0);
        assert_eq!(
            buckets(&ring, 0),
            vec![bucket(T0, 0, true), bucket(T0 + HOUR, 0, false)]
        );
    }

    #[test]
    fn the_hour_in_progress_is_read_against_the_last_tick() {
        let mut ring = Ring::default();
        assert_eq!(
            ring.partial(T0, 4),
            bucket(T0, 4, true),
            "before any tick, everything since the start"
        );
        ring.tick(T0, 10);
        assert_eq!(ring.partial(T0 + HOUR, 13), bucket(T0 + HOUR, 3, true));
    }

    #[test]
    fn since_cuts_at_the_hour_and_keeps_the_order() {
        let mut ring = Ring::default();
        for i in 0..5 {
            ring.tick(T0 + i * HOUR, i);
        }
        let starts: Vec<u64> = ring.since(T0 + 2 * HOUR).map(|b| b.start).collect();
        assert_eq!(starts, vec![T0 + 2 * HOUR, T0 + 3 * HOUR, T0 + 4 * HOUR]);
        assert_eq!(ring.since(T0 + 10 * HOUR).count(), 0);
    }

    #[test]
    fn the_ring_keeps_the_last_hours_only() {
        let mut ring = Ring::default();
        let ticks = HOURS as u64 + 5;
        for i in 0..ticks {
            ring.tick(T0 + i * HOUR, i);
        }
        let buckets: Vec<&Bucket> = ring.since(0).collect();
        assert_eq!(buckets.len(), HOURS);
        // Five hours fell off the oldest end; the rest are in order and
        // each is one hour's growth.
        assert_eq!(buckets[0].start, T0 + 5 * HOUR);
        assert_eq!(buckets[HOURS - 1].start, T0 + (ticks - 1) * HOUR);
        assert_eq!(ring.newest(), Some(T0 + (ticks - 1) * HOUR));
        assert!(buckets.iter().all(|b| b.count == 1 && !b.partial));
    }

    #[test]
    fn the_file_keeps_the_buckets_and_forgets_the_reading() {
        let mut stats = Stats::default();
        let ring = &mut stats.providers.entry("p0".into()).or_default().served;
        ring.tick(T0, 10);
        ring.tick(T0 + HOUR, 25);
        let file = std::env::temp_dir().join(format!("stats-test-{}.json", std::process::id()));
        stats.save(&file).expect("saved");
        let loaded = Stats::load(&file);
        std::fs::remove_file(&file).expect("removed");
        let ring = &loaded.providers["p0"].served;
        assert_eq!(
            buckets(ring, 0),
            vec![bucket(T0, 10, true), bucket(T0 + HOUR, 15, false)]
        );
        assert_eq!(ring.last, None, "a fresh process's counters start at zero");
        assert!(loaded.started_at > 0);
        assert!(
            Stats::load(&file).providers.is_empty(),
            "no file is no history"
        );
    }

    #[test]
    fn a_stop_closes_the_hour_in_progress_and_the_next_process_adds_to_it() {
        let mut ring = Ring::default();
        ring.tick(T0, 10);
        // Stopped twenty minutes into the next hour, with five served.
        ring.stop(T0 + HOUR, 15);
        assert_eq!(
            buckets(&ring, 0),
            vec![bucket(T0, 10, true), bucket(T0 + HOUR, 5, true)]
        );

        // The file read by a fresh process: counters from zero again.
        let mut restored = Ring {
            buckets: ring.buckets.clone(),
            last: None,
        };
        assert_eq!(
            restored.partial(T0 + HOUR, 3),
            bucket(T0 + HOUR, 8, true),
            "the hour in progress counts what the stop saved"
        );
        restored.tick(T0 + HOUR, 7);
        assert_eq!(
            buckets(&restored, 0),
            vec![bucket(T0, 10, true), bucket(T0 + HOUR, 12, true)],
            "one bucket for the hour, both processes' counts, still partial"
        );
        restored.tick(T0 + 2 * HOUR, 9);
        assert_eq!(
            buckets(&restored, T0 + 2 * HOUR),
            vec![bucket(T0 + 2 * HOUR, 2, false)]
        );
    }

    #[test]
    fn a_reading_below_the_last_one_reads_as_nothing() {
        // Impossible on a monotone counter inside one process; a ring
        // restored from a file and ticked by a fresh process is the
        // case, and it must not panic.
        let mut ring = Ring::default();
        ring.tick(T0, 50);
        ring.tick(T0 + HOUR, 3);
        assert_eq!(ring.since(T0 + HOUR).next().map(|b| b.count), Some(0));
    }
}
