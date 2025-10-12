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

impl RedirectPolicy {
    /// Never follow redirects.
    pub const fn none() -> Self {
        RedirectPolicy {
            max_hops: 0,
            forward_credentials: false,
        }
    }

    /// Follow up to `max_hops` redirects.
    pub const fn limited(max_hops: u32) -> Self {
        RedirectPolicy {
            max_hops,
            forward_credentials: false,
        }
    }

    /// Sets [`forward_credentials`](Self::forward_credentials).
    #[must_use]
    pub const fn with_forwarded_credentials(mut self, forward: bool) -> Self {
        self.forward_credentials = forward;
        self
    }
}

/// Client settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Time allowed to establish a TCP connection (per resolved address).
    /// Default 5 s.
    pub connect_timeout: Duration,
    /// Longest wait for the next bytes of a response; `None` waits forever.
    /// Must exceed any server-side long-poll wait. Default 30 s.
    pub read_timeout: Option<Duration>,
    /// Longest a write may block; `None` waits forever. Default 30 s.
    pub write_timeout: Option<Duration>,
    /// Idle connections kept per host and port; 0 disables pooling (every
    /// request then asks the server to close). Default 8.
    pub max_idle_per_host: usize,
    /// How long an idle connection stays in the pool. Keep this below the
    /// server's keep-alive timeout (5 s for a belay server) so the pool
    /// rarely offers a connection the server is about to close. Default 4 s.
    pub idle_timeout: Duration,
    /// `User-Agent` sent unless a request sets its own; empty sends none.
    pub user_agent: String,
    /// Redirect handling. Default: not followed.
    pub redirects: RedirectPolicy,
    /// Largest response body [`Client::request`] reads into memory.
    /// Default 64 MiB.
    pub max_response_bytes: u64,
    /// Largest response head accepted. Default 64 KiB.
    pub max_head_bytes: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            connect_timeout: Duration::from_secs(5),
            read_timeout: Some(Duration::from_secs(30)),
            write_timeout: Some(Duration::from_secs(30)),
            max_idle_per_host: 8,
            idle_timeout: Duration::from_secs(4),
            user_agent: concat!("belay-http/", env!("CARGO_PKG_VERSION")).to_owned(),
            redirects: RedirectPolicy::none(),
            max_response_bytes: 64 * 1024 * 1024,
            max_head_bytes: 64 * 1024,
        }
    }
}

/// Per-request overrides of [`ClientConfig`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// Replaces [`ClientConfig::read_timeout`] (e.g. for a long-poll that
    /// waits longer than usual).
    pub read_timeout: Option<Duration>,
    /// Replaces [`ClientConfig::redirects`].
    pub redirects: Option<RedirectPolicy>,
}
