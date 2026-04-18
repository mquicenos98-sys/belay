//! Shared test support: running the CLI in-process from shell-like command
//! lines, a fake API server that records requests and answers from a queue
//! of canned replies, and a real server over an in-memory engine.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use belay_cli::{Exit, Runner};
use belay_common::{ManualClock, Timestamp};
use belay_core::{Config, Engine};
use belay_http::{
    MatchedRoute, Method, Request, Response, Router, Server, ServerConfig, ServerHandle, StatusCode,
};
use belay_json::{ToJson, Value};
use belay_proto::api::{encode_body, ApiError};
use belay_proto::{Job, JobId, JobSpec};

/// Splits a command line into arguments the way a POSIX shell does for
/// simple cases: whitespace separates, and single or double quotes group
/// (no escapes), so `enqueue q k -d '{"a": 1}'` is five arguments.
pub fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    assert!(quote.is_none(), "unbalanced quote in {line:?}");
    if in_word {
        words.push(current);
    }
    words
}

/// What one invocation printed and returned.
#[derive(Debug)]
pub struct Output {
    pub exit: Exit,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Fails with both streams shown unless the command succeeded.
    #[track_caller]
    pub fn ok(self) -> Output {
        self.exits(Exit::Success)
    }

    /// Fails with both streams shown unless the command exited with `exit`.
    #[track_caller]
    pub fn exits(self, exit: Exit) -> Output {
        assert_eq!(
            self.exit, exit,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }

    /// Stdout parsed as JSON.
    #[track_caller]
    pub fn json(&self) -> Value {
        belay_json::parse(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }

    /// Stdout without surrounding whitespace, e.g. an id printed by `-q`.
    pub fn id(&self) -> String {
        self.stdout.trim().to_owned()
    }
}

/// The fixed "now" of test runs.
pub fn t0() -> Timestamp {
    "2025-03-04T05:06:07.089Z".parse().expect("valid timestamp")
}

/// An isolated runner with the test clock.
pub fn runner() -> Runner {
    Runner::isolated().clock(ManualClock::new(t0()).shared())
}

/// Runs `belay <line>` with `runner`.
pub fn run_with(runner: &Runner, line: &str) -> Output {
    run_args(runner, words(line))
}

/// Runs `belay` with already split arguments.
pub fn run_args(runner: &Runner, args: Vec<String>) -> Output {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = runner.run(args, &mut out, &mut err);
    Output {
        exit,
        stdout: String::from_utf8(out).expect("stdout is UTF-8"),
        stderr: String::from_utf8(err).expect("stderr is UTF-8"),
    }
}

/// Runs `belay <line>` in an isolated environment.
pub fn belay(line: &str) -> Output {
    run_with(&runner(), line)
}

/// Every route of the v1 API, so wrong paths and methods get the router's
/// 404/405 rather than a canned success.
const ROUTES: &[(Method, &str)] = &[
    (Method::Get, "/healthz"),
    (Method::Get, "/readyz"),
    (Method::Get, "/v1/info"),
    (Method::Post, "/v1/jobs"),
    (Method::Get, "/v1/jobs"),
    (Method::Get, "/v1/jobs/:id"),
    (Method::Delete, "/v1/jobs/:id"),
    (Method::Post, "/v1/jobs/:id/cancel"),
    (Method::Post, "/v1/jobs/:id/retry"),
    (Method::Post, "/v1/jobs/:id/promote"),
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
    (Method::Get, "/v1/cluster/members"),
    (Method::Post, "/v1/admin/checkpoint"),
    (Method::Get, "/v1/admin/store"),
    (Method::Get, "/v1/admin/verify"),
    (Method::Post, "/v1/admin/shutdown"),
];

/// A request as the fake server saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    /// The route pattern that matched, e.g. `/v1/jobs/:id/cancel`.
    pub route: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

impl Seen {
    /// `METHOD route`, for compact assertions.
    pub fn call(&self) -> String {
        format!("{} {}", self.method, self.route)
    }

    pub fn json(&self) -> Value {
        belay_json::parse_bytes(&self.body).expect("request body is JSON")
    }

    pub fn query_pairs(&self) -> Vec<(&str, &str)> {
        self.query
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
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
        let config = ServerConfig {
            keep_alive_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_secs(2),
            poll_interval: Duration::from_millis(10),
            ..ServerConfig::default()
        };
        let handle = Server::bind("127.0.0.1:0", config)
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

    /// A runner pointed at the fake, with the test clock.
    pub fn runner(&self) -> Runner {
        runner().env("BELAY_URL", self.url())
    }

    /// Runs `belay <line>` against the fake.
    pub fn belay(&self, line: &str) -> Output {
        run_with(&self.runner(), line)
    }

    pub fn reply_with(&self, reply: impl FnOnce(&Request) -> Response + Send + 'static) {
        guard(&self.state.replies).push_back(Box::new(reply));
    }

    pub fn reply_json<T: ToJson + ?Sized>(&self, status: u16, body: &T) {
        let bytes = encode_body(body);
        self.reply_with(move |_| {
            Response::json_bytes(bytes).with_status(StatusCode::from_u16(status).expect("status"))
        });
    }

    pub fn reply_error(&self, error: ApiError) {
        self.reply_json(error.http_status(), &error);
    }

    pub fn reply_status(&self, status: u16) {
        self.reply_with(move |_| Response::status(StatusCode::from_u16(status).expect("status")));
    }

    pub fn requests(&self) -> Vec<Seen> {
        guard(&self.state.seen).clone()
    }

    /// The only request seen; fails if there were more or none.
    #[track_caller]
    pub fn only(&self) -> Seen {
        let seen = self.requests();
        assert_eq!(seen.len(), 1, "expected exactly one request, saw {seen:#?}");
        seen.into_iter().next().expect("one request")
    }

    pub fn calls(&self) -> Vec<String> {
        self.requests().iter().map(Seen::call).collect()
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
        query: req
            .query
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        authorization: req.headers.get("authorization").map(str::to_owned),
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

pub fn job_id(n: u8) -> JobId {
    format!("01HQ3K9Z8N6V2T4R5S7W0X1Y{n:02}")
        .parse()
        .expect("valid job id")
}

/// A ready job on queue `emails`, created a minute before [`t0`].
pub fn sample_job(n: u8) -> Job {
    Job::new(
        job_id(n),
        JobSpec::new("emails", "send"),
        t0() - Duration::from_secs(60),
    )
}

/// A real server over an in-memory engine on an ephemeral port.
pub struct Live {
    pub handle: Option<belay_server::ServerHandle>,
    pub engine: Arc<Engine>,
    pub url: String,
}

impl Live {
    pub fn start() -> Live {
        Live::with_config(Config::dev())
    }

    pub fn with_config(config: Config) -> Live {
        let engine = Arc::new(Engine::start(config).expect("engine starts"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let handle = belay_server::serve_on(Arc::clone(&engine), listener).expect("server starts");
        let url = format!("http://{}", handle.local_addr());
        Live {
            handle: Some(handle),
            engine,
            url,
        }
    }

    /// A runner pointed at this server, with the real clock.
    pub fn runner(&self) -> Runner {
        Runner::isolated().env("BELAY_URL", self.url.clone())
    }

    /// Runs `belay <line>` against this server.
    pub fn belay(&self, line: &str) -> Output {
        run_with(&self.runner(), line)
    }

    /// Like [`belay`](Self::belay), reading `stdin`.
    pub fn belay_stdin(&self, stdin: &str, line: &str) -> Output {
        run_with(&self.runner().stdin(stdin.as_bytes().to_vec()), line)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown_and_join();
        }
    }
}

/// Polls `check` every few milliseconds until it holds or `timeout`
/// passes; returns whether it held.
pub fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
