//! [`WorkerConfig`]: identity, queues, concurrency and the timing knobs of
//! the lease protocol, with validation against the protocol's limits.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use belay_common::backoff::{BackoffPolicy, Jitter};
use belay_common::format_duration;
use belay_common::log::Logger;
use belay_proto::limits::{MAX_LEASE, MAX_LEASE_QUEUES, MAX_LEASE_WAIT, MIN_LEASE};
use belay_proto::{QueueName, WorkerId};

use crate::error::{Error, Result};

/// Most executor threads one worker may run.
pub const MAX_CONCURRENCY: usize = 1024;

/// How a worker runs.
///
/// Build it with struct update syntax or the builder methods:
///
/// ```
/// use std::time::Duration;
/// use belay_worker::WorkerConfig;
///
/// let config = WorkerConfig::default()
///     .queues(vec!["emails".parse()?, "default".parse()?])
///     .concurrency(8)
///     .lease_for(Duration::from_secs(60));
/// assert_eq!(config.effective_heartbeat(), Duration::from_secs(30));
/// assert!(config.validate().is_ok());
/// # Ok::<(), belay_proto::ParseError>(())
/// ```
#[derive(Clone)]
pub struct WorkerConfig {
    /// Name reported to the source; defaults to `host:pid`.
    pub id: WorkerId,
    /// Queues to lease from, in order of preference.
    pub queues: Vec<QueueName>,
    /// Executor threads, hence the most jobs run at once. Default 4.
    pub concurrency: usize,
    /// Lease length asked for; heartbeats renew it. Default 30s.
    pub lease_for: Duration,
    /// How long a lease call may wait for work (long polling); zero turns
    /// long polling off. Default 10s. Shutdown waits for an in-flight
    /// lease call so that jobs it returns can be handed back, so keep this
    /// well below `shutdown_grace`.
    pub poll_wait: Duration,
    /// Least time between two lease calls while the source has nothing to
    /// hand out, so a source that ignores `poll_wait` is not hammered.
    /// Default 500ms.
    pub poll_interval: Duration,
    /// Heartbeat period; `None` means half of `lease_for`.
    pub heartbeat_every: Option<Duration>,
    /// How long a shutdown waits for running jobs before failing them as
    /// retryable "worker shutting down". Default 30s.
    pub shutdown_grace: Duration,
    /// How long a handler gets to return once cancelled (timeout,
    /// shutdown) before the worker stops waiting for it. Default 5s.
    pub cancel_grace: Duration,
    /// Delay between lease retries while the source keeps failing, and
    /// between attempts to report a result. Default `exp:250ms*2..30s~equal`.
    pub source_backoff: BackoffPolicy,
    /// Calls made to deliver one result (`complete`/`fail`) before giving
    /// up and letting the lease expire. Default 3.
    pub report_attempts: u32,
    /// Receives everything logged on the worker's threads, handlers
    /// included, instead of the global logger.
    pub logger: Option<Arc<dyn Logger>>,
}

impl Default for WorkerConfig {
    fn default() -> WorkerConfig {
        WorkerConfig {
            id: default_worker_id(),
            queues: vec![QueueName::from_static("default")],
            concurrency: 4,
            lease_for: Duration::from_secs(30),
            poll_wait: Duration::from_secs(10),
            poll_interval: Duration::from_millis(500),
            heartbeat_every: None,
            shutdown_grace: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(5),
            source_backoff: BackoffPolicy::exponential(
                Duration::from_millis(250),
                2.0,
                Duration::from_secs(30),
            )
            .with_jitter(Jitter::Equal),
            report_attempts: 3,
            logger: None,
        }
    }
}

impl WorkerConfig {
    /// Defaults with the given identity and queues.
    #[must_use]
    pub fn new(id: WorkerId, queues: Vec<QueueName>) -> WorkerConfig {
        WorkerConfig {
            id,
            queues,
            ..WorkerConfig::default()
        }
    }

    /// Sets the worker id.
    #[must_use]
    pub fn id(mut self, id: WorkerId) -> WorkerConfig {
        self.id = id;
        self
    }

    /// Sets the queues, in order of preference.
    #[must_use]
    pub fn queues(mut self, queues: Vec<QueueName>) -> WorkerConfig {
        self.queues = queues;
        self
    }

    /// Sets the number of executor threads.
    #[must_use]
    pub fn concurrency(mut self, n: usize) -> WorkerConfig {
        self.concurrency = n;
        self
    }

    /// Sets the lease length.
    #[must_use]
    pub fn lease_for(mut self, d: Duration) -> WorkerConfig {
        self.lease_for = d;
        self
    }

    /// Sets the long-poll wait.
    #[must_use]
    pub fn poll_wait(mut self, d: Duration) -> WorkerConfig {
        self.poll_wait = d;
        self
    }

