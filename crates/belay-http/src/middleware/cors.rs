//! Cross-origin resource sharing (the Fetch standard's CORS protocol), for
//! browser dashboards talking to the API from another origin.
//!
//! Preflight requests are answered directly without reaching the router;
//! actual requests get the response headers added on the way out.
//! Requests from origins that are not allowed pass through untouched: the
//! browser enforces the policy by withholding the response from the page.

use std::fmt;
use std::time::Duration;

use crate::handler::{Middleware, Next};
use crate::headers::names::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use crate::method::Method;
use crate::request::Request;
use crate::response::Response;

#[derive(Clone, Debug)]
enum Origins {
    Any,
    List(Vec<String>),
}

/// CORS middleware.
///
/// ```
/// use std::time::Duration;
/// use belay_http::middleware::Cors;
/// use belay_http::{Handler, Method, Request, Response, Stack};
///
/// let app = Stack::new(|_: &mut Request| Response::text("jobs"))
///     .with(Cors::new().allow_origin("https://dash.example").max_age(Duration::from_secs(600)));
///
/// let mut preflight = Request::new(Method::Options, "/v1/jobs")
///     .with_header("Origin", "https://dash.example")
///     .with_header("Access-Control-Request-Method", "POST");
/// let resp = app.call(&mut preflight);
/// assert_eq!(resp.status, 204);
/// assert_eq!(resp.headers.get("access-control-allow-origin"), Some("https://dash.example"));
/// assert_eq!(resp.headers.get("access-control-max-age"), Some("600"));
/// ```
#[derive(Clone)]
pub struct Cors {
    origins: Origins,
    methods: Vec<Method>,
    headers: Vec<String>,
    expose: Vec<String>,
    max_age: Option<Duration>,
    credentials: bool,
}

impl Default for Cors {
    fn default() -> Self {
        Cors::new()
    }
}

impl Cors {
    /// A policy that allows no origin until one is added, with the usual
    /// API methods and the `Content-Type`, `Authorization`, `Last-Event-ID`
    /// and `X-Request-Id` request headers.
    pub fn new() -> Self {
        Cors {
            origins: Origins::List(Vec::new()),
            methods: vec![
                Method::Get,
                Method::Head,
                Method::Post,
                Method::Put,
                Method::Patch,
                Method::Delete,
            ],
            headers: [
                "Content-Type",
                "Authorization",
                "Last-Event-ID",
                "X-Request-Id",
            ]
            .iter()
            .map(|h| (*h).to_owned())
            .collect(),
            expose: Vec::new(),
            max_age: Some(Duration::from_secs(600)),
            credentials: false,
        }
    }

    /// [`Cors::new`] with any origin allowed.
    pub fn permissive() -> Self {
        Cors::new().allow_any_origin()
    }

    /// Allows `origin` (e.g. `https://dash.example:8443`); `*` allows all.
    #[must_use]
    pub fn allow_origin(mut self, origin: &str) -> Self {
        if origin == "*" {
            return self.allow_any_origin();
        }
        if let Origins::List(list) = &mut self.origins {
            list.push(origin.trim_end_matches('/').to_owned());
        }
        self
    }

    /// Allows every origin.
    #[must_use]
    pub fn allow_any_origin(mut self) -> Self {
        self.origins = Origins::Any;
        self
    }

    /// Replaces the allowed methods.
    #[must_use]
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.methods = methods.into_iter().collect();
        self
    }

    /// Replaces the allowed request headers.
    #[must_use]
    pub fn allow_headers<S: Into<String>>(mut self, headers: impl IntoIterator<Item = S>) -> Self {
        self.headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Response headers scripts may read beyond the CORS-safelisted ones.
    #[must_use]
    pub fn expose_headers<S: Into<String>>(mut self, headers: impl IntoIterator<Item = S>) -> Self {
        self.expose = headers.into_iter().map(Into::into).collect();
        self
    }

    /// How long browsers may cache a preflight result (`None` omits the
    /// header).
    #[must_use]
    pub fn max_age(mut self, max_age: impl Into<Option<Duration>>) -> Self {
        self.max_age = max_age.into();
        self
    }

    /// Whether requests may carry credentials (cookies, HTTP auth). The
    /// allowed origin is then always echoed, never `*`, as the protocol
    /// requires.
    #[must_use]
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    fn allows(&self, origin: &str) -> bool {
        match &self.origins {
            Origins::Any => true,
            Origins::List(list) => list.iter().any(|o| o.eq_ignore_ascii_case(origin)),
        }
    }

    fn origin_value(&self, origin: &str) -> String {
        match self.origins {
            Origins::Any if !self.credentials => "*".to_owned(),
            _ => origin.to_owned(),
        }
    }

    fn decorate(&self, response: &mut Response, origin_value: &str) {
        response
            .headers
            .insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
        if self.credentials {
            response
                .headers
                .insert(ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
        }
        if origin_value != "*" && !response.headers.has_token(VARY, "origin") {
            // The response differs per origin; caches must key on it.
            response.headers.append(VARY, "Origin");
        }
    }
}

impl fmt::Debug for Cors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cors")
            .field("origins", &self.origins)
            .field("methods", &self.methods)
            .field("credentials", &self.credentials)
            .finish()
    }
}
