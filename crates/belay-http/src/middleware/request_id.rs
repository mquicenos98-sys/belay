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

impl Middleware for SetRequestId {
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        let incoming = if self.trust_incoming {
            req.header(&self.header).filter(|id| is_acceptable(id))
        } else {
            None
        };
        let id = incoming.map_or_else(generate, str::to_owned);
        req.extensions.insert(RequestId(id.clone()));
        let mut response = next.run(req);
        if !response.headers.contains(&self.header) {
            response.headers.append(self.header.clone(), id);
        }
        response
    }
}

fn is_acceptable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/+=@".contains(&b))
}

/// Mints an id by pushing a per-process counter through the SplitMix64
/// finalizer. The finalizer is a bijection and the counter is offset by a
/// random per-process seed, so ids never repeat within a process and do not
/// reveal request counts.
fn generate() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: OnceLock<u64> = OnceLock::new();
    let seed = *SEED.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u32(std::process::id());
        hasher.finish()
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}",
        mix(seed.wrapping_add(n.wrapping_mul(0x9e37_79b9_7f4a_7c15)))
    )
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{Handler, Stack};
    use crate::method::Method;
    use std::collections::HashSet;

    fn app(layer: SetRequestId) -> Stack {
        Stack::new(|req: &mut Request| {
            Response::text(
                req.extensions
                    .get::<RequestId>()
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            )
        })
        .with(layer)
    }

    #[test]
    fn generates_unique_hex_ids() {
        let ids: HashSet<String> = (0..2_000).map(|_| generate()).collect();
        assert_eq!(ids.len(), 2_000);
        assert!(ids
            .iter()
            .all(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())));
    }

    #[test]
    fn replaces_malformed_incoming_ids() {
        let app = app(SetRequestId::new());
        let long = "x".repeat(129);
        for bad in ["", "has space", "line\u{2028}break", long.as_str()] {
            let mut req = Request::new(Method::Get, "/").with_header("X-Request-Id", bad);
            let resp = app.call(&mut req);
            let id = resp.headers.get("x-request-id").expect("id header");
            assert_ne!(id, bad);
            assert_eq!(id.len(), 16);
        }
    }

    #[test]
    fn untrusted_mode_ignores_incoming_ids() {
        let app = app(SetRequestId::new().trust_incoming(false));
        let mut req = Request::new(Method::Get, "/").with_header("X-Request-Id", "from-client");
        let resp = app.call(&mut req);
        assert_ne!(resp.headers.get("x-request-id"), Some("from-client"));
    }

    #[test]
    fn custom_header_name() {
        let app = app(SetRequestId::new().header("X-Trace"));
        let mut req = Request::new(Method::Get, "/").with_header("x-trace", "t-1");
        let resp = app.call(&mut req);
        assert_eq!(resp.headers.get("X-Trace"), Some("t-1"));
        assert!(!resp.headers.contains("X-Request-Id"));
    }

    #[test]
    fn handler_set_header_is_kept() {
        let app =
            Stack::new(|_: &mut Request| Response::no_content().header("X-Request-Id", "mine"))
                .with(SetRequestId::new());
        let resp = app.call(&mut Request::new(Method::Get, "/"));
        assert_eq!(
            resp.headers.get_all("x-request-id").collect::<Vec<_>>(),
            ["mine"]
        );
    }
}