    /// Sets the least time between idle lease calls.
    #[must_use]
    pub fn poll_interval(mut self, d: Duration) -> WorkerConfig {
        self.poll_interval = d;
        self
    }

    /// Sets the heartbeat period.
    #[must_use]
    pub fn heartbeat_every(mut self, d: Duration) -> WorkerConfig {
        self.heartbeat_every = Some(d);
        self
    }

    /// Sets the shutdown grace period.
    #[must_use]
    pub fn shutdown_grace(mut self, d: Duration) -> WorkerConfig {
        self.shutdown_grace = d;
        self
    }

    /// Sets how long cancelled handlers get to return.
    #[must_use]
    pub fn cancel_grace(mut self, d: Duration) -> WorkerConfig {
        self.cancel_grace = d;
        self
    }

    /// Sets the retry policy for source calls.
    #[must_use]
    pub fn source_backoff(mut self, policy: BackoffPolicy) -> WorkerConfig {
        self.source_backoff = policy;
        self
    }

    /// Sets how many calls a result delivery may take.
    #[must_use]
    pub fn report_attempts(mut self, n: u32) -> WorkerConfig {
        self.report_attempts = n;
        self
    }

    /// Routes the worker's log output to `logger`.
    #[must_use]
    pub fn logger(mut self, logger: Arc<dyn Logger>) -> WorkerConfig {
        self.logger = Some(logger);
        self
    }

    /// The heartbeat period in effect.
    #[must_use]
    pub fn effective_heartbeat(&self) -> Duration {
        self.heartbeat_every.unwrap_or(self.lease_for / 2)
    }

    /// Most jobs asked for in one lease call.
    pub(crate) fn max_batch(&self) -> usize {
        let protocol_max = belay_proto::limits::MAX_LEASE_JOBS as usize;
        self.concurrency.min(protocol_max)
    }

    /// Checks every value against the protocol's limits and the others.
    pub fn validate(&self) -> Result<()> {
        if self.queues.is_empty() {
            return Err(Error::config("queues", "must name at least one queue"));
        }
        if self.queues.len() > MAX_LEASE_QUEUES {
            return Err(Error::config(
                "queues",
                format!(
                    "must name at most {MAX_LEASE_QUEUES} queues, got {}",
                    self.queues.len()
                ),
            ));
        }
        for (i, q) in self.queues.iter().enumerate() {
            if self.queues[..i].contains(q) {
                return Err(Error::config("queues", format!("lists {q} twice")));
            }
        }
        if !(1..=MAX_CONCURRENCY).contains(&self.concurrency) {
            return Err(Error::config(
                "concurrency",
                format!(
                    "must be between 1 and {MAX_CONCURRENCY}, got {}",
                    self.concurrency
                ),
            ));
        }
        check_range("lease_for", self.lease_for, MIN_LEASE, MAX_LEASE)?;
        check_range("poll_wait", self.poll_wait, Duration::ZERO, MAX_LEASE_WAIT)?;
        if let Some(every) = self.heartbeat_every {
            if every.is_zero() || every >= self.lease_for {
                return Err(Error::config(
                    "heartbeat_every",
                    format!(
                        "must be positive and shorter than lease_for ({}), got {}",
                        format_duration(self.lease_for),
                        format_duration(every)
                    ),
                ));
            }
        }
        if self.report_attempts == 0 {
            return Err(Error::config(
                "report_attempts",
                "must be at least 1, got 0",
            ));
        }
        Ok(())
    }
}

fn check_range(field: &'static str, d: Duration, min: Duration, max: Duration) -> Result<()> {
    if d < min || d > max {
        return Err(Error::config(
            field,
            format!(
                "must be between {} and {}, got {}",
                format_duration(min),
                format_duration(max),
                format_duration(d)
            ),
        ));
    }
    Ok(())
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("id", &self.id)
            .field("queues", &self.queues)
            .field("concurrency", &self.concurrency)
            .field("lease_for", &self.lease_for)
            .field("poll_wait", &self.poll_wait)
            .field("poll_interval", &self.poll_interval)
            .field("heartbeat_every", &self.heartbeat_every)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("cancel_grace", &self.cancel_grace)
            .field("source_backoff", &self.source_backoff)
            .field("report_attempts", &self.report_attempts)
            .field("logger", &self.logger.as_ref().map(|_| ".."))
            .finish()
    }
}

/// `host:pid`, with the host name from `HOSTNAME` or `COMPUTERNAME` (std
/// has no portable host name call), cleaned to the characters worker ids
/// allow. Falls back to `worker:pid`.
#[must_use]
pub fn default_worker_id() -> WorkerId {
    let host = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|h| clean_host(&h))
        .find(|h| !h.is_empty())
        .unwrap_or_else(|| "worker".to_owned());
    WorkerId::new(format!("{host}:{}", std::process::id()))
        .unwrap_or_else(|_| WorkerId::from_static("worker"))
}
