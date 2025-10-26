//! The request type shared by handlers (server side) and the client.

use std::fmt;
use std::net::SocketAddr;

use crate::error::{Error, Result};
use crate::extensions::Extensions;
use crate::headers::names::{AUTHORIZATION, CONTENT_TYPE, LAST_EVENT_ID};
use crate::headers::Headers;
use crate::method::{Method, Version};
use crate::proto::parse::{starts_with_ignore_case, RequestHead};
use crate::query::Query;

/// Route parameters captured by the [`Router`](crate::Router), in pattern
/// order, percent-decoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    entries: Vec<(String, String)>,
}

impl Params {
    pub(crate) fn from_vec(entries: Vec<(String, String)>) -> Self {
        Params { entries }
    }

    /// The value captured for `name` (`:name` or `*name` in the pattern).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// `(name, value)` pairs in pattern order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Number of captures.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was captured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// An HTTP request.
///
/// On the server, every field is filled from the wire before the handler
/// runs; the body has been read completely (bounded by
/// [`ServerConfig::max_body_bytes`](crate::ServerConfig::max_body_bytes)).
/// The server also stores a [`ShutdownSignal`](crate::ShutdownSignal) in
/// [`extensions`](Request::extensions).
///
/// For the client, build one with [`Request::new`] and an absolute URL as
/// the target.
///
/// ```
/// use belay_http::{Method, Request};
///
/// let req = Request::new(Method::Post, "/v1/jobs?dry_run=1")
///     .with_header("Content-Type", "application/json; charset=utf-8")
///     .with_body(r#"{"queue":"emails"}"#);
/// assert_eq!(req.path, "/v1/jobs");
/// assert_eq!(req.query.get("dry_run"), Some("1"));
/// assert_eq!(req.content_type(), Some("application/json"));
/// ```
pub struct Request {
    /// The method.
    pub method: Method,
    /// The request target exactly as received (or as given to
    /// [`Request::new`]).
    pub target: String,
    /// The protocol version of the request.
    pub version: Version,
    /// The path component of the target, still percent-encoded.
    pub path: String,
    /// The decoded query parameters.
    pub query: Query,
    /// The header fields.
    pub headers: Headers,
    /// The complete body.
    pub body: Vec<u8>,
    /// Route captures, filled in by the router.
    pub params: Params,
    /// The peer address, when the request came from a socket.
    pub remote_addr: Option<SocketAddr>,
    /// Per-request data attached by the server and by middleware.
    pub extensions: Extensions,
}

impl Request {
    /// A request for `target`, which may be origin-form (`/path?query`) or
    /// an absolute URL. Path and query are derived from it.
    pub fn new(method: Method, target: impl Into<String>) -> Request {
        let target = target.into();
        let (path, query) = split_target(&target);
        Request {
            method,
            path,
            query: Query::parse(query),
            target,
            version: Version::Http11,
            headers: Headers::new(),
            body: Vec::new(),
            params: Params::default(),
            remote_addr: None,
            extensions: Extensions::new(),
        }
    }

    pub(crate) fn from_head(head: RequestHead, body: Vec<u8>, remote: SocketAddr) -> Request {
        let mut req = Request::new(head.method, head.target);
        req.version = head.version;
        req.headers = head.headers;
        req.body = body;
        req.remote_addr = Some(remote);
        req
    }

    /// Adds a header field (builder style).
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Replaces the body (builder style).
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The first value of header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// The route parameter `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name)
    }

    /// The body as UTF-8 text.
    pub fn body_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.body).map_err(Error::Utf8)
    }

    /// The media type from `Content-Type`, without parameters, trimmed
    /// (`application/json; charset=utf-8` → `application/json`).
    pub fn content_type(&self) -> Option<&str> {
        self.header(CONTENT_TYPE)
            .map(|v| v.split(';').next().unwrap_or("").trim())
            .filter(|v| !v.is_empty())
    }

    /// The token from an `Authorization: Bearer <token>` header. The scheme
    /// name is matched case-insensitively (RFC 9110 §11.1).
    pub fn bearer_token(&self) -> Option<&str> {
        let value = self.header(AUTHORIZATION)?;
        let (scheme, token) = value.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
    }

    /// The `Last-Event-ID` header an `EventSource` client sends when it
    /// reconnects.
    pub fn last_event_id(&self) -> Option<&str> {
        self.header(LAST_EVENT_ID)
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("version", &self.version)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("params", &self.params)
            .field("remote_addr", &self.remote_addr)
            .finish()
    }
}

/// Splits a request target into its (still encoded) path and raw query.
fn split_target(target: &str) -> (String, &str) {
    let mut rest = target;
    for scheme in ["http://", "https://"] {
        if starts_with_ignore_case(rest, scheme) {
            let after = &rest[scheme.len()..];
            rest = after.find(&['/', '?'][..]).map_or("", |i| &after[i..]);
            break;
        }
    }
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = if path.is_empty() { "/" } else { path };
    (path.to_owned(), query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_origin_form_targets() {
        let req = Request::new(Method::Get, "/v1/jobs/01H%20X?limit=5&state=ready");
        assert_eq!(req.path, "/v1/jobs/01H%20X");
        assert_eq!(req.query.get("limit"), Some("5"));
        assert_eq!(req.query.len(), 2);
    }

    #[test]
    fn splits_absolute_form_targets() {
        let req = Request::new(Method::Get, "HTTP://example.com:8080/a/b?c=d#frag");
        assert_eq!(req.path, "/a/b");
        assert_eq!(req.query.get("c"), Some("d"));
        let bare = Request::new(Method::Get, "http://example.com");
        assert_eq!(bare.path, "/");
        let query_only = Request::new(Method::Get, "http://example.com?x=1");
        assert_eq!(query_only.path, "/");
        assert_eq!(query_only.query.get("x"), Some("1"));
    }

    #[test]
    fn asterisk_target_keeps_its_path() {
        assert_eq!(Request::new(Method::Options, "*").path, "*");
    }

    #[test]
    fn bearer_token_parsing() {
        let req = Request::new(Method::Get, "/").with_header("Authorization", "bearer  s3cret ");
        assert_eq!(req.bearer_token(), Some("s3cret"));
        let basic = Request::new(Method::Get, "/").with_header("Authorization", "Basic abc");
        assert_eq!(basic.bearer_token(), None);
        let empty = Request::new(Method::Get, "/").with_header("Authorization", "Bearer ");
        assert_eq!(empty.bearer_token(), None);
    }

    #[test]
    fn content_type_strips_parameters() {
        let req = Request::new(Method::Post, "/").with_header("content-type", " text/plain ;q=1");
        assert_eq!(req.content_type(), Some("text/plain"));
        let blank = Request::new(Method::Post, "/").with_header("content-type", ";x");
        assert_eq!(blank.content_type(), None);
    }

    #[test]
    fn body_str_reports_invalid_utf8() {
        let req = Request::new(Method::Post, "/").with_body(vec![0xff, 0xfe]);
        assert!(matches!(req.body_str(), Err(Error::Utf8(_))));
        let ok = Request::new(Method::Post, "/").with_body("héllo");
        assert_eq!(ok.body_str().ok(), Some("héllo"));
    }

    #[test]
    fn params_lookup() {
        let params = Params::from_vec(vec![
            ("id".into(), "7".into()),
            ("rest".into(), "a/b".into()),
        ]);
        assert_eq!(params.get("rest"), Some("a/b"));
        assert_eq!(params.get("missing"), None);
        assert_eq!(params.iter().count(), 2);
    }
}
