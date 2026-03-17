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

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ({})", self.method(), self.pattern, self.access)
    }
}

use Verb::{Delete, Get, Post, Put};

const PUBLIC: Access = Access::Public;
const READ: Access = Access::Token(Scope::Read);
const ENQUEUE: Access = Access::Token(Scope::Enqueue);
const WORK: Access = Access::Token(Scope::Work);
const ADMIN: Access = Access::Token(Scope::Admin);

/// Every route, grouped by resource.
pub(crate) const ROUTES: &[Route] = &[
    // Probes, metrics, identity.
    Route::new(Get, "/healthz", PUBLIC, system::healthz),
    Route::new(Get, "/readyz", PUBLIC, system::readyz),
    Route::new(Get, "/metrics", READ, system::metrics),
    Route::new(Get, "/v1/info", READ, system::info),
    // Jobs.
    Route::new(Post, "/v1/jobs", ENQUEUE, jobs::enqueue),
    Route::new(Get, "/v1/jobs", READ, jobs::list),
    Route::new(Get, "/v1/jobs/:id", READ, jobs::get),
    Route::new(Delete, "/v1/jobs/:id", ADMIN, jobs::delete),
    Route::new(Post, "/v1/jobs/:id/cancel", ADMIN, jobs::cancel),
    Route::new(Post, "/v1/jobs/:id/retry", ADMIN, jobs::retry),
    Route::new(Post, "/v1/jobs/:id/promote", ADMIN, jobs::promote),
    // The worker protocol.
    Route::new(Post, "/v1/lease", WORK, work::lease),
    Route::new(Post, "/v1/jobs/:id/heartbeat", WORK, work::heartbeat),
    Route::new(Post, "/v1/jobs/:id/complete", WORK, work::complete),
    Route::new(Post, "/v1/jobs/:id/fail", WORK, work::fail),
    // Queues.
    Route::new(Get, "/v1/queues", READ, queues::list),
    Route::new(Get, "/v1/queues/:name", READ, queues::get),
    Route::new(Delete, "/v1/queues/:name", ADMIN, queues::delete),
    Route::new(Put, "/v1/queues/:name/config", ADMIN, queues::set_config),
    Route::new(Post, "/v1/queues/:name/pause", ADMIN, queues::pause),
    Route::new(Post, "/v1/queues/:name/resume", ADMIN, queues::resume),
    Route::new(Post, "/v1/queues/:name/purge", ADMIN, queues::purge),
    // Schedules.
    Route::new(Get, "/v1/schedules", READ, schedules::list),
    Route::new(Post, "/v1/schedules", ADMIN, schedules::create),
    Route::new(Get, "/v1/schedules/:id", READ, schedules::get),
    Route::new(Put, "/v1/schedules/:id", ADMIN, schedules::put),
    Route::new(Delete, "/v1/schedules/:id", ADMIN, schedules::delete),
    Route::new(Post, "/v1/schedules/:id/pause", ADMIN, schedules::pause),
    Route::new(Post, "/v1/schedules/:id/resume", ADMIN, schedules::resume),
    Route::new(Post, "/v1/schedules/:id/trigger", ADMIN, schedules::trigger),
    Route::new(Get, "/v1/schedules/:id/preview", READ, schedules::preview),
    // Calendars.
    Route::new(Get, "/v1/calendars", READ, calendars::list),
    Route::new(Get, "/v1/calendars/:name", READ, calendars::get),
    Route::new(Put, "/v1/calendars/:name", ADMIN, calendars::put),
    Route::new(Delete, "/v1/calendars/:name", ADMIN, calendars::delete),
    // Workflows.
    Route::new(Post, "/v1/workflows", ENQUEUE, workflows::submit),
    Route::new(Get, "/v1/workflows", READ, workflows::list),
    Route::new(Get, "/v1/workflows/:id", READ, workflows::get),
    Route::new(Post, "/v1/workflows/:id/cancel", ENQUEUE, workflows::cancel),
    // Workers, events, cluster.
    Route::new(Get, "/v1/workers", READ, workers::list),
    Route::new(Get, "/v1/events", READ, events::stream),
    Route::new(Get, "/v1/cluster/members", READ, cluster::members),
    // Administration.
    Route::new(Post, "/v1/admin/checkpoint", ADMIN, admin::checkpoint),
    Route::new(Get, "/v1/admin/store", ADMIN, admin::store),
    Route::new(Get, "/v1/admin/verify", ADMIN, admin::verify),
    Route::new(Post, "/v1/admin/shutdown", ADMIN, admin::shutdown),
];

