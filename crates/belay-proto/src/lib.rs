//! belay's domain model and API types: jobs, queues, schedules, calendars,
//! workflows, events, and the request and response bodies of the HTTP API.
//!
//! Everything here is plain data with hand-written JSON conversions
//! (`ToJson`/`FromJson` from belay-json), validation, and the small pure
//! helpers that every engine needs to agree on: the job state machine,
//! uniqueness scopes, calendar windows, workflow graph checks, filters and
//! cursors. No I/O, no clocks, no threads.
//!
//! | module | contents |
//! |---|---|
//! | [`ids`] | [`JobId`], [`WorkflowId`], [`QueueName`], [`ScheduleId`], [`WorkerId`], [`TenantId`], [`LeaseToken`] |
//! | [`job`] | [`JobSpec`] (with builder), [`Job`], [`JobState`] machine, [`Lease`], [`JobError`], [`Progress`] |
//! | [`queue`] | [`QueueConfig`], [`QueueInfo`], [`StateCounts`] |
//! | [`schedule`] | [`ScheduleSpec`], [`Schedule`], misfire/overlap policies, [`CalendarSpec`] |
//! | [`workflow`] | [`WorkflowSpec`] with DAG validation, [`Workflow`] runs |
//! | [`event`] | [`Event`](event::Event), [`EventKind`](event::EventKind), type patterns and stream filters |
//! | [`api`] | request/response bodies, [`ApiError`](api::ApiError), [`JobFilter`](api::JobFilter), cursors, pages |
//! | [`source`] | the [`JobSource`] trait workers consume, [`EventSink`] |
//! | [`validate`], [`limits`] | semantic validation and every limit it enforces |
//! | [`wire`] | JSON encodings of timestamps, durations and backoff policies |
//!
//! # Wire format
//!
//! One set of conventions covers API bodies, stored records and events:
//!
//! - **Field names** are `snake_case`.
//! - **Timestamps** are RFC 3339 strings in UTC with milliseconds
//!   (`"2025-03-04T05:06:07.089Z"`). Input may use any offset; it is
//!   normalised to UTC.
//! - **Durations** are compact strings (`"30s"`, `"1h30m"`, `"250ms"`),
//!   exactly as `belay_common::format_duration` writes them. Input uses the
//!   `parse_duration` grammar, where a unitless string (`"90"`) is seconds.
//!   JSON numbers are **not** accepted as durations: whether `30000` means
//!   seconds or milliseconds depends on the writer, so it is rejected with a
//!   message saying how to spell it.
//! - **Backoff policies** use their text form: `"exp:1s*2..10m~full"`,
//!   `"const:5s"`, `"linear:1s+2s..1m"`.
//! - **Enums** are lowercase strings (`"ready"`, `"fire_once"`,
//!   `"return_existing"`). Variants with a parameter use `name:param`
//!   (`"for:1h"` for a uniqueness scope, `"fire_all:10"` for a misfire
//!   policy), so the same text works in JSON, query strings and CLI flags.
//!   Unknown names are rejected with a "did you mean" hint.
//! - **Identifiers** are strings: ULIDs for jobs and workflows, validated
//!   names for everything else, 16 hex digits for lease tokens.
//! - **Absent versus default**: `None` optionals and empty lists and maps
//!   are omitted on output; plain scalars (`priority`, `paused`, ...) are
//!   always written. On input, a missing field and an explicit `null` both
//!   mean "not set" and take the default. The exceptions are handler
//!   results (`Job::result`, `CompleteRequest::result`), where `null` is a
//!   real value and is kept.
//! - **Unknown fields**: types a client *sends* (specs, requests,
//!   configurations) reject unknown fields, naming the closest known one,
//!   so typos fail loudly. Types the server *produces* (records, responses,
//!   events) ignore unknown fields, so an older client can read a newer
//!   server; new spec fields are optional and omitted when unset, so they
//!   only reach old clients when someone uses them.
//! - **Errors** carry a path in `$.jobs[2].priority` notation, both for
//!   decoding problems and for rule violations.
//!
//! # Decoding versus validation
//!
//! `FromJson` checks shape and syntax; [`Validate`] checks policy (ranges,
//! [`limits`], cross-field rules, workflow graphs). Stored records are only
//! decoded, never re-validated, so a limit tightened in a later release
//! cannot make existing jobs unreadable. Servers read request bodies with
//! [`api::decode_request`], which does both.
//!
//! ```
//! use std::time::Duration;
//! use belay_json::json;
//! use belay_proto::{JobSpec, UniqueScope, Validate};
//!
//! let spec = JobSpec::new("emails", "send_welcome")
//!     .payload(json!({"user": 42}))
//!     .delay(Duration::from_secs(30))
//!     .unique("user:42", UniqueScope::UntilFinished);
//! assert!(spec.validate().is_ok());
//!
//! let text = belay_json::to_string_of(&spec);
//! let back: JobSpec = belay_json::from_str(&text).unwrap();
//! assert_eq!(back, spec);
//!
//! let err = belay_json::from_str::<JobSpec>(r#"{"queue": "emails", "kind": "k", "delay": 30}"#)
//!     .unwrap_err();
//! assert_eq!(
//!     err.to_string(),
//!     r#"$.delay: expected duration string such as "30s" or "1h30m", found 30"#
//! );
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod api;
pub mod error;
pub mod event;
pub mod ids;
pub mod job;
pub mod limits;
mod query;
pub mod queue;
pub mod schedule;
pub mod source;
pub mod validate;
pub mod wire;
pub mod workflow;

pub use error::{Error, ParseError, Result};
pub use ids::{
    IntoQueueName, JobId, LeaseToken, QueueName, ScheduleId, TenantId, WorkerId, WorkflowId,
};
pub use job::{
    Job, JobError, JobSpec, JobState, Lease, OnConflict, Progress, TransitionError, Unique,
    UniqueScope, WorkflowRef,
};
pub use queue::{QueueConfig, QueueInfo, RateLimit, Retention, StateCounts, Throughput};
pub use schedule::{CalendarSpec, MisfirePolicy, OverlapPolicy, Schedule, ScheduleSpec};
pub use source::{EventSink, JobSource};
pub use validate::{Validate, Validator, Violation, Violations};
pub use workflow::{Workflow, WorkflowSpec};
