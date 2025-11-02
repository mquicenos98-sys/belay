//! The route tree: one node per path segment.
//!
//! At each node static children are tried first, then the parameter child,
//! then the wildcard, backtracking on failure, so `/jobs/stats` beats
//! `/jobs/:id` regardless of registration order. When the path matches a
//! route that does not allow the method, the search keeps looking for
//! another match that does and only reports 405 if there is none.

use std::fmt;
use std::sync::Arc;

use crate::handler::Handler;
use crate::method::Method;

/// Why a route could not be registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// The pattern is malformed.
    InvalidPattern {
        /// The offending pattern.
        pattern: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The same method and pattern were registered twice.
    Duplicate {
        /// The method.
        method: Method,
        /// The pattern.
        pattern: String,
    },
    /// Two patterns use different parameter names at the same position
    /// (`/jobs/:id` and `/jobs/:job_id/cancel`).
    ConflictingParam {
        /// The pattern being added.
        pattern: String,
        /// The name already registered at that position.
        existing: String,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::InvalidPattern { pattern, reason } => {
                write!(f, "invalid route pattern {pattern:?}: {reason}")
            }
            RouteError::Duplicate { method, pattern } => {
                write!(f, "route {method} {pattern} is already registered")
            }
            RouteError::ConflictingParam { pattern, existing } => write!(
                f,
                "route pattern {pattern:?} conflicts with parameter :{existing} registered at the same position"
            ),
        }
    }
}

impl std::error::Error for RouteError {}

/// All handlers registered for one pattern.
pub(super) struct Route {
    pub(super) pattern: Arc<str>,
    handlers: Vec<(Method, Arc<dyn Handler>)>,
}

impl Route {
    fn new(pattern: &str) -> Route {
        Route {
            pattern: Arc::from(pattern),
            handlers: Vec::new(),
        }
    }

    fn add(
        &mut self,
        method: Method,
        handler: Arc<dyn Handler>,
        pattern: &str,
    ) -> Result<(), RouteError> {
        if self.handler_for(&method).is_some() {
            return Err(RouteError::Duplicate {
                method,
                pattern: pattern.to_owned(),
            });
        }
        self.handlers.push((method, handler));
        Ok(())
    }

    pub(super) fn handler_for(&self, method: &Method) -> Option<&Arc<dyn Handler>> {
        self.handlers
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, h)| h)
    }

    /// Whether the router can answer `method` here: registered directly,
    /// HEAD served by GET, or the automatic OPTIONS response.
    pub(super) fn allows(&self, method: &Method) -> bool {
        self.handler_for(method).is_some()
            || *method == Method::Options
            || (*method == Method::Head && self.handler_for(&Method::Get).is_some())
    }

    /// The `Allow` header value: registered methods in registration order,
    /// then the implied HEAD and OPTIONS.
    pub(super) fn allow_header(&self) -> String {
        let mut methods: Vec<&str> = self.handlers.iter().map(|(m, _)| m.as_str()).collect();
        if self.handler_for(&Method::Get).is_some() && self.handler_for(&Method::Head).is_none() {
            methods.push("HEAD");
        }
        if self.handler_for(&Method::Options).is_none() {
            methods.push("OPTIONS");
        }
        methods.join(", ")
    }
}
