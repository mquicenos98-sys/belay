//! A blocking HTTP/1.1 client for `http://` URLs, with a keep-alive pool,
//! stale-connection retry, opt-in redirects and an event-stream reader. The
//! behaviour is documented on [`Client`].

mod conn;
mod event_source;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::headers::names::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, COOKIE, LOCATION};
use crate::headers::Headers;
use crate::method::{Method, Version};
use crate::proto::framing::{response_framing, wants_keep_alive, BodyFraming};
use crate::request::Request;
use crate::status::StatusCode;
use crate::url::Url;

pub use conn::ResponseBody;
use conn::{connect, exchange, Conn, Outgoing, Pool};
pub use event_source::{EventStream, SseOptions};

/// Unwanted redirect bodies up to this size are read so the connection can
/// be reused.
const REDIRECT_DRAIN_LIMIT: usize = 64 * 1024;

/// Which redirects the client follows on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RedirectPolicy {
    /// Maximum redirects followed for one request; 0 (the default) returns
    /// 3xx responses to the caller.
    pub max_hops: u32,
    /// Keep `Authorization` and `Cookie` when a redirect leaves the
    /// original host and port. Off by default so credentials are not leaked
    /// to a third party; turn it on for trusted peers such as the nodes of
    /// one cluster.
    pub forward_credentials: bool,
}

impl RedirectPolicy {
    /// Never follow redirects.
    pub const fn none() -> Self {
        RedirectPolicy {
            max_hops: 0,
            forward_credentials: false,
        }
    }

    /// Follow up to `max_hops` redirects.
    pub const fn limited(max_hops: u32) -> Self {
        RedirectPolicy {
            max_hops,
            forward_credentials: false,
        }
    }

    /// Sets [`forward_credentials`](Self::forward_credentials).
    #[must_use]
    pub const fn with_forwarded_credentials(mut self, forward: bool) -> Self {
        self.forward_credentials = forward;
        self
    }
}

/// Client settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Time allowed to establish a TCP connection (per resolved address).
    /// Default 5 s.
    pub connect_timeout: Duration,
    /// Longest wait for the next bytes of a response; `None` waits forever.
    /// Must exceed any server-side long-poll wait. Default 30 s.
    pub read_timeout: Option<Duration>,
    /// Longest a write may block; `None` waits forever. Default 30 s.
    pub write_timeout: Option<Duration>,
    /// Idle connections kept per host and port; 0 disables pooling (every
    /// request then asks the server to close). Default 8.
    pub max_idle_per_host: usize,
    /// How long an idle connection stays in the pool. Keep this below the
    /// server's keep-alive timeout (5 s for a belay server) so the pool
    /// rarely offers a connection the server is about to close. Default 4 s.
    pub idle_timeout: Duration,
    /// `User-Agent` sent unless a request sets its own; empty sends none.
    pub user_agent: String,
    /// Redirect handling. Default: not followed.
    pub redirects: RedirectPolicy,
    /// Largest response body [`Client::request`] reads into memory.
    /// Default 64 MiB.
    pub max_response_bytes: u64,
    /// Largest response head accepted. Default 64 KiB.
    pub max_head_bytes: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            connect_timeout: Duration::from_secs(5),
            read_timeout: Some(Duration::from_secs(30)),
            write_timeout: Some(Duration::from_secs(30)),
            max_idle_per_host: 8,
            idle_timeout: Duration::from_secs(4),
            user_agent: concat!("belay-http/", env!("CARGO_PKG_VERSION")).to_owned(),
            redirects: RedirectPolicy::none(),
            max_response_bytes: 64 * 1024 * 1024,
            max_head_bytes: 64 * 1024,
        }
    }
}

/// Per-request overrides of [`ClientConfig`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// Replaces [`ClientConfig::read_timeout`] (e.g. for a long-poll that
    /// waits longer than usual).
    pub read_timeout: Option<Duration>,
    /// Replaces [`ClientConfig::redirects`].
    pub redirects: Option<RedirectPolicy>,
}

