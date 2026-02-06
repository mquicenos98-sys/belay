//! The belay engine: storage, the job queue, cron schedules, workflows,
//! the event stream, metrics and optional clustering, composed into one
//! [`Engine`] and configured from one TOML file ([`Config`]).
//!
//! The HTTP server and the `belay` command are thin layers over this
//! crate: every API endpoint is an `Engine` method returning
//! `Result<_, ApiError>`, and `Engine` implements the worker-facing
//! [`JobSource`] so workers can run in-process
//! ([`Engine::embedded_worker`]).
//!
//! ```
//! use std::time::Duration;
//! use belay_common::{Clock, ManualClock, Timestamp};
//! use belay_core::{Config, Engine};
//! use belay_proto::api::{CompleteRequest, LeaseRequest};
//! use belay_proto::{JobSource, JobSpec, JobState, QueueName, ScheduleSpec, WorkerId};
//!
//! let clock = ManualClock::new("2025-03-03T08:00:00Z".parse::<Timestamp>()?);
//! let engine = Engine::builder()
//!     .config(Config::dev())
//!     .clock(clock.shared())
//!     .background(false) // drive the periodic duties with tick()
//!     .start()?;
//!
//! engine.create_schedule(ScheduleSpec::new(
//!     "hourly-report".parse()?,
//!     "0 * * * *",
//!     JobSpec::new("reports", "build"),
//! ))?;
//! clock.advance(Duration::from_secs(3600));
//! let report = engine.tick(clock.now());
//! assert_eq!(report.schedules.fired.len(), 1);
//!
//! let worker = WorkerId::from_static("w1");
//! let leased = engine.lease(&LeaseRequest::new(worker, vec![QueueName::from_static("reports")]))?;
//! let job = &leased[0];
//! engine.complete(&job.job.id, &CompleteRequest::new(job.token))?;
//! assert_eq!(engine.get_job(&job.job.id)?.state, JobState::Succeeded);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Modules
//!
//! | module | contents |
//! |---|---|
//! | [`config`] | [`Config`] and its sections, loading, environment overlay, validation, the example file, auth tokens |
//! | [`engine`](Engine) | [`Engine`], [`EngineBuilder`], [`TickReport`], [`Paging`], admin reports |
//! | [`events`] | [`EventBus`]: sequence numbers, replay buffer, subscriptions with lag handling |
//! | [`cluster`] | [`Ownership`] of queues and schedules across nodes |
//! | [`metrics`] | the metric families the engine registers |
//! | [`sha256`] | SHA-256 and constant-time comparison for token checks |
//!
//! # Endpoints
//!
//! | endpoint | method |
//! |---|---|
//! | `GET /v1/info`, `/healthz`, `/readyz` | [`Engine::info`], [`Engine::is_ready`] |
//! | `GET /metrics` | [`Engine::render_metrics`] |
//! | `POST /v1/jobs` | [`Engine::enqueue_request`] ([`Engine::enqueue`], [`Engine::enqueue_batch`]) |
//! | `GET /v1/jobs`, `GET /v1/jobs/:id` | [`Engine::list_jobs`], [`Engine::get_job`] |
//! | `POST /v1/jobs/:id/cancel`, `/retry`, `/promote`; `DELETE /v1/jobs/:id` | [`Engine::cancel_job`], [`Engine::retry_job`], [`Engine::promote_job`], [`Engine::delete_job`] |
//! | `POST /v1/lease`, `/v1/jobs/:id/heartbeat`, `/complete`, `/fail` | [`Engine::lease_response`] and [`JobSource`] |
//! | `GET /v1/queues`, `GET /v1/queues/:name` | [`Engine::list_queues`], [`Engine::queue_info`] |
//! | `PUT /v1/queues/:name/config` | [`Engine::set_queue_config`] |
//! | `POST /v1/queues/:name/pause`, `/resume`, `/purge`; `DELETE` | [`Engine::pause_queue`], [`Engine::resume_queue`], [`Engine::purge_queue`], [`Engine::delete_queue`] |
//! | `GET /v1/schedules`, `POST /v1/schedules` | [`Engine::list_schedules`], [`Engine::create_schedule`] |
//! | `GET\|PUT\|DELETE /v1/schedules/:id` | [`Engine::get_schedule`], [`Engine::put_schedule`], [`Engine::delete_schedule`] |
//! | `POST /v1/schedules/:id/pause`, `/resume`, `/trigger` | [`Engine::pause_schedule`], [`Engine::resume_schedule`], [`Engine::trigger_schedule`] |
//! | `GET /v1/schedules/:id/preview?n=` | [`Engine::preview_schedule`] |
//! | `GET\|PUT\|DELETE /v1/calendars/:name`, `GET /v1/calendars` | [`Engine::get_calendar`], [`Engine::put_calendar`], [`Engine::delete_calendar`], [`Engine::list_calendars`] |
//! | `POST /v1/workflows`, `GET /v1/workflows[/:id]`, `POST .../cancel` | [`Engine::submit_workflow`], [`Engine::list_workflows`], [`Engine::get_workflow`], [`Engine::cancel_workflow`] |
//! | `GET /v1/workers` | [`Engine::workers`] |
//! | `GET /v1/events?from=&types=` | [`Engine::subscribe`] |
//! | `GET /v1/cluster/members` | [`Engine::members`]; redirects use [`Engine::owner_of`] |
//! | `POST /v1/admin/checkpoint`, `GET /v1/admin/store` | [`Engine::checkpoint`], [`Engine::store_report`], [`Engine::verify`] |
//!
//! Bearer tokens are checked with [`config::AuthConfig::authorize`].
//!
//! # Threads
//!
//! A started engine runs four background threads: the queue tick
//! (promotions, lease expiry, retention), the scheduler (sleeping until
//! the next fire time), the workflow chores (retention of finished runs,
//! reconciling steps with their jobs) and housekeeping (lease latencies
//! into metrics, time-based checkpoints). Clustering adds the gossip
//! threads of belay-cluster, and a log-structured store its own sync and
//! checkpoint thread. [`EngineBuilder::background`] turns the engine's
//! threads off for tests, which then call [`Engine::tick`].
//!
//! # Limitations
//!
//! - Clustering decides *where* a queue is served, not where its data
//!   lives: there is no replication, and a node's queues are unavailable
//!   while it is down. Views of the membership converge but are not agreed
//!   on at any instant, so ownership can briefly overlap after a change.
//! - Jobs created by schedules and workflows go into the queue of the node
//!   that runs them. In a cluster, give such jobs queues that the same
//!   node owns (or run workers against every node), or they wait on a node
//!   the owner's workers are not redirected to.
//! - When a node takes over a schedule it continues from the present: a
//!   fire that the previous owner missed while dying is not caught up.
//! - Event sequence numbers live in memory. They keep increasing across
//!   restarts (they start from the start time), but events from before a
//!   restart cannot be replayed.
//! - Configured queues, calendars and schedules are created or updated at
//!   startup; removing one from the file does not delete it.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod cluster;
pub mod config;
mod dispatch;
mod engine;
mod error;
pub mod events;
pub mod metrics;
pub mod sha256;

pub use cluster::{Ownership, RemoteOwner};
pub use config::Config;
pub use engine::{Engine, EngineBuilder, Paging, StoreReport, TickReport, VerifyOutcome};
pub use error::{Error, Result};
pub use events::{Delivery, EventBus, RecvError, Subscription};

pub use belay_proto::api::ApiError;
pub use belay_proto::JobSource;
pub use belay_scheduler::{Upserted, WorkflowDetails, WorkflowFilter};

/// This crate's version, reported by [`Engine::info`] and the
/// `belay_info` metric.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