/// Shortcuts for handler tests: an engine without background threads and
/// the full router over it, called in-process.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use belay_core::{Config, Engine};
    use belay_http::{Handler, Method, Request, Router};
    use belay_json::Value;

    use crate::app::App;
    use crate::options::ServerOptions;

    pub(crate) fn engine() -> Arc<Engine> {
        belay_common::log::set_max_level(belay_common::log::Level::Warn);
        let engine = Engine::builder()
            .config(Config::dev())
            .background(false)
            .start()
            .expect("engine starts");
        Arc::new(engine)
    }

    pub(crate) fn app(engine: &Arc<Engine>, options: ServerOptions) -> App {
        App::new(Arc::clone(engine), options)
    }

    pub(crate) fn router(engine: &Arc<Engine>) -> Router {
        super::build(Arc::new(app(engine, ServerOptions::default())))
    }

    /// Calls the router with a request; returns the status and the body as
    /// JSON (`null` when there is none).
    pub(crate) fn call(router: &Router, mut req: Request) -> (u16, Value) {
        let response = router.call(&mut req);
        let body = response.body_bytes().unwrap_or_default();
        let json = if body.is_empty() || !crate::reply::is_json(&response) {
            Value::Null
        } else {
            belay_json::parse_bytes(body).expect("JSON body")
        };
        (response.status.as_u16(), json)
    }

    pub(crate) fn request(method: Method, target: &str, body: &str) -> Request {
        Request::new(method, target).with_body(body)
    }
}