/// A complete response.
#[derive(Clone, Debug)]
pub struct ClientResponse {
    /// Status code.
    pub status: StatusCode,
    /// Version the server answered with.
    pub version: Version,
    /// Header fields.
    pub headers: Headers,
    /// The body.
    pub body: Vec<u8>,
    /// The URL that produced this response (after redirects).
    pub url: Url,
}

impl ClientResponse {
    /// The first value of header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// The body as UTF-8 text.
    pub fn text(&self) -> Result<&str> {
        std::str::from_utf8(&self.body).map_err(Error::Utf8)
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Turns 4xx and 5xx responses into [`Error::Status`], carrying the
    /// first 512 bytes of the body.
    pub fn error_for_status(self) -> Result<ClientResponse> {
        if self.status.is_client_error() || self.status.is_server_error() {
            let end = self.body.len().min(512);
            return Err(Error::Status {
                status: self.status,
                body: String::from_utf8_lossy(&self.body[..end])
                    .trim_end()
                    .to_owned(),
            });
        }
        Ok(self)
    }
}

/// A response whose body has not been read yet.
#[derive(Debug)]
pub struct StreamingResponse {
    /// Status code.
    pub status: StatusCode,
    /// Version the server answered with.
    pub version: Version,
    /// Header fields.
    pub headers: Headers,
    /// The URL that produced this response (after redirects).
    pub url: Url,
    /// The body, readable incrementally.
    pub body: ResponseBody,
}

impl StreamingResponse {
    /// The first value of header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// Reads the rest of the body into a [`ClientResponse`].
    pub fn collect(mut self) -> Result<ClientResponse> {
        let body = self.body.read_to_vec()?;
        Ok(ClientResponse {
            status: self.status,
            version: self.version,
            headers: self.headers,
            body,
            url: self.url,
        })
    }
}

struct ClientInner {
    config: ClientConfig,
    pool: Arc<Pool>,
}

/// An HTTP client. Cheap to clone; clones share the connection pool.
///
/// Connections are pooled per host and port. A pooled connection is probed
/// (non-blocking peek) before reuse, and an idempotent request that fails
/// on a reused connection before any response byte arrives is retried once
/// on a fresh one: that is the signature of a connection the server closed
/// while it sat idle. Non-idempotent requests (POST, PATCH) are never
/// retried, since the server may have acted on them. A response is only
/// pooled once its body has been read to the end.
///
/// There is no TLS support: `https://` URLs are rejected with
/// [`Error::UnsupportedScheme`]. DNS resolution uses the system resolver
/// through `ToSocketAddrs`, which has no timeout of its own;
/// [`ClientConfig::connect_timeout`] applies per resolved address.
///
/// ```no_run
/// use belay_http::{Client, ClientConfig};
///
/// let client = Client::new(ClientConfig::default());
/// let jobs = client
///     .get("http://127.0.0.1:7070/v1/jobs?state=ready", &[("Authorization", "Bearer s3cret")])?
///     .error_for_status()?;
/// println!("{}", jobs.text()?);
/// # Ok::<(), belay_http::Error>(())
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Default for Client {
    fn default() -> Self {
        Client::new(ClientConfig::default())
    }
}

impl Client {
    /// A client with the given settings.
    pub fn new(config: ClientConfig) -> Client {
        let pool = Arc::new(Pool::new(config.max_idle_per_host, config.idle_timeout));
        Client {
            inner: Arc::new(ClientInner { config, pool }),
        }
    }

    /// The settings.
    pub fn config(&self) -> &ClientConfig {
        &self.inner.config
    }

    /// Idle connections currently pooled.
    pub fn idle_connections(&self) -> usize {
        self.inner.pool.idle_count()
    }

    /// Closes every idle pooled connection.
    pub fn clear_idle(&self) {
        self.inner.pool.clear();
    }

    /// Sends `req`, whose target must be an absolute `http://` URL, and
    /// reads the whole response.
    pub fn request(&self, req: Request) -> Result<ClientResponse> {
        self.request_with(req, &RequestOptions::default())
    }

