//! Method + path routing with parameters, groups and middleware; the
//! matching rules are documented on [`Router`].

mod tree;

use std::fmt;
use std::sync::Arc;

use crate::handler::{Handler, Middleware, Next, Stack};
use crate::headers::names::ALLOW;
use crate::method::Method;
use crate::percent;
use crate::request::{Params, Request};
use crate::response::Response;
use crate::status::StatusCode;

pub use tree::RouteError;
use tree::{parse_pattern, Captures, Lookup, Node, Route};

/// What to do when a path differs from a route only by a trailing slash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrailingSlash {
    /// Treat `/a` and `/a/` as different paths.
    Strict,
    /// Answer `308 Permanent Redirect` to the variant that exists (308 keeps
    /// the method and body, unlike 301).
    #[default]
    Redirect,
    /// Serve the variant that exists directly.
    Ignore,
}

/// The pattern of the route that handled a request, stored in the request's
/// extensions. Useful as a low-cardinality label for metrics and logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedRoute(Arc<str>);

impl MatchedRoute {
    /// The pattern, e.g. `/v1/jobs/:id`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Dispatches requests to handlers by method and path pattern.
///
/// Patterns are made of `/`-separated segments: literals, `:name`
/// parameters (one non-empty segment) and a final `*name` wildcard (the
/// rest of the path). At each segment literals win over parameters, which
/// win over the wildcard, regardless of registration order. Matching runs
/// against the percent-decoded segments, so `/v1/queues/:name` captures
/// `a/b` from `/v1/queues/a%2Fb` without splitting on the encoded slash.
/// Dot segments are not collapsed: `/v1/../admin` simply matches nothing,
/// which keeps per-route middleware from being bypassed by path tricks.
///
/// `HEAD` is served by the `GET` handler when there is no explicit one,
/// `OPTIONS` is answered automatically with `Allow`, a path that exists
/// under other methods gets `405` with `Allow`, and anything else `404`.
///
/// ```
/// use belay_http::{Handler, Method, Request, Response, Router, StatusCode};
///
/// let mut router = Router::new();
/// router
///     .get("/v1/jobs/:id", |req: &mut Request| {
///         Response::text(format!("job {}", req.param("id").unwrap_or_default()))
///     })
///     .post("/v1/jobs/:id/cancel", |_: &mut Request| Response::no_content());
///
/// let resp = router.call(&mut Request::new(Method::Get, "/v1/jobs/01H"));
/// assert_eq!(resp.body_bytes(), Some(&b"job 01H"[..]));
///
/// let resp = router.call(&mut Request::new(Method::Delete, "/v1/jobs/01H"));
/// assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
/// assert_eq!(resp.headers.get("allow"), Some("GET, HEAD, OPTIONS"));
/// ```
///
/// Registration methods panic on invalid or conflicting patterns, since
/// those are programming errors caught at startup; [`Router::try_route`]
/// reports them as values instead.
pub struct Router {
    root: Node,
    middleware: Vec<Arc<dyn Middleware>>,
    not_found: Option<Arc<dyn Handler>>,
    method_not_allowed: Option<Arc<dyn Handler>>,
    trailing_slash: TrailingSlash,
}

impl Default for Router {
    fn default() -> Self {
        Router::new()
    }
}

impl Router {
    /// An empty router: everything is 404.
    pub fn new() -> Router {
        Router {
            root: Node::default(),
            middleware: Vec::new(),
            not_found: None,
            method_not_allowed: None,
            trailing_slash: TrailingSlash::default(),
        }
    }

    /// Registers `handler` for `method` and `pattern`.
    ///
    /// # Panics
    ///
    /// If the pattern is invalid or conflicts with an existing route.
    pub fn route(
        &mut self,
        method: Method,
        pattern: &str,
        handler: impl Handler + 'static,
    ) -> &mut Self {
        self.route_arc(method, pattern, Arc::new(handler))
    }

