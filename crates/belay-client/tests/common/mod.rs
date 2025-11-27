//! An in-process fake belay server for the client tests.
//!
//! It registers every route of the v1 API on a belay-http `Router`, so a
//! client that builds a wrong path or method gets the router's 404/405
//! rather than a canned success. Each request is recorded; answers come
//! from a queue of canned replies filled by the test, and an empty queue
//! answers 500 with a message naming the request.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use belay_client::{Client, ClientConfig};
use belay_common::{BackoffPolicy, Timestamp};
use belay_http::{
    Headers, MatchedRoute, Method, Request, Response, Router, Server, ServerConfig, ServerHandle,
    StatusCode,
};
use belay_json::{ToJson, Value};
use belay_proto::api::{encode_body, ApiError};
use belay_proto::{Job, JobId, JobSpec};

pub const TOKEN: &str = "test-token";

/// Every route of the v1 API.
const ROUTES: &[(Method, &str)] = &[
    (Method::Get, "/healthz"),
    (Method::Get, "/readyz"),
    (Method::Get, "/metrics"),
    (Method::Get, "/v1/info"),
    (Method::Post, "/v1/jobs"),
    (Method::Get, "/v1/jobs"),
    (Method::Get, "/v1/jobs/:id"),
    (Method::Delete, "/v1/jobs/:id"),
    (Method::Post, "/v1/jobs/:id/cancel"),
    (Method::Post, "/v1/jobs/:id/retry"),
    (Method::Post, "/v1/jobs/:id/promote"),
    (Method::Post, "/v1/jobs/:id/heartbeat"),
    (Method::Post, "/v1/jobs/:id/complete"),
    (Method::Post, "/v1/jobs/:id/fail"),
    (Method::Post, "/v1/lease"),
    (Method::Get, "/v1/queues"),
    (Method::Get, "/v1/queues/:name"),
    (Method::Delete, "/v1/queues/:name"),
    (Method::Put, "/v1/queues/:name/config"),
    (Method::Post, "/v1/queues/:name/pause"),
    (Method::Post, "/v1/queues/:name/resume"),
    (Method::Post, "/v1/queues/:name/purge"),
    (Method::Get, "/v1/schedules"),
    (Method::Post, "/v1/schedules"),
    (Method::Get, "/v1/schedules/:id"),
    (Method::Put, "/v1/schedules/:id"),
    (Method::Delete, "/v1/schedules/:id"),
    (Method::Post, "/v1/schedules/:id/pause"),
    (Method::Post, "/v1/schedules/:id/resume"),
    (Method::Post, "/v1/schedules/:id/trigger"),
    (Method::Get, "/v1/schedules/:id/preview"),
    (Method::Get, "/v1/calendars"),
    (Method::Get, "/v1/calendars/:name"),
    (Method::Put, "/v1/calendars/:name"),
    (Method::Delete, "/v1/calendars/:name"),
    (Method::Post, "/v1/workflows"),
    (Method::Get, "/v1/workflows"),
    (Method::Get, "/v1/workflows/:id"),
    (Method::Post, "/v1/workflows/:id/cancel"),
    (Method::Get, "/v1/workers"),
    (Method::Get, "/v1/events"),
    (Method::Get, "/v1/cluster/members"),
    (Method::Post, "/v1/admin/checkpoint"),
    (Method::Get, "/v1/admin/store"),
];

/// A request as the fake server saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    /// The route pattern that matched, e.g. `/v1/jobs/:id/cancel`.
    pub route: String,
    /// The path as sent, still percent-encoded.
    pub path: String,
    pub params: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    pub fn query_pairs(&self) -> Vec<(&str, &str)> {
        self.query
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// The body parsed as JSON.
    pub fn json(&self) -> Value {
        belay_json::parse_bytes(&self.body).expect("request body is JSON")
    }

    /// `METHOD route`, for compact assertions.
    pub fn call(&self) -> String {
        format!("{} {}", self.method, self.route)
    }
}

type Reply = Box<dyn FnOnce(&Request) -> Response + Send>;

#[derive(Default)]
struct State {
    seen: Mutex<Vec<Seen>>,
    replies: Mutex<VecDeque<Reply>>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Short timeouts so shutdown and keep-alive expiry are quick.
pub fn server_config() -> ServerConfig {
    ServerConfig {
        keep_alive_timeout: Duration::from_secs(2),
        header_timeout: Duration::from_secs(2),
        body_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_secs(2),
        poll_interval: Duration::from_millis(10),
        ..ServerConfig::default()
    }
}

/// The fake server; shut down when dropped.
pub struct Fake {
    handle: Option<ServerHandle>,
    state: Arc<State>,
}