    /// [`request`](Self::request) with per-request overrides.
    pub fn request_with(&self, req: Request, options: &RequestOptions) -> Result<ClientResponse> {
        self.stream_with(req, options)?.collect()
    }

    /// Sends `req` and returns as soon as the response head has arrived.
    pub fn stream(&self, req: Request) -> Result<StreamingResponse> {
        self.stream_with(req, &RequestOptions::default())
    }

    /// [`stream`](Self::stream) with per-request overrides.
    pub fn stream_with(&self, req: Request, options: &RequestOptions) -> Result<StreamingResponse> {
        let mut url = Url::parse(&req.target)?;
        check_scheme(&url)?;
        let Request {
            mut method,
            mut headers,
            mut body,
            ..
        } = req;
        let config = &self.inner.config;
        let policy = options.redirects.unwrap_or(config.redirects);
        let read_timeout = options.read_timeout.or(config.read_timeout);
        let mut hops = 0;
        loop {
            let response = self.execute(&method, &url, &headers, &body, read_timeout)?;
            let status = response.status;
            let followed = matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308);
            let location = match response.header(LOCATION) {
                Some(location) if followed && policy.max_hops > 0 => location.to_owned(),
                _ => return Ok(response),
            };
            if hops == policy.max_hops {
                return Err(Error::TooManyRedirects {
                    max: policy.max_hops,
                });
            }
            hops += 1;
            let next = url.join(&location)?;
            check_scheme(&next)?;
            response.body.discard(REDIRECT_DRAIN_LIMIT);
            // RFC 9110 §15.4: 303 always switches to GET; 301/302 do so for
            // POST by long-standing convention. 307/308 preserve everything.
            let to_get = status == StatusCode::SEE_OTHER
                || (matches!(status.as_u16(), 301 | 302) && method == Method::Post);
            if to_get {
                if method != Method::Head {
                    method = Method::Get;
                }
                body.clear();
                headers.remove(CONTENT_TYPE);
            }
            let same_origin =
                next.host() == url.host() && next.port_or_default() == url.port_or_default();
            if !same_origin && !policy.forward_credentials {
                headers.remove(AUTHORIZATION);
                headers.remove(COOKIE);
            }
            url = next;
        }
    }

    /// One request/response exchange, retrying once on a fresh connection
    /// when a pooled one turns out to be stale.
    fn execute(
        &self,
        method: &Method,
        url: &Url,
        headers: &Headers,
        body: &[u8],
        read_timeout: Option<Duration>,
    ) -> Result<StreamingResponse> {
        let config = &self.inner.config;
        let port = url.port_or_default().unwrap_or(80);
        let key = (url.host().to_owned(), port);
        let outgoing = Outgoing {
            method,
            url,
            headers,
            body,
            user_agent: &config.user_agent,
            close: config.max_idle_per_host == 0,
            read_timeout,
            write_timeout: config.write_timeout,
            max_head_bytes: config.max_head_bytes,
        };
        if let Some(stream) = self.inner.pool.checkout(&key) {
            let mut conn = Conn::new(stream, key.clone(), true);
            match exchange(&mut conn, &outgoing) {
                Ok(head) => return self.respond(conn, head, &outgoing),
                Err(failure) if failure.stale => {}
                Err(failure) => return Err(failure.error),
            }
        }
        let stream = connect(&key.0, key.1, config.connect_timeout)?;
        let mut conn = Conn::new(stream, key, false);
        match exchange(&mut conn, &outgoing) {
            Ok(head) => self.respond(conn, head, &outgoing),
            Err(failure) => Err(failure.error),
        }
    }

    fn respond(
        &self,
        conn: Conn,
        head: crate::proto::parse::ResponseHead,
        outgoing: &Outgoing<'_>,
    ) -> Result<StreamingResponse> {
        let framing = response_framing(outgoing.method, head.status, &head.headers)?;
        let keep_alive = !outgoing.close
            && !outgoing.headers.has_token(CONNECTION, "close")
            && wants_keep_alive(head.version, &head.headers)
            && framing != BodyFraming::UntilClose;
        let body = ResponseBody::new(
            conn,
            framing,
            keep_alive,
            Arc::clone(&self.inner.pool),
            self.inner.config.max_response_bytes,
        );
        Ok(StreamingResponse {
            status: head.status,
            version: head.version,
            headers: head.headers,
            url: outgoing.url.clone(),
            body,
        })
    }

