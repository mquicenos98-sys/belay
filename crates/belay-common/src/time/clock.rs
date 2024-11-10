//! Injectable time sources.
//!
//! Everything in belay that makes decisions based on the current time (lease
//! expiry, cron fires, retention) asks a [`Clock`] rather than the operating
//! system, so tests can move time explicitly with a [`ManualClock`].

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::Timestamp;

/// A source of the current wall-clock time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// A clock shared between components.
pub type SharedClock = Arc<dyn Clock>;

/// Lets structs holding a [`SharedClock`] derive `Debug`.
impl fmt::Debug for dyn Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock").field("now", &self.now()).finish()
    }
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for Box<C> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

/// The operating system's real-time clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;
