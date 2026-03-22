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
pub const READER: &str = "reader-secret-0123456789";

pub struct TestServer {
    pub engine: Arc<Engine>,
    pub clock: ManualClock,
    pub url: String,
    handle: Option<ServerHandle>,
}

impl TestServer {
    /// An open server (no authentication) with default options.
    pub fn start() -> TestServer {
        TestServer::with(test_config(), ServerOptions::default())
    }

    /// A server requiring tokens: `admin`, `producer` (enqueue),
    /// `worker` (work) and `reader` (read).
    pub fn secured(options: ServerOptions) -> TestServer {
        let mut config = test_config();
        for (name, secret, scope) in [
            ("admin", ADMIN, Scope::Admin),
            ("producer", PRODUCER, Scope::Enqueue),
            ("worker", WORKER, Scope::Work),
            ("reader", READER, Scope::Read),
        ] {
            let scopes: Scopes = [scope].into_iter().collect();
            config.auth.tokens.push(ApiToken::new(name, secret, scopes));
        }
        TestServer::with(config, options)
    }

    pub fn with(config: Config, options: ServerOptions) -> TestServer {
        quiet_logs();
        let clock = ManualClock::new(START.parse::<Timestamp>().expect("valid start"));
        let engine = Engine::builder()
            .config(config)
            .clock(clock.shared())
            .background(false)
            .seed(7)
            .start()
            .unwrap_or_else(|e| panic!("engine did not start: {e}"));
        let engine = Arc::new(engine);
        let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
        let handle = belay_server::serve_on_with(Arc::clone(&engine), listener, options)
            .expect("server starts");
        TestServer {
            url: format!("http://{}", handle.local_addr()),
            engine,
            clock,
            handle: Some(handle),
        }
    }

    pub fn handle(&self) -> &ServerHandle {
        self.handle.as_ref().expect("still running")
    }

    /// Takes the handle, e.g. to wait for a shutdown.
    pub fn take_handle(&mut self) -> ServerHandle {
        self.handle.take().expect("still running")
    }

    /// The typed client without a token.
    pub fn client(&self) -> Client {
        Client::new(client_config(&self.url)).expect("valid client config")
    }

    /// The typed client presenting `token`.
    pub fn client_as(&self, token: &str) -> Client {
        Client::new(client_config(&self.url).token(token)).expect("valid client config")
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// A raw request, not following redirects.
    pub fn send(
        &self,
        method: Method,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> ClientResponse {
        let mut req = Request::new(method, self.url(path)).with_body(body);
        if !body.is_empty() {
            req.headers.insert("Content-Type", "application/json");
        }
        if let Some(token) = token {
            req.headers
                .insert("Authorization", format!("Bearer {token}"));
        }
        http().request(req).expect("the server answers")
    }

    pub fn get(&self, path: &str, token: Option<&str>) -> ClientResponse {
        self.send(Method::Get, path, "", token)
    }

    pub fn post(&self, path: &str, body: &str, token: Option<&str>) -> ClientResponse {
        self.send(Method::Post, path, body, token)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown_and_join();
        }
    }
}

/// `Config::dev()` with a short shutdown grace.
pub fn test_config() -> Config {
    let mut config = Config::dev();
    config.server.shutdown_grace = Duration::from_secs(5);
    config
}

/// Near-instant retries and a generous timeout.
pub fn client_config(url: &str) -> ClientConfig {
    ClientConfig::new(url)
        .backoff(BackoffPolicy::constant(Duration::from_millis(1)))
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(2))
}

/// A raw HTTP client that returns redirects instead of following them.
pub fn http() -> belay_http::Client {
    belay_http::Client::new(belay_http::ClientConfig {
        read_timeout: Some(Duration::from_secs(10)),
        ..belay_http::ClientConfig::default()
    })
}

/// The `ApiError` in an error response, checking the JSON content type
/// and that the status matches the code.
pub fn api_error(response: &ClientResponse) -> ApiError {
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "error bodies are JSON: {}",
        String::from_utf8_lossy(&response.body)
    );
    let status = response.status.as_u16();
    let error = decode_error(status, &response.body);
    if !matches!(status, 405 | 413) {
        assert_eq!(error.http_status(), status, "{error:?}");
    }
    error
}

/// The response body as JSON.
pub fn json(response: &ClientResponse) -> belay_json::Value {
    belay_json::parse_bytes(&response.body).expect("JSON body")
}

/// Keeps test output readable: only warnings and errors are logged.
pub fn quiet_logs() {
    belay_common::log::set_max_level(belay_common::log::Level::Warn);
}

/// Polls `condition` until it holds or `timeout` passes.
pub fn eventually(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
