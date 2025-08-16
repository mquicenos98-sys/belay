//! What one [`Scheduler::tick`](crate::Scheduler::tick) did, for logs,
//! metrics and tests.

use std::fmt;

use belay_common::Timestamp;
use belay_proto::event::SkipReason as EventSkipReason;
use belay_proto::{JobId, ScheduleId};

/// Everything a tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct TickReport {
    /// Fires that dispatched a job (or found the job of an earlier attempt
    /// at the same fire), in the order they happened.
    pub fired: Vec<Fired>,
    /// Fire times that did not dispatch anything, and why.
    pub skipped: Vec<Skipped>,
    /// Things that went wrong. A tick never fails as a whole; failed fires
    /// are retried or dropped as each error's [`kind`](TickError::kind)
    /// says.
    pub errors: Vec<TickError>,
}

impl TickReport {
    /// Whether the tick did nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fired.is_empty() && self.skipped.is_empty() && self.errors.is_empty()
    }

    /// Fires of schedule `id`.
    #[must_use]
    pub fn fired_for(&self, id: &str) -> Vec<&Fired> {
        self.fired.iter().filter(|f| f.schedule == id).collect()
    }

    /// Skips of schedule `id`.
    #[must_use]
    pub fn skipped_for(&self, id: &str) -> Vec<&Skipped> {
        self.skipped.iter().filter(|s| s.schedule == id).collect()
    }
}

impl fmt::Display for TickReport {
    /// A one-line summary: `3 fired, 1 skipped (overlap), 0 errors`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} fired, {} skipped",
            self.fired.len(),
            self.skipped.len()
        )?;
        if !self.skipped.is_empty() {
            let mut reasons: Vec<&str> = self.skipped.iter().map(|s| s.reason.as_str()).collect();
            reasons.sort_unstable();
            reasons.dedup();
            write!(f, " ({})", reasons.join(", "))?;
        }
        write!(
            f,
            ", {}",
            belay_common::text::plural(self.errors.len() as u64, "error")
        )
    }
}

/// One fire that produced a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fired {
    /// The schedule.
    pub schedule: ScheduleId,
    /// The nominal fire time (before jitter).
    pub fire_time: Timestamp,
    /// The job representing the fire.
    pub job: JobId,
    /// When the job may run, if jitter pushed it past `fire_time`.
    pub run_at: Option<Timestamp>,
    /// Whether the dispatcher returned the job of an earlier attempt at the
    /// same fire instead of a new one: the fire had been dispatched before
    /// a crash that lost the bookkeeping.
    pub duplicate: bool,
    /// Whether this fire caught up on a misfire rather than being on time.
    pub catch_up: bool,
}

/// Fire times that were passed over.
///
/// One entry can stand for a whole run of fire times (a misfire covering
/// hours of downtime, a weekend closed by the calendar); `count` says how
/// many.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The schedule.
    pub schedule: ScheduleId,
    /// Why.
    pub reason: SkipReason,
    /// The first fire time skipped.
    pub fire_time: Timestamp,
    /// The last fire time skipped (equal to `fire_time` for a single one).
    pub last_fire_time: Timestamp,
    /// How many fire times this entry covers. Counting stops at 10,000,
    /// so a longer run reads 10,000; `fire_time` and `last_fire_time` are
    /// exact either way.
    pub count: u32,
}

/// Why fire times were skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// The job of the previous fire had not finished and the schedule's
    /// overlap policy is `skip`.
    Overlap,
    /// The schedule's calendar does not allow these times. The entry is
    /// reported when the scheduler moves past a closed stretch, which may
    /// be before the stretch begins.
    Calendar,
    /// The times were missed (belay was down, or ticks stalled for longer
    /// than the misfire threshold) and the misfire policy dropped them.
    Misfire,
    /// The schedule was paused when the times came due.
    Paused,
}

impl SkipReason {
    /// The lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SkipReason::Overlap => "overlap",
            SkipReason::Calendar => "calendar",
            SkipReason::Misfire => "misfire",
            SkipReason::Paused => "paused",
        }
    }

    /// The reason as carried by a `schedule.skipped` event, for the
    /// reasons that have one. Misfires are reported as `schedule.misfired`
    /// instead, and paused schedules emit nothing per fire.
    #[must_use]
    pub fn event_reason(self) -> Option<EventSkipReason> {
        match self {
            SkipReason::Overlap => Some(EventSkipReason::Overlap),
            SkipReason::Calendar => Some(EventSkipReason::Calendar),
            SkipReason::Misfire | SkipReason::Paused => None,
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// A problem during a tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickError {
    /// The schedule concerned, if the problem was specific to one.
    pub schedule: Option<ScheduleId>,
    /// The fire concerned, if any.
    pub fire_time: Option<Timestamp>,
    /// What kind of problem, which also says what happens next.
    pub kind: TickErrorKind,
    /// The underlying error message.
    pub message: String,
}

impl fmt::Display for TickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.describe())?;
        if let Some(id) = &self.schedule {
            write!(f, " for schedule {id}")?;
        }
        if let Some(t) = self.fire_time {
            write!(f, " at {t}")?;
        }
        write!(f, ": {}", self.message)
    }
}
