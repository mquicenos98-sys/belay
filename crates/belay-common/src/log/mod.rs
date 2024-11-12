//! A small structured logger.
//!
//! Records carry a level, a target (the module path by default), a formatted
//! message and typed key/value fields. They are produced by the
//! [`error!`](crate::error), [`warn!`](crate::warn), [`info!`](crate::info),
//! [`debug!`](crate::debug) and [`trace!`](crate::trace) macros, whose syntax
//! is described in [`macros`], and consumed by a [`Logger`].
//!
//! The installed logger defaults to a [`StreamLogger`] writing logfmt lines
//! to stderr:
//!
//! ```text
//! ts=2025-03-04T05:06:07.089Z level=info target=belay_queue::lease msg="lease expired" job=01JAB… worker=w1
//! ```
//!
//! A disabled statement costs one relaxed atomic load: the macros compare
//! the level with the global maximum before evaluating any argument.
//!
//! Tests capture output with a [`CaptureLogger`], usually scoped to the
//! current thread with [`CaptureLogger::scope`] so parallel tests do not see
//! each other's records.

mod capture;

mod filter;

mod format;

pub mod macros;

use std::cell::RefCell;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

pub use capture::{CaptureLogger, CapturedRecord};
pub use filter::Filter;
pub use format::{LogFormat, StreamLogger};

use crate::duration::CompactDuration;
use crate::sync::{read, write};

/// Severity of a record, from most to least severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Something failed and needs attention.
    Error = 1,
    /// Something unexpected that belay recovered from.
    Warn,
    /// Normal operational events.
    Info,
    /// Detail useful when diagnosing a problem.
    Debug,
    /// Very verbose detail, e.g. per-request internals.
    Trace,
}

impl Level {
    /// Every level, most severe first.
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// Lowercase name as used in output and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}
