//! Code that runs around every handler: the [`Middleware`] trait, the
//! [`Next`] continuation, and two built-ins, [`LoggingMiddleware`] and
//! [`TimingMiddleware`].
//!
//! Panics, timeouts and cancellation are handled by the worker itself, not
//! by middleware, so they apply even to a registry without any: a
//! panicking handler unwinds through the middleware and is caught by the
//! executor (see [`Handler`] for how it is reported).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use belay_common::log::Level;
use belay_common::sync::lock;

use crate::context::JobContext;
use crate::handler::{Handler, HandlerError, Outcome};

/// Wraps handler execution.
///
/// Call `next.run(ctx)` to continue down the chain (eventually reaching
/// the handler), or return without calling it to short-circuit.
///
/// ```
/// use belay_worker::middleware::Next;
/// use belay_worker::{HandlerError, JobContext, Middleware, Outcome};
///
/// /// Refuses jobs from tenants on a deny list.
/// struct DenyTenants(Vec<String>);
///
/// impl Middleware for DenyTenants {
///     fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
///         let denied = ctx.job().spec.tenant.as_ref().is_some_and(|t| self.0.iter().any(|d| d == t.as_str()));
///         if denied {
///             return Err(HandlerError::fatal("tenant is suspended"));
///         }
///         next.run(ctx)
///     }
/// }
/// ```
pub trait Middleware: Send + Sync {
    /// Runs around the rest of the chain.
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError>;
}

impl<M: Middleware + ?Sized> Middleware for Arc<M> {
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        (**self).call(ctx, next)
    }
}

/// The rest of the chain after the current middleware.
pub struct Next<'a> {
    chain: &'a [Arc<dyn Middleware>],
    handler: &'a dyn Handler,
}

impl<'a> Next<'a> {
    pub(crate) fn new(chain: &'a [Arc<dyn Middleware>], handler: &'a dyn Handler) -> Next<'a> {
        Next { chain, handler }
    }

    /// Runs the remaining middleware and then the handler.
    pub fn run(self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        match self.chain.split_first() {
            Some((first, rest)) => first.call(ctx, Next::new(rest, self.handler)),
            None => self.handler.handle(ctx),
        }
    }
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("remaining_middleware", &self.chain.len())
            .finish()
    }
}

/// [`Middleware`] made from a closure by [`middleware_fn`].
#[derive(Clone)]
pub struct FnMiddleware<F>(F);

impl<F> fmt::Debug for FnMiddleware<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnMiddleware(..)")
    }
}

impl<F> Middleware for FnMiddleware<F>
where
    F: Fn(&mut JobContext, Next<'_>) -> Result<Outcome, HandlerError> + Send + Sync,
{
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        (self.0)(ctx, next)
    }
}
