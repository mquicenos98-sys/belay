//! [`Registry`]: which handler runs which job kind, with per-kind limits,
//! and the middleware wrapped around every handler.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::context::JobContext;
use crate::error::{Error, Result};
use crate::handler::{failure_kind, Handler, HandlerError, Outcome};
use crate::middleware::{Middleware, Next};

/// Per-kind settings.
///
/// ```
/// use std::time::Duration;
/// use belay_worker::KindOptions;
///
/// let a = KindOptions { concurrency: Some(2), timeout: Some(Duration::from_secs(60)) };
/// let b = KindOptions::new().concurrency(2).timeout(Duration::from_secs(60));
/// assert_eq!(a, b);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindOptions {
    /// At most this many jobs of the kind run at once on this worker.
    /// `None`: only the worker's overall concurrency applies.
    pub concurrency: Option<usize>,
    /// Upper bound for one attempt. The job's own `timeout` still applies;
    /// the shorter of the two wins.
    pub timeout: Option<Duration>,
}

impl KindOptions {
    /// No limits beyond the worker's.
    #[must_use]
    pub fn new() -> KindOptions {
        KindOptions::default()
    }

    /// Limits how many jobs of the kind run at once.
    #[must_use]
    pub fn concurrency(mut self, n: usize) -> KindOptions {
        self.concurrency = Some(n);
        self
    }

    /// Caps the time one attempt may take.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> KindOptions {
        self.timeout = Some(timeout);
        self
    }
}

/// A registered handler and its options.
pub(crate) struct Entry {
    pub(crate) kind: String,
    pub(crate) handler: Arc<dyn Handler>,
    pub(crate) options: KindOptions,
}

/// Maps job kinds to handlers.
///
/// Middleware added with [`wrap`](Self::wrap) runs around every handler,
/// the first added outermost.
///
/// ```
/// use belay_worker::middleware::LoggingMiddleware;
/// use belay_worker::{handler_fn, JobContext, KindOptions, Outcome, Registry};
///
/// let registry = Registry::new()
///     .register("send_email", handler_fn(|_: &mut JobContext| Ok(Outcome::done())))
///     .register_with(
///         "resize",
///         handler_fn(|_: &mut JobContext| Ok(Outcome::done())),
///         KindOptions::new().concurrency(2),
///     )
///     .wrap(LoggingMiddleware::new());
/// assert_eq!(registry.kinds().collect::<Vec<_>>(), ["resize", "send_email"]);
/// assert_eq!(registry.options("resize").and_then(|o| o.concurrency), Some(2));
/// ```
#[derive(Clone, Default)]
pub struct Registry {
    entries: BTreeMap<String, Arc<Entry>>,
    middleware: Vec<Arc<dyn Middleware>>,
    /// Kinds registered more than once; reported by `validate` because a
    /// builder cannot fail halfway through a chain.
    duplicates: Vec<String>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Runs `handler` for jobs of `kind`.
    #[must_use]
    pub fn register(self, kind: impl Into<String>, handler: impl Handler + 'static) -> Registry {
        self.register_with(kind, handler, KindOptions::default())
    }

    /// Runs `handler` for jobs of `kind`, with limits.
    ///
    /// Registering a kind twice keeps the later handler, and
    /// [`validate`](Self::validate) (called by
    /// [`Worker::new`](crate::Worker::new)) reports the mistake.
    #[must_use]
    pub fn register_with(
        mut self,
        kind: impl Into<String>,
        handler: impl Handler + 'static,
        options: KindOptions,
    ) -> Registry {
        let kind = kind.into();
        let entry = Arc::new(Entry {
            kind: kind.clone(),
            handler: Arc::new(handler),
            options,
        });
        if self.entries.insert(kind.clone(), entry).is_some() {
            self.duplicates.push(kind);
        }
        self
    }

