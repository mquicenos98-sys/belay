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

impl Fake {
    pub fn start() -> Fake {
        let state = Arc::new(State::default());
        let mut router = Router::new();
        for (method, pattern) in ROUTES {
            let state = Arc::clone(&state);
            router.route(method.clone(), pattern, move |req: &mut Request| {
                answer(&state, req)
            });
        }
        let handle = Server::bind("127.0.0.1:0", server_config())
            .expect("bind an ephemeral port")
            .spawn(router)
            .expect("spawn the fake server");
        Fake {
            handle: Some(handle),
            state,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.as_ref().expect("running").local_addr()
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr())
    }

    /// A client with the test token, 1 ms retry backoff and 3 retries.
    pub fn client(&self) -> Client {
        Client::new(fast_config(&self.url())).expect("valid client config")
    }

    /// A client built from `config` adjusted by `adjust`.
    pub fn client_with(&self, adjust: impl FnOnce(ClientConfig) -> ClientConfig) -> Client {
        Client::new(adjust(fast_config(&self.url()))).expect("valid client config")
    }

    /// Queues an answer computed from the request.
    pub fn reply_with(&self, reply: impl FnOnce(&Request) -> Response + Send + 'static) {
        guard(&self.state.replies).push_back(Box::new(reply));
    }

    /// Queues a JSON answer.
    pub fn reply_json<T: ToJson + ?Sized>(&self, status: u16, body: &T) {
        let bytes = encode_body(body);
        self.reply_with(move |_| {
            Response::json_bytes(bytes).with_status(StatusCode::from_u16(status).expect("status"))
        });
    }

    /// Queues an [`ApiError`] answer with its proper status.
    pub fn reply_error(&self, error: ApiError) {
        self.reply_json(error.http_status(), &error);
    }

    /// Queues an empty answer.
    pub fn reply_status(&self, status: u16) {
        self.reply_with(move |_| Response::status(StatusCode::from_u16(status).expect("status")));
    }

    pub fn requests(&self) -> Vec<Seen> {
        guard(&self.state.seen).clone()
    }

    pub fn count(&self) -> usize {
        guard(&self.state.seen).len()
    }

    /// The only request seen; fails if there were more or none.
    pub fn only(&self) -> Seen {
        let seen = self.requests();
        assert_eq!(seen.len(), 1, "expected exactly one request, saw {seen:#?}");
        seen.into_iter().next().expect("one request")
    }

    pub fn last(&self) -> Seen {
        self.requests().pop().expect("at least one request")
    }

    /// Canned replies not consumed yet.
    pub fn pending_replies(&self) -> usize {
        guard(&self.state.replies).len()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown_and_join();
        }
    }
}

fn answer(state: &State, req: &mut Request) -> Response {
    let route = req
        .extensions
        .get::<MatchedRoute>()
        .map(|r| r.as_str().to_owned())
        .unwrap_or_default();
    guard(&state.seen).push(Seen {
        method: req.method.clone(),
        route,
        path: req.path.clone(),
        params: req
            .params
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        query: req
            .query
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        headers: req.headers.clone(),
        body: req.body.clone(),
    });
    let reply = guard(&state.replies).pop_front();
    match reply {
        Some(reply) => reply(req),
        None => {
            let error = ApiError::internal(format!(
                "fake server has no reply queued for {} {}",
                req.method, req.path
            ));
            Response::json_bytes(encode_body(&error)).with_status(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Test defaults: the fake's token, near-instant retries, generous timeout.
pub fn fast_config(url: &str) -> ClientConfig {
    ClientConfig::new(url)
        .token(TOKEN)
        .backoff(BackoffPolicy::constant(Duration::from_millis(1)))
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(2))
}

/// A fixed point in time for sample records.
pub fn t0() -> Timestamp {
    "2025-03-04T05:06:07.089Z".parse().expect("valid timestamp")
}

pub fn job_id(n: u8) -> JobId {
    format!("01HQ3K9Z8N6V2T4R5S7W0X1Y{n:02}")
        .parse()
        .expect("valid job id")
}

/// A ready job on queue `emails`.
pub fn sample_job(n: u8) -> Job {
    Job::new(job_id(n), JobSpec::new("emails", "send"), t0())
}

/// A port with nothing listening on it.
pub fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("ephemeral port")
        .port()
}

/// Test defaults for a server that is not there. Linux refuses such a
/// connection at once; Windows retries the handshake for about two seconds
/// first, so the connect timeout is kept short. Either way nothing is sent.
pub fn unreachable_config() -> ClientConfig {
    fast_config(&format!("http://127.0.0.1:{}", dead_port()))
        .connect_timeout(Duration::from_millis(200))
}

/// A listener that reads each request head and then hangs up without
/// answering, counting the connections: what a crashing server looks like.
pub struct Hangup {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Hangup {
    pub fn start() -> Hangup {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (accepted, stop) = (Arc::clone(&accepted), Arc::clone(&stop));
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    accepted.fetch_add(1, Ordering::SeqCst);
                    read_head(&mut stream);
                }
            })
        };
        Hangup {
            addr,
            accepted,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Hangup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_head(stream: &mut TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    // Consume a small body too, so closing does not reset the connection
    // before the client finished writing.
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    if let Some(len) = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
    }
    let _ = stream.flush();
}
