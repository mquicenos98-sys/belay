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
