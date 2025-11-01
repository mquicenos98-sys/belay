//! Request ids: accept a well-formed incoming id or mint one, expose it to
//! handlers through the extensions, and echo it on the response.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::handler::{Middleware, Next};
use crate::headers::names::X_REQUEST_ID;
use crate::request::Request;
use crate::response::Response;

/// The id assigned to a request, stored in its extensions by
/// [`SetRequestId`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
