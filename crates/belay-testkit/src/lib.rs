//! Harnesses for testing belay in-process, end to end: a real engine
//! behind a real HTTP server on `127.0.0.1:0`, real workers leasing over
//! HTTP, clusters on an in-memory gossip network, and the fixtures and
//! waits that make such tests short and deterministic.
//!
//! | item | what it is |
//! |---|---|
//! | [`TestServer`] | engine + server with a manual (or system) clock; [`tick`](TestServer::tick), [`restart`](TestServer::restart), [`crash`](TestServer::crash) |
//! | [`TestWorker`] | the belay-worker runtime over the typed client, with recording handlers and a cuttable link |
//! | [`TestCluster`] | several servers gossiping over a [`MemNetwork`](belay_cluster::MemNetwork); ownership lookups, [`kill`](TestCluster::kill), [`restart`](TestCluster::restart) |
//! | [`Recorder`], [`Behavior`], [`Gate`] | what handlers ran, and what they do: succeed, fail `n` times, sleep, wait, fail at random |
//! | [`EventLog`] | the `/v1/events` stream collected on a thread |
//! | [`fixtures`] | `job`, `schedule`, `workflow`, `step`, payload helpers |
//! | [`eventually`], [`wait_for_state`], ... | polling with timeouts that say what they waited for |
//! | [`LogCapture`] | log records from every thread, for assertions |
//! | [`TcpProxy`], [`raw`] | dropped connections; raw HTTP for statuses, headers and redirects |
//!
//! The crate's own `tests/` directory holds belay's end-to-end suite and
//! is the best catalogue of how the pieces combine.
//!
//! # Example
//!
//! A job that fails twice, retried on the backoff curve of
//! [`fixtures::TEST_RETRY`] (1 s, then 2 s), with the test moving the
//! manual clock:
//!
//! ```
//! use std::time::Duration;
//! use belay_testkit::prelude::*;
//!
//! let server = TestServer::start()?;
//! let worker = server.worker().queue("sync").failing_n_times("pull", 2).start()?;
//! let client = server.client();
//! let id = client.jobs().enqueue(&job("sync", "pull").payload(json!({"repo": "belay"})))?.id;
//!
//! wait_for_state(&client, id, JobState::Retrying, DEFAULT_TIMEOUT)?;
//! server.tick(Duration::from_secs(1))?; // first retry is due
//! wait_for_job(&client, id, "retrying again", DEFAULT_TIMEOUT, |j| {
//!     j.state == JobState::Retrying && j.attempt == 2
//! })?;
//! server.tick(Duration::from_secs(2))?; // second retry is due
//!
//! let done = wait_for_state(&client, id, JobState::Succeeded, DEFAULT_TIMEOUT)?;
//! assert_eq!(done.attempt, 3);
//! assert_eq!(done.result, Some(json!({"repo": "belay"})));
//! assert_eq!(worker.recorder().attempts_of(id), [1, 2, 3]);
//! # Ok::<(), belay_testkit::Error>(())
//! ```
//!
//! # Conventions
//!
//! - Everything that can fail returns [`Result`], whose [`Error`] converts
//!   from the typed client's, the engine's and the store's errors, so test
//!   functions can return `belay_testkit::Result<()>` and use `?`.
//! - Waits take explicit timeouts; [`DEFAULT_TIMEOUT`] is generous (a
//!   timeout bounds how long a broken test hangs, it is not a performance
//!   claim).
//! - Harnesses stop on drop: servers drain and shut their engine down,
//!   workers hand back what they hold and join their threads.
//! - Workers run in real time while a manual clock only moves when told.
//!   Jumping it past a running job's lease or timeout expires the job, as
//!   the same gap would in production, so let running jobs finish (or give
//!   them long leases) before a jump.
//! - [`Recorder`]s see a handler return before its worker reports the
//!   outcome; assertions about server state wait on the server
//!   ([`wait_for_state`]) rather than on a recorder.
//! - Nothing is logged to stderr unless `BELAY_TEST_LOG` asks for it
//!   (`BELAY_TEST_LOG=debug cargo test ...`, see [`logs`]).
//!
//! # Limitations
//!
//! - One process: nodes of a [`TestCluster`] share an address space and a
//!   clock, so clock skew between nodes is not modelled.
//! - Event sequence numbers start from the engine's start time in whole
//!   seconds, so a server restarted within the same second, or on a
//!   manual clock that has not moved, reuses sequence numbers from before
//!   the restart; do not resume an event stream across such a restart.
//! - [`TestServer::crash`] needs [`Storage::SimulatedDisk`]; a real disk
//!   cannot be made to forget unsynced writes.
//! - Handlers run in the test process, so a handler that ignores
//!   cancellation keeps its worker thread busy (see belay-worker).

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod cluster;
mod endpoint;
mod error;
mod events;
pub mod fixtures;
mod handlers;
pub mod logs;
mod proxy;
pub mod raw;
mod server;
mod wait;
mod worker;

pub use cluster::{TestCluster, TestClusterBuilder};
pub use endpoint::{client_config, connect, CONNECT_TIMEOUT};
pub use error::{Error, Result};
pub use events::{first_gap, EventLog};
pub use handlers::{Behavior, Execution, Gate, Recorded, Recorder, RecordingHandler};
pub use logs::{init_test_logging, LogCapture};
pub use proxy::TcpProxy;
pub use server::{tokens, Storage, TestClock, TestServer, TestServerBuilder};
pub use wait::{
    eventually, eventually_some, wait_for_job, wait_for_state, wait_for_terminal,
    wait_for_workflow, DEFAULT_TIMEOUT,
};
pub use worker::{TestWorker, TestWorkerBuilder};

pub use belay_json::json;

/// The kit plus the belay types most tests touch, for a glob import.
pub mod prelude {
    pub use crate::fixtures::{
        job, numbered, numbered_jobs, placeholder, queue_name, schedule, step, workflow,
    };
    pub use crate::{
        eventually, eventually_some, tokens, wait_for_job, wait_for_state, wait_for_terminal,
        wait_for_workflow, Behavior, EventLog, Gate, LogCapture, Recorded, Recorder, Result,
        Storage, TestCluster, TestServer, TestWorker, DEFAULT_TIMEOUT,
    };
    pub use belay_client::Client;
    pub use belay_json::{json, Value};
    pub use belay_proto::api::{ApiError, ErrorCode, JobFilter};
    pub use belay_proto::event::{Event, EventFilter, EventKind};
    pub use belay_proto::workflow::{StepFailurePolicy, StepState, WorkflowState};
    pub use belay_proto::{
        Job, JobId, JobSpec, JobState, MisfirePolicy, QueueConfig, QueueName, ScheduleId,
        ScheduleSpec, UniqueScope, WorkflowSpec,
    };
}
