//! The [`Client`] handle and the pipeline every call goes through: URL
//! building, authentication, retries with backoff, cluster redirects and
//! response decoding.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use belay_http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use belay_http::percent::{encode, EncodeSet};
use belay_http::{ClientResponse, Method, RedirectPolicy, RequestOptions};
use belay_json::{FromJson, ToJson};
use belay_proto::api::{decode_error, encode_body};

use crate::config::ClientConfig;
use crate::error::{DecodeError, Error, Result};
use crate::resources::{Admin, Calendars, Jobs, Queues, Schedules, Workflows};
use crate::retry::{parse_retry_after, Failure, RetryPolicy, Safety};

const JSON: &str = "application/json";

/// A typed client for a belay server's HTTP API.
///
/// Cheap to clone: clones share one connection pool and configuration, so
/// create one per process and hand clones to threads. Calls block the
/// calling thread.
///
/// Collections are reached through a handle named after them
/// ([`jobs`](Self::jobs), [`queues`](Self::queues),
/// [`schedules`](Self::schedules), [`calendars`](Self::calendars),
/// [`workflows`](Self::workflows), [`admin`](Self::admin)); server-wide
/// calls ([`info`](Self::info), [`health`](Self::health),
/// [`ready`](Self::ready), [`metrics`](Self::metrics),
/// [`workers`](Self::workers), [`members`](Self::members),
/// [`events`](Self::events)) are methods of the client itself.
///
/// ```no_run
/// use belay_client::{Client, ClientConfig};
/// use belay_json::json;
/// use belay_proto::JobSpec;
///
/// let client = Client::new(ClientConfig::new("http://127.0.0.1:7700").token("s3cret"))?;
/// let created = client
///     .jobs()
///     .enqueue(&JobSpec::new("emails", "send_welcome").payload(json!({"user": 42})))?;
/// let job = client.jobs().get(created.id)?;
/// println!("{} is {}", job.id, job.state);
/// # Ok::<(), belay_client::Error>(())
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}
