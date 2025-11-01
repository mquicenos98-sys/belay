//! Request ids: accept a well-formed incoming id or mint one, expose it to
//! handlers through the extensions, and echo it on the response.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::handler::{Middleware, Next};
use crate::headers::names::X_REQUEST_ID;
use crate::request::Request;
use crate::response::Response;

/// The id assigned to a request, stored in its extensions by
/// [`SetRequestId`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Middleware that assigns every request a [`RequestId`].
///
/// An incoming id (by default from `X-Request-Id`) is reused when it is 1 to
/// 128 characters of `[A-Za-z0-9-_.:/+=@]`, so ids set by a proxy or a
/// calling service carry through; anything else is replaced, which keeps
/// log injection and oversized values out. Generated ids are 16 hex digits,
/// unique within the process.
///
/// ```
/// use belay_http::middleware::{RequestId, SetRequestId};
/// use belay_http::{Handler, Method, Request, Response, Stack};
///
/// let app = Stack::new(|req: &mut Request| {
///     let id = req.extensions.get::<RequestId>().map(|id| id.to_string());
///     Response::text(id.unwrap_or_default())
/// })
/// .with(SetRequestId::new());
///
/// let mut req = Request::new(Method::Get, "/").with_header("X-Request-Id", "abc-123");
/// let resp = app.call(&mut req);
/// assert_eq!(resp.headers.get("x-request-id"), Some("abc-123"));
/// assert_eq!(resp.body_bytes(), Some(&b"abc-123"[..]));
/// ```
#[derive(Clone, Debug)]
pub struct SetRequestId {
    header: String,
    trust_incoming: bool,
}

impl Default for SetRequestId {
    fn default() -> Self {
        SetRequestId::new()
    }
}

impl SetRequestId {
    /// Uses `X-Request-Id` and trusts well-formed incoming ids.
    pub fn new() -> Self {
        SetRequestId {
            header: X_REQUEST_ID.to_owned(),
            trust_incoming: true,
        }
    }

    /// Reads and writes the id under a different header name.
    #[must_use]
    pub fn header(mut self, name: &str) -> Self {
        self.header = name.to_owned();
        self
    }

    /// Whether to reuse incoming ids (turn off at an untrusted edge).
    #[must_use]
    pub fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }
}
