//! State shared by every handler, and [`Endpoint`], the wrapper that runs
//! one route's handler behind its access check and rate limit.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use belay_common::warn;
use belay_core::config::AuthConfig;
use belay_core::Engine;
use belay_http::{Handler, Request, Response, ShutdownSignal};
use belay_proto::api::ApiError;

use crate::auth::{self, Access};
use crate::observe::HttpMetrics;
use crate::options::ServerOptions;
use crate::rate_limit::RateLimiter;
use crate::reply;

/// A route handler: the request in, a response or an error out.
pub(crate) type HandlerFn = fn(&App, &mut Request) -> Result<Response, ApiError>;

/// What the handlers share.
pub(crate) struct App {
    pub(crate) engine: Arc<Engine>,
    pub(crate) options: ServerOptions,
    pub(crate) metrics: Option<HttpMetrics>,
    limiter: RateLimiter,
}
