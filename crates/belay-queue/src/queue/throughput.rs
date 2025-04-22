//! Trailing-window activity counters for [`QueueInfo::throughput`].
//!
//! The window is split into a fixed number of slots, each covering
//! `window / SLOTS`; a slot is reset when time reaches it again. Counts
//! are therefore exact to one slot width at the trailing edge, which is
//! plenty for a dashboard number, and recording is O(1) with no
//! allocation.
//!
//! [`QueueInfo::throughput`]: belay_proto::QueueInfo::throughput

use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::Throughput;

const SLOTS: usize = 60;

/// What is being counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Enqueued,
    Succeeded,
    /// An attempt failed (including an expired lease), retried or not.
    Failed,
    /// A job died.
    Dead,
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    /// Which slot of time this holds, as `millis / width`.
    index: i64,
    counts: [u64; 4],
}

#[derive(Clone, Debug)]
pub(crate) struct ThroughputWindow {
    window: Duration,
    width_ms: i64,
    slots: [Slot; SLOTS],
}

impl ThroughputWindow {
    pub(crate) fn new(window: Duration) -> ThroughputWindow {
        let window_ms = i64::try_from(window.as_millis()).unwrap_or(i64::MAX);
        let width_ms = (window_ms / SLOTS as i64).max(1);
        ThroughputWindow {
            window,
            width_ms,
            // Slot index i64::MIN never matches a real time, so every slot
            // starts out stale.
            slots: [Slot {
                index: i64::MIN,
                counts: [0; 4],
            }; SLOTS],
        }
    }

    fn slot_index(&self, now: Timestamp) -> i64 {
        now.as_millis().div_euclid(self.width_ms)
    }

    pub(crate) fn record(&mut self, now: Timestamp, metric: Metric) {
        let index = self.slot_index(now);
        let slot = &mut self.slots[index.rem_euclid(SLOTS as i64) as usize];
        if slot.index != index {
            *slot = Slot {
                index,
                counts: [0; 4],
            };
        }
        slot.counts[metric as usize] += 1;
    }

    pub(crate) fn snapshot(&self, now: Timestamp) -> Throughput {
        let current = self.slot_index(now);
        let oldest = current - SLOTS as i64 + 1;
        let mut totals = [0u64; 4];
        for slot in &self.slots {
            if (oldest..=current).contains(&slot.index) {
                for (total, count) in totals.iter_mut().zip(slot.counts) {
                    *total += count;
                }
            }
        }
        Throughput {
            window: self.window,
            enqueued: totals[Metric::Enqueued as usize],
            succeeded: totals[Metric::Succeeded as usize],
            failed: totals[Metric::Failed as usize],
            dead: totals[Metric::Dead as usize],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_secs(1_700_000_000 + secs)
    }

    #[test]
    fn counts_within_the_window_and_forgets_older_activity() {
        let mut w = ThroughputWindow::new(Duration::from_secs(60));
        w.record(at(0), Metric::Enqueued);
        w.record(at(0), Metric::Enqueued);
        w.record(at(30), Metric::Succeeded);
        w.record(at(45), Metric::Failed);
        w.record(at(45), Metric::Dead);
        let snap = w.snapshot(at(59));
        assert_eq!(
            (snap.enqueued, snap.succeeded, snap.failed, snap.dead),
            (2, 1, 1, 1)
        );
        assert_eq!(snap.window, Duration::from_secs(60));
        let later = w.snapshot(at(60));
        assert_eq!((later.enqueued, later.succeeded), (0, 1));
        assert_eq!(
            w.snapshot(at(200)),
            Throughput {
                window: Duration::from_secs(60),
                ..Throughput::default()
            }
        );
    }

    #[test]
    fn slots_are_reused_when_time_comes_around() {
        let mut w = ThroughputWindow::new(Duration::from_secs(60));
        w.record(at(5), Metric::Enqueued);
        // Same slot position one full window later: the old count is gone.
        w.record(at(65), Metric::Enqueued);
        assert_eq!(w.snapshot(at(65)).enqueued, 1);
    }

    #[test]
    fn tiny_windows_still_have_one_millisecond_slots() {
        let mut w = ThroughputWindow::new(Duration::from_millis(10));
        let t = Timestamp::from_millis(1_000);
        w.record(t, Metric::Succeeded);
        assert_eq!(w.snapshot(t).succeeded, 1);
        assert_eq!(w.snapshot(Timestamp::from_millis(1_060)).succeeded, 0);
    }
}
