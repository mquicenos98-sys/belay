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

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("engine", &self.engine)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl App {
    /// State for serving `engine`. The server's metric families go into
    /// the engine's registry, so `/metrics` shows both; with
    /// `metrics.enabled = false` neither registers anything.
    pub(crate) fn new(engine: Arc<Engine>, options: ServerOptions) -> App {
        let metrics = if engine.config().metrics.enabled {
            match HttpMetrics::register(engine.metrics()) {
                Ok(metrics) => Some(metrics),
                Err(e) => {
                    warn!("HTTP metrics are not recorded: registering them failed"; error = %e);
                    None
                }
            }
        } else {
            None
        };
        App {
            limiter: RateLimiter::new(&options),
            engine,
            options,
            metrics,
        }
    }

    pub(crate) fn auth(&self) -> &AuthConfig {
        &self.engine.config().auth
    }

    /// Lets `req` through `access`, recording the caller in its
    /// extensions (also when refused for its rate, so the access log
    /// names who was throttled).
    fn admit(&self, req: &mut Request, access: Access) -> Result<(), Response> {
        let Access::Token(scope) = access else {
            return Ok(());
        };
        let principal = auth::authorize(self.auth(), req, scope)?;
        let limited = self.limiter.check(&principal.name, Instant::now()).err();
        let refusal = limited.map(|wait| {
            if let Some(metrics) = &self.metrics {
                metrics.rate_limited(&principal.name);
            }
            reply::rate_limited(&principal.name, wait)
        });
        req.extensions.insert(principal);
        match refusal {
            Some(response) => Err(response),
            None => Ok(()),
        }
    }
}

/// Whether the HTTP server `req` arrived through is shutting down. False
/// for requests handed to the router directly, which have no server.
pub(crate) fn server_draining(req: &Request) -> bool {
    req.extensions
        .get::<ShutdownSignal>()
        .is_some_and(ShutdownSignal::is_triggered)
}

/// One route's handler behind its access check.
pub(crate) struct Endpoint {
    app: Arc<App>,
    access: Access,
    run: HandlerFn,
}

impl Endpoint {
    pub(crate) fn new(app: Arc<App>, access: Access, run: HandlerFn) -> Endpoint {
        Endpoint { app, access, run }
    }
}

impl Handler for Endpoint {
    fn call(&self, req: &mut Request) -> Response {
        if let Err(refusal) = self.app.admit(req, self.access) {
            return refusal;
        }
        // A stopped engine may still accept some calls (an in-memory store
        // has nothing to close), but nothing done now would be served:
        // send clients elsewhere, or back after the restart.
        if self.access != Access::Public && self.app.engine.is_shutting_down() {
            return reply::error(&ApiError::unavailable("the engine is shutting down"));
        }
        match (self.run)(&self.app, req) {
            Ok(response) => response,
            Err(error) => reply::error(&error),
        }
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::config::{ApiToken, Principal, Scope, Scopes};
    use belay_core::Config;
    use belay_http::Method;
    use belay_proto::api::{decode_error, ErrorCode};

    use crate::options::RequestRate;

    const SECRET: &str = "reader-secret-0123456789";

    fn app(tokens: bool, options: ServerOptions) -> App {
        let mut config = Config::dev();
        if tokens {
            config.auth.tokens.push(ApiToken::new(
                "reader",
                SECRET,
                [Scope::Read].into_iter().collect::<Scopes>(),
            ));
        }
        let engine = Engine::builder()
            .config(config)
            .background(false)
            .start()
            .expect("engine starts");
        App::new(Arc::new(engine), options)
    }

    fn whoami(_: &App, req: &mut Request) -> Result<Response, ApiError> {
        let name = req
            .extensions
            .get::<Principal>()
            .map_or("nobody", |p| p.name.as_str())
            .to_owned();
        Ok(Response::text(name))
    }

    fn failing(_: &App, _: &mut Request) -> Result<Response, ApiError> {
        Err(ApiError::not_found("job", "x"))
    }

    fn call(endpoint: &Endpoint, bearer: Option<&str>) -> Response {
        let mut req = Request::new(Method::Get, "/v1/whoami");
        if let Some(secret) = bearer {
            req.headers
                .insert("Authorization", format!("Bearer {secret}"));
        }
        endpoint.call(&mut req)
    }

    #[test]
    fn scoped_endpoints_record_the_caller() {
        let app = Arc::new(app(true, ServerOptions::new()));
        let endpoint = Endpoint::new(Arc::clone(&app), Access::Token(Scope::Read), whoami);
        let response = call(&endpoint, Some(SECRET));
        assert_eq!(response.body_bytes(), Some(&b"reader"[..]));
        assert_eq!(call(&endpoint, None).status, 401);
        let admin_only = Endpoint::new(app, Access::Token(Scope::Admin), whoami);
        assert_eq!(call(&admin_only, Some(SECRET)).status, 403);
    }

    #[test]
    fn public_endpoints_skip_authentication() {
        let app = Arc::new(app(true, ServerOptions::new()));
        let endpoint = Endpoint::new(app, Access::Public, whoami);
        assert_eq!(call(&endpoint, None).body_bytes(), Some(&b"nobody"[..]));
    }

    #[test]
    fn tokens_over_their_rate_are_refused_and_counted() {
        let options = ServerOptions::new().with_rate_limit(Some(RequestRate::new(0.001, 2)));
        let app = Arc::new(app(true, options));
        let endpoint = Endpoint::new(Arc::clone(&app), Access::Token(Scope::Read), whoami);
        assert_eq!(call(&endpoint, Some(SECRET)).status, 200);
        assert_eq!(call(&endpoint, Some(SECRET)).status, 200);
        let refused = call(&endpoint, Some(SECRET));
        assert_eq!(refused.status, 429);
        assert!(refused.headers.get("retry-after").is_some());
        let error = decode_error(429, refused.body_bytes().expect("json"));
        assert_eq!(error.code, ErrorCode::ResourceExhausted);
        let scrape = app.engine.render_metrics();
        assert!(
            scrape.contains("belay_http_rate_limited_total{token=\"reader\"} 1\n"),
            "{scrape}"
        );
    }

    #[test]
    fn handler_errors_become_api_error_responses() {
        let app = Arc::new(app(false, ServerOptions::new()));
        let endpoint = Endpoint::new(app, Access::Token(Scope::Read), failing);
        let response = call(&endpoint, None);
        assert_eq!(response.status, 404);
        let error = decode_error(404, response.body_bytes().expect("json"));
        assert_eq!(error.message, "job x not found");
    }

    #[test]
    fn a_stopped_engine_turns_api_calls_away_but_not_probes() {
        let app = Arc::new(app(false, ServerOptions::new()));
        app.engine.shutdown();
        let api = Endpoint::new(Arc::clone(&app), Access::Token(Scope::Read), whoami);
        let refused = call(&api, None);
        assert_eq!(refused.status, 503);
        assert_eq!(refused.headers.get("retry-after"), Some("1"));
        let probe = Endpoint::new(app, Access::Public, whoami);
        assert_eq!(call(&probe, None).status, 200);
    }

    #[test]
    fn requests_outside_a_server_are_never_draining() {
        let mut req = Request::new(Method::Get, "/");
        assert!(!server_draining(&req));
        let signal = ShutdownSignal::new();
        req.extensions.insert(signal.clone());
        assert!(!server_draining(&req));
        signal.trigger();
        assert!(server_draining(&req));
    }
}