    /// Like [`route`](Self::route), but returns registration errors.
    pub fn try_route(
        &mut self,
        method: Method,
        pattern: &str,
        handler: impl Handler + 'static,
    ) -> Result<&mut Self, RouteError> {
        let segments = parse_pattern(pattern)?;
        self.root
            .insert(&segments, pattern, method, Arc::new(handler))?;
        Ok(self)
    }

    fn route_arc(&mut self, method: Method, pattern: &str, handler: Arc<dyn Handler>) -> &mut Self {
        let result = parse_pattern(pattern)
            .and_then(|segments| self.root.insert(&segments, pattern, method, handler));
        if let Err(err) = result {
            panic!("{err}");
        }
        self
    }

    /// Registers a `GET` route (which also answers `HEAD`).
    pub fn get(&mut self, pattern: &str, handler: impl Handler + 'static) -> &mut Self {
        self.route(Method::Get, pattern, handler)
    }

    /// Registers a `POST` route.
    pub fn post(&mut self, pattern: &str, handler: impl Handler + 'static) -> &mut Self {
        self.route(Method::Post, pattern, handler)
    }

    /// Registers a `PUT` route.
    pub fn put(&mut self, pattern: &str, handler: impl Handler + 'static) -> &mut Self {
        self.route(Method::Put, pattern, handler)
    }

    /// Registers a `DELETE` route.
    pub fn delete(&mut self, pattern: &str, handler: impl Handler + 'static) -> &mut Self {
        self.route(Method::Delete, pattern, handler)
    }

    /// Registers a `PATCH` route.
    pub fn patch(&mut self, pattern: &str, handler: impl Handler + 'static) -> &mut Self {
        self.route(Method::Patch, pattern, handler)
    }

    /// Registers routes under a common prefix, optionally with middleware
    /// that applies to those routes only (after routing, so it sees route
    /// parameters).
    ///
    /// ```
    /// use belay_http::{Handler, Method, Next, Request, Response, Router};
    ///
    /// let mut router = Router::new();
    /// router.group("/v1/admin", |admin| {
    ///     admin.wrap(|req: &mut Request, next: Next<'_>| next.run(req).header("X-Admin", "1"));
    ///     admin.post("/checkpoint", |_: &mut Request| Response::no_content());
    /// });
    /// let resp = router.call(&mut Request::new(Method::Post, "/v1/admin/checkpoint"));
    /// assert_eq!(resp.headers.get("x-admin"), Some("1"));
    /// ```
    ///
    /// # Panics
    ///
    /// If the prefix or a pattern is invalid or conflicts with an existing
    /// route.
    pub fn group(&mut self, prefix: &str, build: impl FnOnce(&mut Group)) -> &mut Self {
        let mut group = Group::new(prefix);
        build(&mut group);
        for (method, pattern, handler) in group.finish() {
            self.route_arc(method, &pattern, handler);
        }
        self
    }

