//! Engine settings: [`EngineOptions`] and [`QueueOrder`].
//!
//! Per-queue behaviour (concurrency, rate limits, retries, retention) lives
//! in each queue's [`QueueConfig`](belay_proto::QueueConfig); these options
//! cover what is engine-wide: fairness knobs, batch sizes, worker tracking
//! and the randomness seed that makes tests reproducible.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use belay_proto::TenantId;
use belay_store::Namespace;

/// How a lease request naming several queues spreads its picks over them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QueueOrder {
    /// Take one job from each queue in turn, starting from a position that
    /// rotates between requests, so that a flooded queue cannot starve the
    /// others. The default. Text form `round_robin`.
    #[default]
    RoundRobin,
    /// Drain the queues in the order the request lists them: the second is
    /// only consulted when the first has nothing to give. Lets workers
    /// express strict preference. Text form `in_order`.
    InOrder,
}

impl fmt::Display for QueueOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            QueueOrder::RoundRobin => "round_robin",
            QueueOrder::InOrder => "in_order",
        })
    }
}

/// Error parsing a [`QueueOrder`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseQueueOrderError(String);

impl fmt::Display for ParseQueueOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown queue order {:?}, expected round_robin or in_order",
            self.0
        )
    }
}

impl std::error::Error for ParseQueueOrderError {}
