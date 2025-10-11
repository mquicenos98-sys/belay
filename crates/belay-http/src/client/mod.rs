//! A blocking HTTP/1.1 client for `http://` URLs, with a keep-alive pool,
//! stale-connection retry, opt-in redirects and an event-stream reader. The
//! behaviour is documented on [`Client`].

mod conn;

mod event_source;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::headers::names::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, COOKIE, LOCATION};
use crate::headers::Headers;
use crate::method::{Method, Version};
use crate::proto::framing::{response_framing, wants_keep_alive, BodyFraming};
use crate::request::Request;
use crate::status::StatusCode;
use crate::url::Url;

pub use conn::ResponseBody;
use conn::{connect, exchange, Conn, Outgoing, Pool};
pub use event_source::{EventStream, SseOptions};

/// Unwanted redirect bodies up to this size are read so the connection can
/// be reused.
const REDIRECT_DRAIN_LIMIT: usize = 64 * 1024;

/// Which redirects the client follows on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RedirectPolicy {
    /// Maximum redirects followed for one request; 0 (the default) returns
    /// 3xx responses to the caller.
    pub max_hops: u32,
    /// Keep `Authorization` and `Cookie` when a redirect leaves the
    /// original host and port. Off by default so credentials are not leaked
    /// to a third party; turn it on for trusted peers such as the nodes of
    /// one cluster.
    pub forward_credentials: bool,
}
