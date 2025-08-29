//! Cron schedules and the calendars that restrict them.
//!
//! [`Scheduler`] owns every schedule and calendar, keeps them in memory
//! (they are few and small) and persists each change as one atomic batch.
//! Firing happens only in [`Scheduler::tick`], which the caller drives:
//! belay-core runs it from a loop that sleeps until
//! [`Scheduler::next_wakeup`]; tests call it with a manual clock.
//!
//! # Semantics
//!
//! - **Fire times** are the matches of the cron expression in the
//!   schedule's time zone, inside `[starts_at, ends_at)`, that its calendar
//!   (if any) allows, evaluated in the calendar's own zone. DST follows
//!   belay-cron: a time skipped by a spring-forward fires once at the
//!   first instant after the gap, and a repeated time fires once.
//!   `Schedule::next_fire_at` is always the next such time.
//! - **On time or missed.** A due fire processed within the misfire
//!   threshold ([`SchedulerOptions::misfire_threshold`], 60s by default)
//!   is on time. Older due fires are misfires and the schedule's
//!   [`MisfirePolicy`](belay_proto::MisfirePolicy) decides: `skip` drops
//!   them, `fire_once` fires once for the lot, stamped with the *first*
//!   missed time, and `fire_all:<max>` fires the oldest `max` of them.
//! - **Overlap.** Before firing, the state of the previous fire's job is
//!   checked: `allow` ignores it, `skip` skips the fire while that job is
//!   unfinished, `cancel_previous` asks the dispatcher to cancel it first.
//! - **Paused** schedules keep moving through their fire times, reporting
//!   each as skipped, so resuming never unleashes a burst of catch-up jobs.
//! - **Jitter.** Each fire's job gets `run_at = fire_time + offset`, where
//!   the offset is derived from the schedule id and fire time
//!   ([`jitter_offset`]) and lies in `[0, jitter)`. The job is dispatched at
//!   the nominal time; the queue holds it until `run_at`.
//! - **Exactly one job per fire.** Each dispatched job carries the unique
//!   key `sched:<id>:<fire time in ms>` for
//!   [`SchedulerOptions::dedupe_window`], plus `meta["belay.schedule"]` and
//!   `meta["belay.fire_time"]`. Fire selection is deterministic from the
//!   stored `next_fire_at`, so if belay stops after dispatching but before
//!   saving, the next tick repeats the same fires and the dispatcher hands
//!   back the jobs it already has.

mod calendar;

mod plan;

mod report;

mod tick;

pub use plan::jitter_offset;
pub use report::{Fired, SkipReason, Skipped, TickError, TickErrorKind, TickReport};

pub(crate) use calendar::CompiledCalendar;
pub(crate) use plan::Plan;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::sync::lock;
use belay_common::{warn, SharedClock, Timestamp};
use belay_json::{FromJsonError, PathSegment};
use belay_proto::api::{Cursor, Page};
use belay_proto::event::EventKind;
use belay_proto::job::{META_FIRE_TIME, META_SCHEDULE};
use belay_proto::validate::{Validate, Validator, Violations};
use belay_proto::{
    CalendarSpec, EventSink, JobSpec, Schedule, ScheduleId, ScheduleSpec, Unique, UniqueScope,
};
use belay_store::{Kv, WriteBatch};

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::options::SchedulerOptions;
use crate::store;

/// `meta` key marking jobs started by [`Scheduler::trigger_now`]; its
/// value is `manual`.
pub const META_TRIGGER: &str = "belay.trigger";

/// Most fire times [`Scheduler::preview`] returns.
pub const MAX_PREVIEW: usize = 1000;

/// The unique key of the job for schedule `id`'s fire at `fire_time`.
///
/// ```
/// use belay_common::Timestamp;
/// use belay_proto::ScheduleId;
///
/// let key = belay_scheduler::fire_key(&ScheduleId::from_static("nightly"), Timestamp::from_secs(60));
/// assert_eq!(key, "sched:nightly:60000");
/// ```
#[must_use]
pub fn fire_key(id: &ScheduleId, fire_time: Timestamp) -> String {
    format!("sched:{id}:{}", fire_time.as_millis())
}

/// What an upsert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upserted {
    /// Nothing by that name existed; it was created.
    Created,
    /// It existed with a different definition, which was replaced.
    Updated,
    /// It existed with exactly this definition; nothing was written.
    Unchanged,
}

/// Cron schedules and calendars. See the [module docs](self) for how
/// schedules fire.
///
/// All methods take `&self`; one internal lock serialises them, and
/// [`tick`](Self::tick) holds it while it dispatches, so the dispatcher
/// must not call back into the scheduler.
pub struct Scheduler {
    kv: Arc<dyn Kv>,
    clock: SharedClock,
    dispatcher: Arc<dyn Dispatcher>,
    sink: Arc<dyn EventSink>,
    options: SchedulerOptions,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    schedules: BTreeMap<ScheduleId, Entry>,
    calendars: BTreeMap<String, Arc<CompiledCalendar>>,
}
