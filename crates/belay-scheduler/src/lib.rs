//! Cron schedules, business calendars and DAG workflows for belay.
//!
//! Two engines live here, both driven from outside and both creating jobs
//! only through a [`Dispatcher`] (belay-core implements it over the queue;
//! [`MemoryDispatcher`] stands in for tests and examples):
//!
//! | engine | does |
//! |---|---|
//! | [`Scheduler`] | schedule and calendar CRUD, pause/resume, manual triggers, previews, and [`tick`](Scheduler::tick), which fires what is due under the misfire, overlap, jitter, calendar and bounds rules |
//! | [`WorkflowEngine`] | runs [`WorkflowSpec`](belay_proto::WorkflowSpec) DAGs: dispatches steps as their dependencies finish, applies conditions and failure policies, resolves payload placeholders, recovers after downtime |
//!
//! Both keep their records as JSON documents in a [`belay_store::Kv`] and
//! write every change as one atomic batch; both report what happens through
//! an [`EventSink`](belay_proto::EventSink). Neither runs threads of its
//! own: the caller ticks the scheduler (sleeping until
//! [`Scheduler::next_wakeup`]) and feeds finished jobs to
//! [`WorkflowEngine::on_job_finished`]. That keeps them deterministic under
//! a [`ManualClock`](belay_common::ManualClock).
//!
//! ```
//! use std::sync::Arc;
//! use std::time::Duration;
//! use belay_common::{Clock, ManualClock, Timestamp};
//! use belay_proto::source::CollectingSink;
//! use belay_proto::{JobSpec, ScheduleSpec};
//! use belay_scheduler::{MemoryDispatcher, Scheduler};
//! use belay_store::MemKv;
//!
//! let clock = ManualClock::new("2025-03-03T08:00:00Z".parse::<Timestamp>().unwrap());
//! let queue = Arc::new(MemoryDispatcher::new(clock.shared()));
//! let scheduler = Scheduler::open(
//!     Arc::new(MemKv::new()),
//!     clock.shared(),
//!     queue.clone(),
//!     Arc::new(CollectingSink::new()),
//! )?;
//!
//! let spec = ScheduleSpec::new(
//!     "morning-report".parse().unwrap(),
//!     "0 9 * * MON-FRI",
//!     JobSpec::new("reports", "build"),
//! )
//! .timezone("Europe/Berlin");
//! let schedule = scheduler.create(spec)?;
//! // 09:00 in Berlin is 08:00 UTC in winter; creation at 08:00 UTC is not
//! // strictly before it, so the first fire is Tuesday.
//! assert_eq!(schedule.next_fire_at.unwrap().to_string(), "2025-03-04T08:00:00.000Z");
//!
//! clock.advance(Duration::from_secs(86_400));
//! let report = scheduler.tick(clock.now());
//! assert_eq!(report.fired.len(), 1);
//! let job = queue.last().unwrap();
//! assert_eq!(job.spec.meta["belay.schedule"], "morning-report");
//! assert_eq!(job.spec.unique.unwrap().key, "sched:morning-report:1741075200000");
//! # Ok::<(), belay_scheduler::Error>(())
//! ```
//!
//! # Persistence
//!
//! | key | document |
//! |---|---|
//! | `sched/<schedule id>` | [`Schedule`](belay_proto::Schedule) |
//! | `cal/<calendar name>` | [`CalendarSpec`](belay_proto::CalendarSpec) |
//! | `wf/<run id>` | [`WorkflowDetails`] |
//! | `wfr/<run id>/<step>` | a step result that a later step's placeholders read |
//!
//! # Limitations
//!
//! - Time zones follow belay-cron: only each zone's current DST rule is
//!   known, so fire times in years before a zone's last rule change use
//!   today's rule.
//! - Deduplication of a retried fire or step relies on the dispatcher's
//!   unique keys and lasts for the configured window (24 hours by default);
//!   a crash between dispatching and saving followed by a longer outage can
//!   produce a second job under `fire_once` or `fire_all`.
//! - Calendars have minute resolution, like [`TimeOfDay`](belay_proto::schedule::TimeOfDay).
//! - Everything is held in memory except the documents of finished
//!   workflow runs, which are read from the store on demand.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod dispatch;

mod error;

mod options;

pub mod schedule;

mod store;