    /// Adds router-wide middleware. It runs for every request, including
    /// ones that end in 404 or 405, before routing (so route parameters are
    /// not yet set when it runs, but are when `next.run` returns). The first
    /// layer added is the outermost.
    pub fn wrap(&mut self, middleware: impl Middleware + 'static) -> &mut Self {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Replaces the default `404 Not Found` response.
    pub fn not_found(&mut self, handler: impl Handler + 'static) -> &mut Self {
        self.not_found = Some(Arc::new(handler));
        self
    }

    /// Replaces the default `405 Method Not Allowed` response. The router
    /// adds the `Allow` header to whatever the handler returns.
    pub fn method_not_allowed(&mut self, handler: impl Handler + 'static) -> &mut Self {
        self.method_not_allowed = Some(Arc::new(handler));
        self
    }

    /// Sets the trailing-slash policy (default: redirect).
    pub fn trailing_slash(&mut self, policy: TrailingSlash) -> &mut Self {
        self.trailing_slash = policy;
        self
    }

    fn dispatch(&self, req: &mut Request) -> Response {
        if req.path == "*" {
            return match req.method {
                Method::Options => Response::no_content(),
                _ => Response::error(StatusCode::BAD_REQUEST, "asterisk target requires OPTIONS"),
            };
        }
        let segments = match split_path(&req.path) {
            Ok(segments) => segments,
            Err(_) => {
                return Response::error(StatusCode::BAD_REQUEST, "invalid percent-encoding in path")
            }
        };
        match self.root.lookup(&segments, &req.method) {
            Lookup::Found(route, captures) => invoke(route, captures, req),
            Lookup::MethodNotAllowed(route) => self.reject_method(route, req),
            Lookup::NotFound => self.retry_trailing_slash(req, segments),
        }
    }

    fn retry_trailing_slash(&self, req: &mut Request, mut segments: Vec<String>) -> Response {
        if self.trailing_slash == TrailingSlash::Strict || segments.is_empty() {
            return self.missing(req);
        }
        if segments.last().is_some_and(String::is_empty) {
            segments.pop();
        } else {
            segments.push(String::new());
        }
        let lookup = self.root.lookup(&segments, &req.method);
        if matches!(lookup, Lookup::NotFound) {
            return self.missing(req);
        }
        if self.trailing_slash == TrailingSlash::Redirect {
            let mut location = match req.path.strip_suffix('/') {
                Some(trimmed) => trimmed.to_owned(),
                None => format!("{}/", req.path),
            };
            if let Some((_, query)) = req.target.split_once('?') {
                location.push('?');
                location.push_str(query);
            }
            return Response::redirect(StatusCode::PERMANENT_REDIRECT, &location);
        }
        match lookup {
            Lookup::Found(route, captures) => invoke(route, captures, req),
            Lookup::MethodNotAllowed(route) => self.reject_method(route, req),
            Lookup::NotFound => self.missing(req),
        }
    }

    fn reject_method(&self, route: &Route, req: &mut Request) -> Response {
        match &self.method_not_allowed {
            Some(handler) => handler.call(req),
            None => Response::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                .header(ALLOW, route.allow_header()),
        }
    }

    fn missing(&self, req: &mut Request) -> Response {
        match &self.not_found {
            Some(handler) => handler.call(req),
            None => Response::error(StatusCode::NOT_FOUND, "not found"),
        }
    }
}

fn invoke(route: &Route, captures: Captures, req: &mut Request) -> Response {
    req.params = Params::from_vec(captures);
    req.extensions
        .insert(MatchedRoute(Arc::clone(&route.pattern)));
    if let Some(handler) = route.handler_for(&req.method) {
        return handler.call(req);
    }
    if req.method == Method::Head {
        if let Some(handler) = route.handler_for(&Method::Get) {
            // The server drops the body of responses to HEAD.
            return handler.call(req);
        }
    }
    Response::no_content().header(ALLOW, route.allow_header())
}

fn split_path(path: &str) -> Result<Vec<String>, percent::DecodeError> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed.split('/').map(percent::decode).collect()
}

/// Routing happens inside router-wide middleware, so it needs to be a
/// handler of its own.
struct Dispatch<'r>(&'r Router);

impl Handler for Dispatch<'_> {
    fn call(&self, req: &mut Request) -> Response {
        self.0.dispatch(req)
    }
}

impl Handler for Router {
    fn call(&self, req: &mut Request) -> Response {
        if self.middleware.is_empty() {
            self.dispatch(req)
        } else {
            Next::new(&self.middleware, &Dispatch(self)).run(req)
        }
    }
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("middleware", &self.middleware.len())
            .field("trailing_slash", &self.trailing_slash)
            .finish()
    }
}

type PendingRoute = (Method, String, Arc<dyn Handler>);

/// Routes sharing a prefix and middleware; see [`Router::group`].
pub struct Group {
    prefix: String,
    middleware: Vec<Arc<dyn Middleware>>,
    routes: Vec<PendingRoute>,
}
