//! The [`Handler`] and [`Middleware`] traits and their composition.
//!
//! A handler turns a request into a response. Middleware wraps a handler:
//! it receives the request together with a [`Next`] that runs the rest of
//! the chain, so it can act before, after, or instead of the inner handler.
//! Both traits are implemented for closures with the right signature.

use std::fmt;
use std::sync::Arc;

use crate::request::Request;
use crate::response::Response;

/// Something that answers requests.
///
/// Handlers are shared across connection threads, hence `Send + Sync`, and
/// take `&self`; keep mutable state behind a lock or atomics.
///
/// ```
/// use belay_http::{Handler, Method, Request, Response};
///
/// let hello = |req: &mut Request| Response::text(format!("{} {}", req.method, req.path));
/// let resp = hello.call(&mut Request::new(Method::Get, "/ping"));
/// assert_eq!(resp.body_bytes(), Some(&b"GET /ping"[..]));
/// ```
pub trait Handler: Send + Sync {
    /// Produces the response for `req`.
    fn call(&self, req: &mut Request) -> Response;
}

impl<F> Handler for F
where
    F: Fn(&mut Request) -> Response + Send + Sync,
{
    fn call(&self, req: &mut Request) -> Response {
        self(req)
    }
}

/// A layer around a handler.
///
/// ```
/// use belay_http::{Handler, Method, Next, Request, Response, Stack, StatusCode};
///
/// // Rejects requests without a token, otherwise continues down the chain.
/// let auth = |req: &mut Request, next: Next<'_>| {
///     if req.bearer_token().is_none() {
///         return Response::error(StatusCode::UNAUTHORIZED, "missing token");
///     }
///     next.run(req)
/// };
/// let app = Stack::new(|_: &mut Request| Response::text("secret")).with(auth);
/// assert_eq!(app.call(&mut Request::new(Method::Get, "/")).status, 401);
/// ```
pub trait Middleware: Send + Sync {
    /// Handles `req`, typically by calling `next.run(req)` somewhere.
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response;
}
