//! HTTP/1.1 server and client built directly on `std::net`, with no async
//! runtime and no dependencies.
//!
//! The server gives every connection its own thread from an elastic pool
//! capped at [`ServerConfig::max_connections`]; see the [`server`](Server)
//! docs for why that model suits long-polling and event streams. Requests
//! are routed with [`Router`], wrapped in [`Middleware`], and answered with
//! [`Response`]s whose bodies may be streamed (see [`SseWriter`]). The
//! [`Client`] pools keep-alive connections, retries idempotent requests on
//! stale connections, follows redirects when asked to, and consumes event
//! streams through [`EventStream`].
//!
//! ```
//! use belay_http::{Client, ClientConfig, Request, Response, Router, Server, ServerConfig};
//!
//! let mut router = Router::new();
//! router.get("/hello/:name", |req: &mut Request| {
//!     Response::text(format!("hello, {}", req.param("name").unwrap_or("stranger")))
//! });
//! let server = Server::bind("127.0.0.1:0", ServerConfig::default())?.spawn(router)?;
//!
//! let client = Client::new(ClientConfig::default());
//! let url = format!("http://{}/hello/belay", server.local_addr());
//! let response = client.get(&url, &[])?;
//! assert_eq!(response.status, 200);
//! assert_eq!(response.text()?, "hello, belay");
//!
//! server.shutdown_and_join();
//! # Ok::<(), belay_http::Error>(())
//! ```
//!
//! Only plain `http://` is spoken: there is no TLS. Deploy behind a
//! TLS-terminating proxy, or on a trusted network.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod buf;
mod client;
mod date;
mod error;
mod extensions;
mod handler;
mod headers;
mod method;
pub mod middleware;
pub mod percent;
mod proto;
mod query;
mod request;
mod response;
mod router;
mod server;
mod sse;
mod status;
mod sync;
mod url;

pub use client::{
    Client, ClientConfig, ClientResponse, EventStream, RedirectPolicy, RequestOptions,
    ResponseBody, SseOptions, StreamingResponse,
};
pub use date::{fmt_http_date, parse_http_date};
pub use error::{Error, Result};
pub use extensions::Extensions;
pub use handler::{Handler, Middleware, Next, Stack};
pub use headers::names as header;
pub use headers::{is_valid_name, is_valid_value, Headers};
pub use method::{Method, Version};
pub use proto::ParseError;
pub use query::{Query, QueryError, QueryErrorKind};
pub use request::{Params, Request};
pub use response::{Body, BodyWriter, Response, StreamBody};
pub use router::{Group, MatchedRoute, RouteError, Router, TrailingSlash};
pub use server::{Drain, ErrorHook, Server, ServerConfig, ServerError, ServerHandle, ServerStats};
pub use sse::{SseEvent, SseParser, SseWriter, DEFAULT_HEARTBEAT};
pub use status::StatusCode;
pub use sync::ShutdownSignal;
pub use url::{default_port, Url, UrlError};
