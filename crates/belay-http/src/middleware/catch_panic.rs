//! Turns handler panics into `500` responses.
//!
//! The server already contains panics per request (the connection gets a
//! bare 500 and the worker survives), but this layer runs inside the rest
//! of the middleware stack: outer layers such as the access log and request
//! id still see a normal response, and the application chooses the body
//! (for belay, a JSON `ApiError`) and gets a hook to log the panic.

use std::any::Any;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::handler::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use crate::status::StatusCode;

type PanicHook = Arc<dyn Fn(&Request, &str) + Send + Sync>;

type MakeResponse = Arc<dyn Fn() -> Response + Send + Sync>;

/// Middleware that catches panics from inner layers and the handler.
///
/// The process-wide panic hook still runs first (by default printing the
/// message to stderr); install one with `std::panic::set_hook` to route it
/// elsewhere.
///
/// ```
/// use belay_http::middleware::CatchPanic;
/// use belay_http::{Handler, Method, Request, Response, Stack};
///
/// let app = Stack::new(|_: &mut Request| -> Response { panic!("handler bug") })
///     .with(CatchPanic::new().respond_with(|| Response::json_bytes(r#"{"code":"internal"}"#)
///         .with_status(belay_http::StatusCode::INTERNAL_SERVER_ERROR)));
/// # let previous = std::panic::take_hook();
/// # std::panic::set_hook(Box::new(|_| {}));
/// let resp = app.call(&mut Request::new(Method::Get, "/"));
/// # std::panic::set_hook(previous);
/// assert_eq!(resp.status, 500);
/// assert_eq!(resp.headers.get("content-type"), Some("application/json"));
/// ```
#[derive(Clone, Default)]
pub struct CatchPanic {
    on_panic: Option<PanicHook>,
    respond: Option<MakeResponse>,
}

impl CatchPanic {
    /// Answers panics with a plain-text `500 Internal Server Error`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `hook` with the request and the panic message.
    #[must_use]
    pub fn on_panic(mut self, hook: impl Fn(&Request, &str) + Send + Sync + 'static) -> Self {
        self.on_panic = Some(Arc::new(hook));
        self
    }

    /// Uses `make` to build the response sent after a panic.
    #[must_use]
    pub fn respond_with(mut self, make: impl Fn() -> Response + Send + Sync + 'static) -> Self {
        self.respond = Some(Arc::new(make));
        self
    }
}

impl fmt::Debug for CatchPanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatchPanic")
            .field("on_panic", &self.on_panic.is_some())
            .field("custom_response", &self.respond.is_some())
            .finish()
    }
}
