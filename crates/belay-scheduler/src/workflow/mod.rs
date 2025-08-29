//! DAG workflows: runs of steps, each a job, started as their dependencies
//! finish.
//!
//! The rules (eligibility, conditions, failure policies, when a run
//! succeeds) are stated in [`belay_proto::workflow`]; payload placeholders
//! are described below.
//!
//! # Driving the engine
//!
//! - [`WorkflowEngine::submit`] validates a spec, stores the run and
//!   dispatches its root steps.
//! - [`WorkflowEngine::on_job_finished`] must be called with every job that
//!   reaches a terminal state (belay-core wires it to the queue's
//!   `on_finished` hook). Jobs that are not workflow steps, or whose run is
//!   over, are ignored; reporting the same job twice is harmless.
//! - [`WorkflowEngine::recover`] runs once after opening, and may run
//!   periodically: it asks the dispatcher about every running step (to
//!   catch completions reported while the engine was down) and dispatches
//!   steps whose dispatch was deferred or lost.
//! - [`WorkflowEngine::sweep`] deletes finished runs past their retention.
//!
//! # Crash safety
//!
//! A run is stored before any of its jobs is dispatched, and every step job
//! carries the unique key `wf:<run>:<step>` (unless the step's spec brings
//! its own key). If belay stops between dispatching a step and saving that
//! fact, recovery dispatches the step again and the dispatcher returns the
//! original job, whose outcome is then picked up.
//!
//! # Payload placeholders
//!
//! A string in a step's payload may refer to the result of an ancestor
//! step as `{{steps.<name>.result}}`, optionally followed by a JSON Pointer
//! (`{{steps.fetch.result/items/0}}`) or dotted keys
//! (`{{steps.fetch.result.items.0}}`). A string that is exactly one
//! placeholder becomes the referenced value itself, of whatever type;
//! placeholders inside longer strings are replaced by text (strings as
//! they are, other values as compact JSON). Placeholders are resolved when
//! the step is dispatched; a missing value (the step did not succeed, or
//! the path leads nowhere) fails the step with an error naming the
//! placeholder, and its failure policy applies.
//!
//! # Limitations
//!
//! Retrying a step's dead job by hand after the run has moved on does not
//! reopen the step. Results are kept only for steps that some placeholder
//! reads.

mod machine;

mod record;

mod template;

pub use record::{WorkflowDetails, WorkflowFilter};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::id::SyncUlidGenerator;
use belay_common::sync::lock;
use belay_common::{info, warn, SharedClock, Timestamp};
use belay_json::Value;
use belay_proto::api::{Cursor, Page, SortOrder};
use belay_proto::event::EventKind;
use belay_proto::validate::{Validate, Validator};
use belay_proto::workflow::{referenced_steps, StepSpec, StepState, WorkflowSpec, WorkflowState};
use belay_proto::{EventSink, Job, JobId, Unique, UniqueScope, Workflow, WorkflowId, WorkflowRef};
use belay_store::{Kv, WriteBatch};

use crate::dispatch::Dispatcher;
use crate::error::{Error, Result};
use crate::options::WorkflowOptions;
use crate::store;

use machine::{Effects, Machine, Outcome, Start};

/// Most finished runs one [`WorkflowEngine::sweep`] call deletes, to keep
/// its write batch bounded.
pub const SWEEP_BATCH: usize = 1000;
