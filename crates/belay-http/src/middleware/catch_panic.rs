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

impl Middleware for CatchPanic {
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        // The request may be half-modified after a panic; it is only read
        // (by the hook) and then dropped, so observing that state is fine.
        match catch_unwind(AssertUnwindSafe(|| next.run(req))) {
            Ok(response) => response,
            Err(payload) => {
                if let Some(hook) = &self.on_panic {
                    hook(req, &panic_message(payload.as_ref()));
                }
                match &self.respond {
                    Some(make) => make(),
                    None => {
                        Response::error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
                    }
                }
            }
        }
    }
}

/// Extracts the message from a panic payload (`&str` or `String` for
/// `panic!` with a literal or a format string).
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-string payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{Handler, Stack};
    use crate::method::Method;
    use std::sync::Mutex;

    #[test]
    fn converts_panics_to_500_and_reports_them() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hook_seen = Arc::clone(&seen);
        let app = Stack::new(|req: &mut Request| -> Response {
            panic!("boom on {}", req.path);
        })
        .with(CatchPanic::new().on_panic(move |req, msg| {
            hook_seen
                .lock()
                .expect("test lock")
                .push(format!("{} {}: {msg}", req.method, req.path));
        }));
        let resp = app.call(&mut Request::new(Method::Post, "/v1/jobs"));
        assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.body_bytes(), Some(&b"internal server error\n"[..]));
        assert_eq!(
            *seen.lock().expect("test lock"),
            ["POST /v1/jobs: boom on /v1/jobs"]
        );
    }

    #[test]
    fn passes_normal_responses_through() {
        let app = Stack::new(|_: &mut Request| Response::text("fine")).with(CatchPanic::new());
        assert_eq!(app.call(&mut Request::new(Method::Get, "/")).status, 200);
    }

    #[test]
    fn extracts_messages_from_common_payloads() {
        let literal: Box<dyn Any + Send> = Box::new("static");
        let formatted: Box<dyn Any + Send> = Box::new(String::from("formatted 7"));
        let other: Box<dyn Any + Send> = Box::new(42u8);
        assert_eq!(panic_message(literal.as_ref()), "static");
        assert_eq!(panic_message(formatted.as_ref()), "formatted 7");
        assert_eq!(
            panic_message(other.as_ref()),
            "panic with a non-string payload"
        );
    }
}
