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

impl<F> Middleware for F
where
    F: Fn(&mut Request, Next<'_>) -> Response + Send + Sync,
{
    fn handle(&self, req: &mut Request, next: Next<'_>) -> Response {
        self(req, next)
    }
}

/// The remainder of a middleware chain.
///
/// Consumed by [`run`](Next::run), so a middleware can call the rest of the
/// chain at most once.
pub struct Next<'a> {
    chain: &'a [Arc<dyn Middleware>],
    endpoint: &'a dyn Handler,
}

impl<'a> Next<'a> {
    pub(crate) fn new(chain: &'a [Arc<dyn Middleware>], endpoint: &'a dyn Handler) -> Self {
        Next { chain, endpoint }
    }

    /// Runs the remaining middleware and then the handler.
    pub fn run(self, req: &mut Request) -> Response {
        match self.chain.split_first() {
            Some((first, rest)) => first.handle(req, Next::new(rest, self.endpoint)),
            None => self.endpoint.call(req),
        }
    }
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("remaining_middleware", &self.chain.len())
            .finish()
    }
}

/// A handler wrapped in middleware; itself a [`Handler`].
///
/// Layers run in the order they were added: the first `with` is the
/// outermost, seeing the request first and the response last.
#[derive(Clone)]
pub struct Stack {
    middleware: Vec<Arc<dyn Middleware>>,
    endpoint: Arc<dyn Handler>,
}

impl Stack {
    /// A stack with no middleware yet.
    pub fn new(endpoint: impl Handler + 'static) -> Stack {
        Stack::from_arc(Arc::new(endpoint))
    }

    pub(crate) fn from_arc(endpoint: Arc<dyn Handler>) -> Stack {
        Stack {
            middleware: Vec::new(),
            endpoint,
        }
    }

    /// Adds a layer inside the ones already added.
    #[must_use]
    pub fn with(mut self, middleware: impl Middleware + 'static) -> Stack {
        self.middleware.push(Arc::new(middleware));
        self
    }

    pub(crate) fn with_arc(mut self, middleware: Arc<dyn Middleware>) -> Stack {
        self.middleware.push(middleware);
        self
    }
}
