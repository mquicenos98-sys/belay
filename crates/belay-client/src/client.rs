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

struct Inner {
    config: ClientConfig,
    /// `http://host:port[/prefix]`, without a trailing slash.
    base: String,
    http: belay_http::Client,
    /// The complete `Authorization` header value.
    auth: Option<String>,
    retry: RetryPolicy,
}

impl Client {
    /// A client for the server described by `config`. Nothing is sent
    /// until the first call; an unusable configuration fails here with
    /// [`Error::Config`].
    pub fn new(config: ClientConfig) -> Result<Client> {
        let base = config.validated_base()?;
        let http = belay_http::Client::new(belay_http::ClientConfig {
            connect_timeout: config.connect_timeout,
            read_timeout: Some(config.timeout),
            write_timeout: Some(config.timeout),
            user_agent: config.user_agent.clone(),
            // Cluster nodes redirect to each other; the token must follow
            // or the owner answers 401.
            redirects: RedirectPolicy::limited(config.max_redirects)
                .with_forwarded_credentials(true),
            ..belay_http::ClientConfig::default()
        });
        let auth = config.token.as_ref().map(|t| format!("Bearer {t}"));
        let retry = RetryPolicy::new(config.retries, config.backoff, config.timeout);
        Ok(Client {
            inner: Arc::new(Inner {
                config,
                base,
                http,
                auth,
                retry,
            }),
        })
    }

    /// A client configured from `BELAY_URL` and `BELAY_TOKEN`; see
    /// [`ClientConfig::from_env`].
    pub fn from_env() -> Result<Client> {
        Client::new(ClientConfig::from_env())
    }

    /// The configuration this client was built with.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.inner.config
    }

    /// The normalised base URL requests are sent to.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.inner.base
    }

    /// Jobs: enqueue, inspect, manage, and the worker protocol.
    #[must_use]
    pub fn jobs(&self) -> Jobs<'_> {
        Jobs::new(self)
    }

    /// Queues: status, configuration, pause/resume, purge.
    #[must_use]
    pub fn queues(&self) -> Queues<'_> {
        Queues::new(self)
    }

    /// Cron schedules.
    #[must_use]
    pub fn schedules(&self) -> Schedules<'_> {
        Schedules::new(self)
    }

    /// Calendars that restrict when schedules fire.
    #[must_use]
    pub fn calendars(&self) -> Calendars<'_> {
        Calendars::new(self)
    }

    /// Workflow runs.
    #[must_use]
    pub fn workflows(&self) -> Workflows<'_> {
        Workflows::new(self)
    }

    /// Administrative calls: checkpoints and storage statistics.
    #[must_use]
    pub fn admin(&self) -> Admin<'_> {
        Admin::new(self)
    }

    pub(crate) fn http(&self) -> &belay_http::Client {
        &self.inner.http
    }

    pub(crate) fn auth_header(&self) -> Option<&str> {
        self.inner.auth.as_deref()
    }

    /// The absolute URL for an already encoded `path` plus `query`.
    pub(crate) fn url(&self, path: &str, query: &[(String, String)]) -> String {
        let mut url = format!("{}{path}", self.inner.base);
        if !query.is_empty() {
            let query: belay_http::Query = query.iter().cloned().collect();
            url.push('?');
            url.push_str(&query.to_query_string());
        }
        url
    }

    /// Sends `call` and decodes a JSON response body as `T`.
    pub(crate) fn fetch<T: FromJson>(&self, call: Call) -> Result<T> {
        let response = self.send(&call)?;
        decode(&call.endpoint(), &response.body)
    }

    /// Sends `call`, ignoring the body of a successful response.
    pub(crate) fn execute(&self, call: Call) -> Result<()> {
        self.send(&call).map(drop)
    }

    /// Sends `call` and returns the response body as text.
    pub(crate) fn fetch_text(&self, call: Call) -> Result<String> {
        let response = self.send(&call)?;
        String::from_utf8(response.body)
            .map_err(|_| DecodeError::new(call.endpoint(), "body is not valid UTF-8").into())
    }

    /// Sends `call`, retrying as its [`Safety`] allows, and returns the
    /// first successful response. Error statuses become [`Error::Api`].
    pub(crate) fn send(&self, call: &Call) -> Result<ClientResponse> {
        let inner = &*self.inner;
        let url = self.url(&call.path, &call.query);
        let options = RequestOptions {
            read_timeout: Some(inner.config.timeout.max(call.extra_wait)),
            redirects: None,
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = belay_http::Request::new(call.method.clone(), url.as_str());
            request.headers.insert(ACCEPT, call.accept);
            if let Some(auth) = &inner.auth {
                request.headers.insert(AUTHORIZATION, auth.as_str());
            }
            if let Some(body) = &call.body {
                request.headers.insert(CONTENT_TYPE, JSON);
                request.body = body.clone();
            }
            let (failure_text, delay) = match inner.http.request_with(request, &options) {
                Ok(response) if response.status.is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status.as_u16();
                    let retry_after = response
                        .header(RETRY_AFTER)
                        .and_then(|v| parse_retry_after(v, SystemTime::now()));
                    let failure = Failure::Status {
                        status,
                        retry_after,
                    };
                    match inner.retry.next_delay(attempt, call.safety, &failure) {
                        Some(delay) => (format!("HTTP {status}"), delay),
                        None => return Err(Error::Api(decode_error(status, &response.body))),
                    }
                }
                Err(e) => {
                    match inner
                        .retry
                        .next_delay(attempt, call.safety, &Failure::Transport(&e))
                    {
                        Some(delay) => (e.to_string(), delay),
                        None => return Err(Error::Transport(e)),
                    }
                }
            };
            belay_common::debug!(
                "retrying {} after attempt {attempt} failed", call.endpoint();
                reason = %failure_text, delay
            );
            std::thread::sleep(delay);
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base)
            .field("config", &self.inner.config)
            .finish()
    }
}

