//! A test harness: a real server on 127.0.0.1:0 over an in-memory engine
//! on a manual clock, plus clients for it (the typed belay client and a
//! raw belay-http client for looking at statuses and headers).

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_client::{Client, ClientConfig};
use belay_common::{BackoffPolicy, ManualClock, Timestamp};
use belay_core::config::{ApiToken, Scope, Scopes};
use belay_core::{Config, Engine};
use belay_http::{ClientResponse, Method, Request};
use belay_proto::api::{decode_error, ApiError};
use belay_server::{ServerHandle, ServerOptions};

/// Monday 2025-03-03 08:00 UTC.
pub const START: &str = "2025-03-03T08:00:00Z";

/// Token secrets of [`TestServer::secured`], one per scope.
pub const ADMIN: &str = "admin-secret-0123456789";

pub const PRODUCER: &str = "producer-secret-0123456789";

pub const WORKER: &str = "worker-secret-0123456789";
