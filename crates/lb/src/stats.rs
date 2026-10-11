//! Hourly buckets, kept in a fixed ring per counter: the stats the
//! dashboard reads, as differences of the monotone counters on the
//! entries and the pool. Nothing here does I/O or keeps time; the tick
//! task hands in the hour and the readings.

use std::collections::VecDeque;

/// How many hours a ring holds: 31 days. The oldest bucket makes room
/// for the newest; nothing grows and nothing is evicted.
pub const HOURS: usize = 31 * 24;

/// Seconds in an hour; bucket stamps are unix seconds at the hour's
/// start.
pub const HOUR: u64 = 3600;

/// One hour of one counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// The hour's start, unix seconds.
    pub hour: u64,
    pub count: u64,
    /// Not a whole hour: the hour in progress, or the hour cut by a
    /// start.
    pub partial: bool,
}

/// The ring of one counter: its last `HOURS` completed hours, and the
/// reading the next bucket is a difference from. A missing hour is
/// absent, not zero: the LB was not running then.
#[derive(Clone, Debug)]
pub struct Ring {
    /// Oldest at the front, the latest hour at the back.
    buckets: VecDeque<Bucket>,
    /// The reading at the last tick; none before the first tick after
    /// a start, when the counter began at zero. A reading of zero is
    /// not the same: that is a tick that found nothing served.
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
    /// Closes the hour that ends at `hour`: the bucket for the hour
    /// before it, counting what the reading grew by since the last
    /// tick. The first tick after a start closes a partial hour, since
    /// the counter began at zero when the process did.
    pub fn tick(&mut self, hour: u64, reading: u64) {
        if self.buckets.len() == HOURS {
            self.buckets.pop_front();
        }
        self.buckets.push_back(Bucket {
            hour: hour - HOUR,
            count: reading.saturating_sub(self.last.unwrap_or(0)),
            partial: self.last.is_none(),
        });
        self.last = Some(reading);
    }

    /// The hour in progress, from the last tick to the reading given.
    pub fn partial(&self, hour: u64, reading: u64) -> Bucket {
        Bucket {
            hour,
            count: reading.saturating_sub(self.last.unwrap_or(0)),
            partial: true,
        }
    }

    /// The completed buckets from `since` on, oldest first.
    pub fn since(&self, since: u64) -> impl Iterator<Item = &Bucket> {
        self.buckets
            .iter()
            .filter(move |bucket| bucket.hour >= since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real-looking stamp (2025-10-09), rounded down to the start of
    // its hour, the way the tick task's stamps are.
    const T0: u64 = 1_760_000_000 - 1_760_000_000 % HOUR;

    fn bucket(hour: u64, count: u64, partial: bool) -> Bucket {
        Bucket {
            hour,
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
        ring.tick(T0 + HOUR, 10);
        ring.tick(T0 + 2 * HOUR, 25);
        ring.tick(T0 + 3 * HOUR, 25);
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
        ring.tick(T0 + HOUR, 0);
        ring.tick(T0 + 2 * HOUR, 0);
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
        ring.tick(T0 + HOUR, 10);
        assert_eq!(ring.partial(T0 + HOUR, 13), bucket(T0 + HOUR, 3, true));
    }

    #[test]
    fn since_cuts_at_the_hour_and_keeps_the_order() {
        let mut ring = Ring::default();
        for i in 1..=5 {
            ring.tick(T0 + i * HOUR, i);
        }
        let hours: Vec<u64> = ring.since(T0 + 2 * HOUR).map(|b| b.hour).collect();
        assert_eq!(hours, vec![T0 + 2 * HOUR, T0 + 3 * HOUR, T0 + 4 * HOUR]);
        assert_eq!(ring.since(T0 + 10 * HOUR).count(), 0);
    }

    #[test]
    fn the_ring_keeps_the_last_hours_only() {
        let mut ring = Ring::default();
        let ticks = HOURS as u64 + 5;
        for i in 1..=ticks {
            ring.tick(T0 + i * HOUR, i);
        }
        let buckets: Vec<&Bucket> = ring.since(0).collect();
        assert_eq!(buckets.len(), HOURS);
        // Five hours fell off the oldest end; the rest are in order and
        // each is one hour's growth.
        assert_eq!(buckets[0].hour, T0 + 5 * HOUR);
        assert_eq!(buckets[HOURS - 1].hour, T0 + (ticks - 1) * HOUR);
        assert!(buckets.iter().all(|b| b.count == 1 && !b.partial));
    }

    #[test]
    fn a_reading_below_the_last_one_reads_as_nothing() {
        // Impossible on a monotone counter inside one process; a ring
        // restored from a file and ticked by a fresh process is the
        // case, and it must not panic.
        let mut ring = Ring::default();
        ring.tick(T0 + HOUR, 50);
        ring.tick(T0 + 2 * HOUR, 3);
        assert_eq!(ring.since(T0 + HOUR).next().map(|b| b.count), Some(0));
    }
}
