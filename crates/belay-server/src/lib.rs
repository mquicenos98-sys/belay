//! The belay HTTP/JSON API server: every endpoint of the v1 API over a
//! [`belay_core::Engine`], with bearer-token scopes, per-token rate
//! limits, request ids and access logs, Prometheus metrics, a
//! server-sent event stream, long-polling leases, cluster redirects and a
//! graceful shutdown.
//!
//! ```
//! use std::net::TcpListener;
//! use std::sync::Arc;
//! use belay_client::{Client, ClientConfig};
//! use belay_core::{Config, Engine};
//! use belay_proto::{JobSpec, JobState};
//!
//! let engine = Arc::new(Engine::start(Config::dev())?);
//! let server = belay_server::serve_on(engine, TcpListener::bind("127.0.0.1:0")?)?;
//!
//! let client = Client::new(ClientConfig::new(format!("http://{}", server.local_addr())))?;
//! let created = client.jobs().enqueue(&JobSpec::new("emails", "send_welcome"))?;
//! assert_eq!(client.jobs().get(created.id)?.state, JobState::Ready);
//!
//! server.shutdown_and_join(); // drains, then shuts the engine down
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`serve`] binds `server.listen` from the engine's configuration,
//! [`serve_on`] takes a bound listener, and [`router`] returns the routes
//! with their middleware for embedding in another belay-http server (or
//! for calling in tests without a socket). The `_with` variants take
//! [`ServerOptions`].
//!
//! # Endpoints and scopes
//!
//! When authentication is enforced (`[[auth.tokens]]` configured, and
//! `auth.disabled` not set), every route except the two probes needs an
//! `Authorization: Bearer <token>` header whose token grants the scope
//! below; `admin` grants every scope. The full table is also available as
//! data from [`routes`].
//!
//! | scope | routes |
//! |---|---|
//! | none | `GET /healthz`, `GET /readyz` |
//! | `read` | every `GET` below except `/v1/admin/*`: `/metrics`, `/v1/info`, `/v1/jobs[/:id]`, `/v1/queues[/:name]`, `/v1/schedules[/:id]`, `/v1/schedules/:id/preview`, `/v1/calendars[/:name]`, `/v1/workflows[/:id]`, `/v1/workers`, `/v1/events`, `/v1/cluster/members` |
//! | `enqueue` | `POST /v1/jobs`, `POST /v1/jobs/:id/cancel`, `POST /v1/workflows`, `POST /v1/workflows/:id/cancel` |
//! | `work` | `POST /v1/lease`, `POST /v1/jobs/:id/heartbeat`, `/complete`, `/fail` |
//! | `admin` | everything else: `DELETE /v1/jobs/:id`, `POST /v1/jobs/:id/retry`, `/promote`; every queue, schedule and calendar change; `/v1/admin/checkpoint`, `/store`, `/verify`, `/shutdown` |
//!
//! Producers may withdraw what they submitted (cancelling a job or a
//! workflow run) but not operate the system: retrying and promoting jobs
//! change what runs when, and deleting erases history, so those are
//! `admin` along with queue, schedule and calendar changes.
//!
//! Failed checks answer `401` (no token, or an unknown one) or `403` (the
//! token lacks the scope) with a `WWW-Authenticate: Bearer` challenge per
//! RFC 6750. The token's name is recorded as the request's
//! [`Principal`](belay_core::config::Principal) (in the request
//! extensions) and appears in the access log.
//!
//! # Errors
//!
//! Every failure has an [`ApiError`](belay_proto::api::ApiError) JSON body
//! and the status its code maps to, including the router's own answers:
//! unknown paths are `404 not_found`, a wrong method is `405` with `Allow`
//! and an `invalid_argument` body, a body over `server.max_body` is `413`
//! with `invalid_argument`. Unknown query parameters are `400`, never
//! silently ignored. `503` and `429` answers carry `Retry-After`; once the
//! engine has begun shutting down, every route but the probes answers
//! `503 unavailable`.
//!
//! The exceptions are answers the HTTP layer gives before a request
//! reaches the API, which are plain text: a body declared at more than
//! twice `server.max_body` (`413`, refused before it is read), a
//! connection beyond `server.max_connections` (`503`), and a malformed or
//! too slow request (`400`, `408`, `414`, `431`).
//!
//! # Rate limits
//!
//! Each token has a token bucket ([`RequestRate::DEFAULT`]: 1000 requests
//! per second, bursts of 2000, adjustable per token in
//! [`ServerOptions`]); a request over it is answered `429
//! resource_exhausted` with `Retry-After`. Limits apply only while
//! authentication is enforced.
//!
//! # Long polls, event streams and connections
//!
//! belay-http serves every connection on its own thread, so a
//! long-polling `POST /v1/lease` (up to 60 s) and an event stream
//! (indefinitely) each hold a thread and a connection slot while they
//! wait. `server.max_connections` (default 1024) must therefore cover the
//! workers polling at once plus the event subscribers plus ordinary
//! traffic; beyond it new connections get an immediate `503`. The pool
//! grows and shrinks with demand, which is why `server.threads` has no
//! effect here. Both kinds of request check for a shutdown every
//! [`ServerOptions::shutdown_poll`], so neither holds up a graceful stop.
//!
//! # Clusters
//!
//! Requests about a queue or a schedule owned by another node are
//! answered `307 Temporary Redirect` with an absolute `Location` on the
//! owner: enqueueing, listing jobs of one queue, leasing, and every
//! `/v1/queues/:name` and `/v1/schedules/:id` call. A 307 keeps method
//! and body, so the client replays the request on the owner with its own
//! credentials; nothing is proxied. Everything else is served by the node
//! asked: listings show what that node holds, events are that node's
//! events, and calls naming a job by id reach only the node that stores
//! the job (a node without it says so in its `not_found` message).
//!
//! An enqueue batch spanning queues owned by different nodes is refused
//! with `400` naming the owners. A lease is served from the queues owned
//! here; queues owned elsewhere are skipped and named in the
//! `Belay-Remote-Queues` response header, and only when none is owned here
//! is the lease redirected, to the owner of the first queue listed.
//!
//! # Shutdown
//!
//! [`ServerHandle::shutdown`], dropping the [`ServerHandle`], or
//! `POST /v1/admin/shutdown` stop the server gracefully: it stops
//! accepting connections, `/readyz` turns to `503`, requests in flight
//! finish (long polls answer with no jobs, event streams end), and after
//! at most `server.shutdown_grace` the engine shuts down. The standard
//! library cannot catch signals, so Ctrl-C simply ends the process; the
//! store is crash-consistent, and leased jobs return to their queues when
//! their leases expire.
//!
//! # Limitations
//!
//! - Plain HTTP only; put a TLS-terminating proxy in front.
//! - Nothing is proxied between cluster nodes, and a client following a
//!   redirected lease must send that job's heartbeat and result to the
//!   node that answered (belay's client does not remember redirects).
//! - The access log and request metrics time a streamed response up to the
//!   moment streaming starts.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

mod app;

mod auth;

mod error;

mod extract;

mod guard;

mod handle;

mod observe;

mod options;

mod placement;

mod rate_limit;
