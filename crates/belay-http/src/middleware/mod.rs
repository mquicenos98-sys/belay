//! Ready-made middleware: request ids, access logging, panic recovery and
//! CORS.
//!
//! A typical stack, outermost first:
//!
//! ```
//! use belay_http::middleware::{AccessLog, CatchPanic, Cors, SetRequestId};
//! use belay_http::{Request, Response, Router};
//!
//! let mut router = Router::new();
//! router
//!     .wrap(SetRequestId::new())
//!     .wrap(AccessLog::new(|entry| eprintln!("{entry}")))
//!     .wrap(CatchPanic::new())
//!     .wrap(Cors::permissive())
//!     .get("/healthz", |_: &mut Request| Response::text("ok"));
//! ```
//!
//! Request ids go first so every later layer (and the log line) can see
//! them; the panic catcher sits inside the access log so panics are logged
//! as the 500 they turn into.

mod access_log;
mod catch_panic;
mod cors;
mod request_id;

pub use access_log::{AccessEntry, AccessLog};
pub(crate) use catch_panic::panic_message;
pub use catch_panic::CatchPanic;
pub use cors::Cors;
pub use request_id::{RequestId, SetRequestId};