/// The router for `app`: every route behind the middleware stack.
///
/// Outermost first: request ids (so every later layer and the log line
/// see one), the access log and metrics (so they record the final status,
/// panics included), JSON error bodies, the panic catcher, and the body
/// size limit, which refuses oversized bodies before authentication or
/// any handler looks at them.
pub(crate) fn build(app: Arc<App>) -> Router {
    let mut router = Router::new();
    router
        .wrap(SetRequestId::new())
        .wrap(Observe::new(app.metrics.clone()))
        .wrap(ApiErrors)
        .wrap(catch_panics())
        .wrap(BodyLimit::new(app.engine.config().server.max_body));
    for route in ROUTES {
        let endpoint = Endpoint::new(Arc::clone(&app), route.access, route.handler);
        router.route(route.method(), route.pattern, endpoint);
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn access_of(verb: Verb, pattern: &str) -> Access {
        ROUTES
            .iter()
            .find(|r| r.verb == verb && r.pattern == pattern)
            .unwrap_or_else(|| panic!("no route {verb:?} {pattern}"))
            .access
    }

    #[test]
    fn every_documented_endpoint_has_a_route() {
        // The API reference, endpoint by endpoint.
        let documented = [
            (Get, "/healthz"),
            (Get, "/readyz"),
            (Get, "/metrics"),
            (Get, "/v1/info"),
            (Post, "/v1/jobs"),
            (Get, "/v1/jobs"),
            (Get, "/v1/jobs/:id"),
            (Post, "/v1/jobs/:id/cancel"),
            (Post, "/v1/jobs/:id/retry"),
            (Post, "/v1/jobs/:id/promote"),
            (Delete, "/v1/jobs/:id"),
            (Post, "/v1/lease"),
            (Post, "/v1/jobs/:id/heartbeat"),
            (Post, "/v1/jobs/:id/complete"),
            (Post, "/v1/jobs/:id/fail"),
            (Get, "/v1/queues"),
            (Get, "/v1/queues/:name"),
            (Put, "/v1/queues/:name/config"),
            (Post, "/v1/queues/:name/pause"),
            (Post, "/v1/queues/:name/resume"),
            (Post, "/v1/queues/:name/purge"),
            (Delete, "/v1/queues/:name"),
            (Get, "/v1/schedules"),
            (Post, "/v1/schedules"),
            (Get, "/v1/schedules/:id"),
            (Put, "/v1/schedules/:id"),
            (Delete, "/v1/schedules/:id"),
            (Post, "/v1/schedules/:id/pause"),
            (Post, "/v1/schedules/:id/resume"),
            (Post, "/v1/schedules/:id/trigger"),
            (Get, "/v1/schedules/:id/preview"),
            (Get, "/v1/calendars/:name"),
            (Put, "/v1/calendars/:name"),
            (Delete, "/v1/calendars/:name"),
            (Get, "/v1/calendars"),
            (Post, "/v1/workflows"),
            (Get, "/v1/workflows"),
            (Get, "/v1/workflows/:id"),
            (Post, "/v1/workflows/:id/cancel"),
            (Get, "/v1/workers"),
            (Get, "/v1/events"),
            (Get, "/v1/cluster/members"),
            (Post, "/v1/admin/checkpoint"),
            (Get, "/v1/admin/store"),
        ];
        for (verb, pattern) in documented {
            access_of(verb, pattern);
        }
        // Plus two administrative additions: verify and shutdown.
        assert_eq!(ROUTES.len(), documented.len() + 2);
    }

    #[test]
    fn routes_are_unique_and_register_cleanly() {
        let mut seen = BTreeSet::new();
        let mut router = Router::new();
        for route in ROUTES {
            assert!(
                seen.insert((route.method().to_string(), route.pattern)),
                "{route:?} is listed twice"
            );
            router
                .try_route(
                    route.method(),
                    route.pattern,
                    |_: &mut belay_http::Request| belay_http::Response::no_content(),
                )
                .unwrap_or_else(|e| panic!("{route:?}: {e}"));
        }
    }

    #[test]
    fn only_the_probes_are_public() {
        let public: Vec<&str> = ROUTES
            .iter()
            .filter(|r| r.access == PUBLIC)
            .map(|r| r.pattern)
            .collect();
        assert_eq!(public, ["/healthz", "/readyz"]);
    }

    #[test]
    fn reads_need_read_except_administration() {
        for route in ROUTES
            .iter()
            .filter(|r| r.verb == Get && r.access != PUBLIC)
        {
            let expected = if route.pattern.starts_with("/v1/admin/") {
                ADMIN
            } else {
                READ
            };
            assert_eq!(route.access, expected, "{route:?}");
        }
    }

    #[test]
    fn the_worker_protocol_needs_work() {
        for pattern in [
            "/v1/lease",
            "/v1/jobs/:id/heartbeat",
            "/v1/jobs/:id/complete",
            "/v1/jobs/:id/fail",
        ] {
            assert_eq!(access_of(Post, pattern), WORK, "{pattern}");
        }
        let work: Vec<&str> = ROUTES
            .iter()
            .filter(|r| r.access == WORK)
            .map(|r| r.pattern)
            .collect();
        assert_eq!(work.len(), 4, "{work:?}");
    }

    #[test]
    fn every_other_mutation_needs_admin() {
        for route in ROUTES.iter().filter(|r| r.verb != Get) {
            if matches!(route.access, Access::Token(Scope::Enqueue | Scope::Work)) {
                continue;
            }
            assert_eq!(route.access, ADMIN, "{route:?}");
        }
        assert_eq!(access_of(Delete, "/v1/jobs/:id"), ADMIN);
        assert_eq!(access_of(Post, "/v1/admin/shutdown"), ADMIN);
    }

    #[test]
    fn routes_describe_themselves() {
        assert_eq!(format!("{:?}", ROUTES[0]), "GET /healthz (public)");
        let lease = ROUTES
            .iter()
            .find(|r| r.pattern() == "/v1/lease")
            .expect("lease route");
        assert_eq!(lease.method(), Method::Post);
        assert_eq!(lease.access(), WORK);
    }
}