/// Decodes a JSON body, naming the endpoint in errors.
pub(crate) fn decode<T: FromJson>(endpoint: &str, body: &[u8]) -> Result<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(DecodeError::new(endpoint, "empty body where JSON was expected").into());
    }
    belay_json::from_bytes(body).map_err(|e| DecodeError::json(endpoint, e).into())
}

/// Percent-encodes one path segment. `:` and `@` stay readable (queue
/// names like `billing:invoices`); `/` is escaped so a name can never add
/// a segment.
pub(crate) fn segment(raw: &str) -> String {
    encode(raw, EncodeSet::Path).replace('/', "%2F")
}

/// One API call, before retries.
#[derive(Debug)]
pub(crate) struct Call {
    method: Method,
    /// Encoded path starting with `/`.
    path: String,
    query: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    safety: Safety,
    /// Added to the read timeout, for long polls.
    extra_wait: Duration,
    accept: &'static str,
}

impl Call {
    fn new(method: Method, path: String, safety: Safety) -> Call {
        Call {
            method,
            path,
            query: Vec::new(),
            body: None,
            safety,
            extra_wait: Duration::ZERO,
            accept: JSON,
        }
    }

    /// A `GET`, which is always repeatable.
    pub(crate) fn get(path: String) -> Call {
        Call::new(Method::Get, path, Safety::Repeatable)
    }

    /// A `PUT`, which replaces state and so is repeatable.
    pub(crate) fn put(path: String) -> Call {
        Call::new(Method::Put, path, Safety::Repeatable)
    }

    /// A `DELETE`, repeatable (a repeat may answer `not_found`).
    pub(crate) fn delete(path: String) -> Call {
        Call::new(Method::Delete, path, Safety::Repeatable)
    }

    /// A `POST`, whose safety depends on the endpoint.
    pub(crate) fn post(path: String, safety: Safety) -> Call {
        Call::new(Method::Post, path, safety)
    }

    /// Sets a JSON body.
    pub(crate) fn json<T: ToJson + ?Sized>(mut self, body: &T) -> Call {
        self.body = Some(encode_body(body));
        self
    }

    /// Sets the query parameters.
    pub(crate) fn query(mut self, pairs: Vec<(String, String)>) -> Call {
        self.query = pairs;
        self
    }

    /// Overrides how safe the call is to repeat.
    pub(crate) fn safety(mut self, safety: Safety) -> Call {
        self.safety = safety;
        self
    }

    /// Lets the server hold the request for up to `wait` before answering.
    pub(crate) fn long_poll(mut self, wait: Duration) -> Call {
        self.extra_wait = wait;
        self
    }

    /// Sets the `Accept` header.
    pub(crate) fn accept(mut self, media_type: &'static str) -> Call {
        self.accept = media_type;
        self
    }

    /// `METHOD /path` for error messages and logs.
    pub(crate) fn endpoint(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> Client {
        Client::new(ClientConfig::new(base)).expect("valid config")
    }

    #[test]
    fn urls_keep_the_base_prefix_and_encode_the_query() {
        let c = client("http://proxy:8080/belay/");
        assert_eq!(c.url("/v1/info", &[]), "http://proxy:8080/belay/v1/info");
        let query = vec![
            ("state".to_owned(), "dead,retrying".to_owned()),
            ("tag".to_owned(), "a&b=c d".to_owned()),
        ];
        assert_eq!(
            c.url("/v1/jobs", &query),
            "http://proxy:8080/belay/v1/jobs?state=dead,retrying&tag=a%26b%3Dc%20d"
        );
    }

    #[test]
    fn segments_cannot_escape_their_place_in_the_path() {
        assert_eq!(segment("billing:invoices"), "billing:invoices");
        assert_eq!(segment("../admin"), "..%2Fadmin");
        assert_eq!(segment("a b?c#d"), "a%20b%3Fc%23d");
        assert_eq!(segment("ü"), "%C3%BC");
    }

    #[test]
    fn empty_and_malformed_bodies_are_decode_errors() {
        let err = decode::<belay_json::Value>("GET /v1/info", b"  ").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unexpected response to GET /v1/info: empty body where JSON was expected"
        );
        let err = decode::<u32>("GET /v1/info", b"\"x\"").unwrap_err();
        assert!(matches!(err, Error::Decode(_)), "{err}");
        assert_eq!(decode::<u32>("GET /x", b"42").unwrap(), 42);
    }

    #[test]
    fn calls_describe_themselves() {
        let call = Call::post("/v1/jobs/X/cancel".to_owned(), Safety::Repeatable)
            .query(vec![("a".to_owned(), "b".to_owned())]);
        assert_eq!(call.endpoint(), "POST /v1/jobs/X/cancel");
        assert_eq!(Call::get("/v1/info".to_owned()).safety, Safety::Repeatable);
    }

    #[test]
    fn clones_share_one_configuration() {
        let a = client("http://127.0.0.1:7700");
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.inner, &b.inner));
        assert_eq!(b.base_url(), "http://127.0.0.1:7700");
        let text = format!("{a:?}");
        assert!(text.contains("base_url"), "{text}");
    }

    #[test]
    fn invalid_configuration_fails_at_construction() {
        let err = Client::new(ClientConfig::new("ftp://x")).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    #[test]
    fn client_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Client>();
    }
}
