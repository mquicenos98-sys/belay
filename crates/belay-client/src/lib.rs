//! Typed Rust client for the belay HTTP/JSON API (v1).
//!
//! [`Client`] wraps every endpoint of a belay server in a method that takes
//! and returns the [`belay_proto`] types, so request bodies, query strings
//! and error bodies are never handled by hand:
//!
//! | handle | endpoints |
//! |---|---|
//! | [`client.jobs()`](Client::jobs) | enqueue (one or a batch), get, list, cancel, retry, promote, delete; lease (long-poll), heartbeat, complete, fail |
//! | [`client.queues()`](Client::queues) | list, get, set config, pause, resume, purge, delete |
//! | [`client.schedules()`](Client::schedules) | list, get, create, put, delete, pause, resume, trigger, preview |
//! | [`client.calendars()`](Client::calendars) | list, get, put, delete |
//! | [`client.workflows()`](Client::workflows) | submit, get, list, cancel |
//! | [`client.admin()`](Client::admin) | checkpoint, store statistics |
//! | [`Client`] itself | [`info`](Client::info), [`health`](Client::health), [`ready`](Client::ready), [`metrics`](Client::metrics), [`workers`](Client::workers), [`members`](Client::members), [`events`](Client::events) |
//!
//! Listings come one page at a time (`list`) or as a lazy iterator over
//! every item (`list_all`, a [`Paginator`]). [`Client::events`] follows the
//! server-sent event stream and resumes after disconnects without gaps or
//! repeats. `Client` also implements [`JobSource`](belay_proto::JobSource),
//! so a worker runtime can lease from a remote server.
//!
//! # Failures
//!
//! Every call returns [`Result`]. An error answer from the server is
//! decoded into [`Error::Api`] carrying the server's
//! [`ApiError`](belay_proto::api::ApiError); branch on its code
//! ([`Error::is_not_found`], [`Error::code`]). Requests that are safe to
//! repeat are retried with backoff on connection failures and on `502`,
//! `503`, `504` and `429`, honouring `Retry-After`; the [`retry`] module
//! lists exactly which calls qualify and why.
//!
//! # Clusters
//!
//! A node answers requests for queues owned by another node with a `307`
//! redirect to the owner. The client follows up to
//! [`max_redirects`](ClientConfig::max_redirects) of them, forwarding the
//! bearer token, since every node of a cluster is equally trusted.
//!
//! ```no_run
//! use std::time::Duration;
//! use belay_client::{Client, ClientConfig};
//! use belay_json::json;
//! use belay_proto::api::{CompleteRequest, LeaseRequest};
//! use belay_proto::{JobSpec, QueueName, UniqueScope, WorkerId};
//!
//! let client = Client::new(ClientConfig::from_env().timeout(Duration::from_secs(10)))?;
//!
//! // A uniqueness key makes the enqueue safe to retry automatically.
//! let spec = JobSpec::new("emails", "send_welcome")
//!     .payload(json!({"user": 42}))
//!     .unique("welcome:42", UniqueScope::UntilFinished);
//! let created = client.jobs().enqueue(&spec)?;
//!
//! // Worker side: long-poll for up to 20 seconds.
//! let request = LeaseRequest::new(
//!     WorkerId::new("worker-1").expect("valid worker id"),
//!     vec![QueueName::from_static("emails")],
//! )
//! .wait(Duration::from_secs(20));
//! for leased in client.jobs().lease(&request)? {
//!     client.jobs().complete(leased.job.id, &CompleteRequest::new(leased.token))?;
//! }
//! assert!(client.jobs().get(created.id)?.state.is_terminal());
//! # Ok::<(), belay_client::Error>(())
//! ```
//!
//! # Limitations
//!
//! Plain `http://` only (no TLS; use a TLS-terminating proxy). Calls block
//! the calling thread; there is no async interface. The redirect to a
//! queue's owner is not remembered, so every request pays the extra hop.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod client;

mod config;

mod error;

mod events;
