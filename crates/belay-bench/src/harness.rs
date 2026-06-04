//! Measuring loops: warm-up calls, then measured calls each timed into a
//! [`Histogram`], on one thread or several started together.
//!
//! An operation is a closure called with a sequence number and returning
//! the [`Work`] it did (items, bytes). The harness times every call, adds
//! up the work, and measures the wall-clock time of the measured phase;
//! throughput is work divided by that wall-clock time, so with several
//! threads it is the aggregate rate.

use std::fmt;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use crate::histogram::Histogram;
use crate::{Error, Result};

/// The parameters that identify a case within a scenario, in a fixed
/// order: `sync=always batch=16 value=128B`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params(Vec<(&'static str, String)>);

impl Params {
    /// No parameters.
    #[must_use]
    pub fn new() -> Params {
        Params::default()
    }

    /// Appends `key=value`.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl fmt::Display) -> Params {
        self.0.push((key, value.to_string()));
        self
    }

    /// The value of `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The parameters in order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> + '_ {
        self.0.iter().map(|(k, v)| (*k, v.as_str()))
    }
}

impl fmt::Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{key}={value}")?;
        }
        Ok(())
    }
}

/// What one call of an operation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    /// Items processed (keys written, jobs enqueued, documents parsed).
    pub items: u64,
    /// Payload bytes processed, for MB/s; 0 when not meaningful.
    pub bytes: u64,
}

impl Work {
    /// One item, no byte count.
    pub const ONE: Work = Work { items: 1, bytes: 0 };

    /// `items` items, no byte count.
    #[must_use]
    pub fn items(items: u64) -> Work {
        Work { items, bytes: 0 }
    }

    /// Sets the byte count.
    #[must_use]
    pub fn with_bytes(self, bytes: u64) -> Work {
        Work { bytes, ..self }
    }
}

/// How many calls to make: `warmup` untimed ones, then `iterations`
/// measured ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Calls made before measuring, to fill caches, grow tables and start
    /// connections.
    pub warmup: u64,
    /// Calls measured.
    pub iterations: u64,
}
