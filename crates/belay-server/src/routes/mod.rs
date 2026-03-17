//! The route table: every endpoint of the v1 API with the access it
//! requires, and the router built from it.
//!
//! The table is data so that the scope of every route is visible in one
//! place and checked by tests, rather than scattered over handlers. The
//! modules below hold the handlers, one per resource.

mod admin;

mod calendars;

mod cluster;

mod events;

mod jobs;

mod queues;

mod schedules;

mod system;

mod work;

mod workers;

mod workflows;

use std::fmt;
use std::sync::Arc;

use belay_core::config::Scope;
use belay_http::middleware::SetRequestId;
use belay_http::{Method, Router};

use crate::app::{App, Endpoint, HandlerFn};
use crate::auth::Access;
use crate::guard::{catch_panics, ApiErrors, BodyLimit};
use crate::observe::Observe;

/// The methods the API uses; `HEAD` and `OPTIONS` are answered by the
/// router for every `GET` route and every path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Get,
    Post,
    Put,
    Delete,
}

impl Verb {
    fn method(self) -> Method {
        match self {
            Verb::Get => Method::Get,
            Verb::Post => Method::Post,
            Verb::Put => Method::Put,
            Verb::Delete => Method::Delete,
        }
    }
}

/// One endpoint: method, path pattern, and who may call it.
#[derive(Clone, Copy)]
pub struct Route {
    verb: Verb,
    pattern: &'static str,
    access: Access,
    handler: HandlerFn,
}

impl Route {
    const fn new(verb: Verb, pattern: &'static str, access: Access, handler: HandlerFn) -> Route {
        Route {
            verb,
            pattern,
            access,
            handler,
        }
    }

    /// The method.
    #[must_use]
    pub fn method(&self) -> Method {
        self.verb.method()
    }

    /// The path pattern, e.g. `/v1/jobs/:id/cancel`.
    #[must_use]
    pub fn pattern(&self) -> &'static str {
        self.pattern
    }

    /// What a caller needs.
    #[must_use]
    pub fn access(&self) -> Access {
        self.access
    }
}
