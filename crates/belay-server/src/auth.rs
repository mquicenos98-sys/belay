//! Who may call what: each route declares an [`Access`], and the endpoint
//! checks the bearer token against it before running.
//!
//! The comparison itself lives in belay-core
//! ([`AuthConfig::authorize`]): tokens are hashed and compared with every
//! configured digest in constant time. This module turns its verdicts into
//! responses, with the `WWW-Authenticate` challenge RFC 6750 asks for, so
//! a client can tell a missing token (`401`, no error), a wrong one (`401`,
//! `invalid_token`) and a missing scope (`403`, `insufficient_scope`) apart
//! without parsing the message.

use std::fmt;

use belay_core::config::{AuthConfig, Principal, Scope};
use belay_http::header::AUTHORIZATION;
use belay_http::{Request, Response};
use belay_proto::api::{ApiError, ErrorCode};

use crate::reply;

/// The `WWW-Authenticate` header.
pub(crate) const WWW_AUTHENTICATE: &str = "WWW-Authenticate";

/// What a route requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Nothing: probes a load balancer or orchestrator must reach without
    /// credentials (`/healthz`, `/readyz`).
    Public,
    /// A token granting this scope (or `admin`), when authentication is
    /// enforced.
    Token(Scope),
}
