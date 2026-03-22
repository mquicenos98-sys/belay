//! Bearer tokens and scopes as a client sees them, and per-token rate
//! limits.

mod common;

use belay_http::Method;
use belay_proto::api::ErrorCode;
use belay_proto::JobSpec;
use belay_server::{Access, RequestRate, ServerOptions};
use common::{api_error, TestServer, ADMIN, PRODUCER, READER, WORKER};

const JOB: &str = r#"{"queue": "emails", "kind": "send"}"#;

const LEASE: &str = r#"{"worker": "w1", "queues": ["emails"]}"#;