    /// Adds middleware around every handler. The first added runs
    /// outermost: it sees the job first and the outcome last.
    #[must_use]
    pub fn wrap(mut self, middleware: impl Middleware + 'static) -> Registry {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Registered kinds, sorted.
    pub fn kinds(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.keys().map(String::as_str)
    }

    /// Whether `kind` has a handler.
    #[must_use]
    pub fn contains(&self, kind: &str) -> bool {
        self.entries.contains_key(kind)
    }

    /// The options `kind` was registered with.
    #[must_use]
    pub fn options(&self, kind: &str) -> Option<KindOptions> {
        self.entries.get(kind).map(|e| e.options)
    }

    /// Number of registered kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no kind is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Checks for mistakes a worker should refuse to start with: no
    /// handlers, a kind registered twice, an empty kind name, or a
    /// concurrency limit of zero (the kind could never run).
    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(Error::NoHandlers);
        }
        if let Some(kind) = self.duplicates.first() {
            return Err(Error::DuplicateKind(kind.clone()));
        }
        for entry in self.entries.values() {
            if entry.kind.is_empty() {
                return Err(Error::KindOptions {
                    kind: String::new(),
                    message: "kind name must not be empty".to_owned(),
                });
            }
            if entry.options.concurrency == Some(0) {
                return Err(Error::KindOptions {
                    kind: entry.kind.clone(),
                    message: "concurrency must be at least 1".to_owned(),
                });
            }
            if entry.options.timeout == Some(Duration::ZERO) {
                return Err(Error::KindOptions {
                    kind: entry.kind.clone(),
                    message: "timeout must be positive".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Runs the handler for `ctx`'s kind through the middleware, on the
    /// calling thread. This is what the worker's executor does, minus
    /// panic catching and reporting; call it directly to run a job inline
    /// or to test handlers together with their middleware.
    ///
    /// A kind without a handler fails with a non-retryable error of kind
    /// [`unknown_kind`](crate::failure_kind::UNKNOWN_KIND).
    pub fn run(&self, ctx: &mut JobContext) -> std::result::Result<Outcome, HandlerError> {
        match self.entries.get(ctx.kind()) {
            Some(entry) => self.dispatch(entry, ctx),
            None => Err(unknown_kind(ctx.kind())),
        }
    }

    pub(crate) fn get(&self, kind: &str) -> Option<&Arc<Entry>> {
        self.entries.get(kind)
    }

    /// Whether some kind runs with no limit of its own.
    pub(crate) fn has_unlimited_kind(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.options.concurrency.is_none())
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &Arc<Entry>> + '_ {
        self.entries.values()
    }

    pub(crate) fn dispatch(
        &self,
        entry: &Entry,
        ctx: &mut JobContext,
    ) -> std::result::Result<Outcome, HandlerError> {
        Next::new(&self.middleware, &*entry.handler).run(ctx)
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: BTreeMap<&str, KindOptions> = self
            .entries
            .iter()
            .map(|(k, e)| (k.as_str(), e.options))
            .collect();
        f.debug_struct("Registry")
            .field("kinds", &kinds)
            .field("middleware", &self.middleware.len())
            .finish()
    }
}

/// The failure for a job whose kind has no handler here.
pub(crate) fn unknown_kind(kind: &str) -> HandlerError {
    HandlerError::fatal(format!("no handler registered for kind {kind:?}"))
        .with_kind(failure_kind::UNKNOWN_KIND)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::handler_fn;
    use belay_common::Timestamp;
    use belay_json::json;
    use belay_proto::{Job, JobId, JobSpec};

    fn ok() -> impl Handler {
        handler_fn(|_: &mut JobContext| Ok(Outcome::done()))
    }

    fn ctx(kind: &str) -> JobContext {
        JobContext::detached(Job::new(
            JobId::generate(),
            JobSpec::new("default", kind).payload(json!(5)),
            Timestamp::from_secs(1),
        ))
    }

    #[test]
    fn run_dispatches_on_kind() {
        let registry = Registry::new()
            .register(
                "double",
                handler_fn(|ctx: &mut JobContext| {
                    let n: i64 = ctx.payload_as()?;
                    Ok(Outcome::with(n * 2))
                }),
            )
            .register("noop", ok());
        assert_eq!(registry.run(&mut ctx("double")), Ok(Outcome::with(10)));
        assert_eq!(registry.run(&mut ctx("noop")), Ok(Outcome::done()));
        assert_eq!(registry.len(), 2);
        assert!(registry.contains("noop") && !registry.contains("nope"));
    }

    #[test]
    fn unknown_kinds_fail_permanently_with_a_quoted_name() {
        let err = Registry::new()
            .register("a", ok())
            .run(&mut ctx("x"))
            .unwrap_err();
        assert_eq!(err.message, r#"no handler registered for kind "x""#);
        assert!(!err.retryable);
        assert_eq!(err.kind.as_deref(), Some(failure_kind::UNKNOWN_KIND));
    }

    #[test]
    fn validate_rejects_setup_mistakes() {
        assert!(matches!(Registry::new().validate(), Err(Error::NoHandlers)));
        let twice = Registry::new().register("a", ok()).register("a", ok());
        assert!(matches!(twice.validate(), Err(Error::DuplicateKind(k)) if k == "a"));
        let zero = Registry::new().register_with("a", ok(), KindOptions::new().concurrency(0));
        assert_eq!(
            zero.validate().unwrap_err().to_string(),
            r#"invalid options for kind "a": concurrency must be at least 1"#
        );
        let instant =
            Registry::new().register_with("a", ok(), KindOptions::new().timeout(Duration::ZERO));
        assert!(instant.validate().is_err());
        let unnamed = Registry::new().register("", ok());
        assert!(unnamed.validate().is_err());
        assert!(Registry::new().register("a", ok()).validate().is_ok());
    }

    #[test]
    fn unlimited_kinds_are_detected() {
        let limited = Registry::new().register_with("a", ok(), KindOptions::new().concurrency(1));
        assert!(!limited.has_unlimited_kind());
        assert!(limited.register("b", ok()).has_unlimited_kind());
    }

    #[test]
    fn debug_lists_kinds_and_their_options() {
        let registry = Registry::new().register_with("a", ok(), KindOptions::new().concurrency(3));
        let text = format!("{registry:?}");
        assert!(text.contains("\"a\""), "{text}");
        assert!(text.contains("concurrency: Some(3)"), "{text}");
    }
}