    /// `GET url`.
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<ClientResponse> {
        self.request(build(Method::Get, url, headers, Vec::new()))
    }

    /// `HEAD url`.
    pub fn head(&self, url: &str, headers: &[(&str, &str)]) -> Result<ClientResponse> {
        self.request(build(Method::Head, url, headers, Vec::new()))
    }

    /// `DELETE url`.
    pub fn delete(&self, url: &str, headers: &[(&str, &str)]) -> Result<ClientResponse> {
        self.request(build(Method::Delete, url, headers, Vec::new()))
    }

    /// `POST url` with `body`.
    pub fn post(
        &self,
        url: &str,
        body: impl Into<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<ClientResponse> {
        self.request(build(Method::Post, url, headers, body.into()))
    }

    /// `PUT url` with `body`.
    pub fn put(
        &self,
        url: &str,
        body: impl Into<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<ClientResponse> {
        self.request(build(Method::Put, url, headers, body.into()))
    }

    /// `PATCH url` with `body`.
    pub fn patch(
        &self,
        url: &str,
        body: impl Into<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<ClientResponse> {
        self.request(build(Method::Patch, url, headers, body.into()))
    }

    /// Opens an event stream with default options; see [`EventStream`].
    pub fn sse(&self, url: &str) -> Result<EventStream> {
        self.sse_with(url, SseOptions::default())
    }

    /// Opens an event stream. The first connection is made before this
    /// returns, so a refused connection or an error status (401, 404, …)
    /// is reported here rather than retried.
    pub fn sse_with(&self, url: &str, options: SseOptions) -> Result<EventStream> {
        let url = Url::parse(url)?;
        check_scheme(&url)?;
        EventStream::connect(self.clone(), url, options)
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("config", &self.inner.config)
            .field("pool", &self.inner.pool)
            .finish()
    }
}

fn build(method: Method, url: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Request {
    let mut req = Request::new(method, url).with_body(body);
    req.headers.extend(headers.iter().copied());
    req
}

fn check_scheme(url: &Url) -> Result<()> {
    if url.scheme() == "http" {
        Ok(())
    } else {
        Err(Error::UnsupportedScheme(url.scheme().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_urls_before_connecting() {
        let client = Client::default();
        assert!(matches!(
            client.get("https://example.com/", &[]),
            Err(Error::UnsupportedScheme(s)) if s == "https"
        ));
        assert!(matches!(client.get("not a url", &[]), Err(Error::Url(_))));
        assert!(matches!(
            client.sse("ftp://h/"),
            Err(Error::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn connection_refused_is_an_io_error() {
        // Bind and drop to find a port nobody listens on.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("ephemeral port")
            .port();
        let client = Client::new(ClientConfig {
            connect_timeout: Duration::from_millis(500),
            ..ClientConfig::default()
        });
        let err = client
            .get(&format!("http://127.0.0.1:{port}/"), &[])
            .expect_err("nothing listens there");
        assert!(err.is_transient(), "{err}");
    }

    #[test]
    fn error_for_status_keeps_a_body_excerpt() {
        let response = ClientResponse {
            status: StatusCode::CONFLICT,
            version: Version::Http11,
            headers: Headers::new(),
            body: b"schedule nightly already exists\n".to_vec(),
            url: Url::parse("http://h/").expect("valid"),
        };
        let err = response.error_for_status().expect_err("409 is an error");
        assert_eq!(
            err.to_string(),
            "server responded 409 Conflict: schedule nightly already exists"
        );
    }

    #[test]
    fn redirect_policy_builders() {
        assert_eq!(RedirectPolicy::default(), RedirectPolicy::none());
        let policy = RedirectPolicy::limited(3).with_forwarded_credentials(true);
        assert_eq!(policy.max_hops, 3);
        assert!(policy.forward_credentials);
    }
}
