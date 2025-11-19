//! Summaries: quantile estimates over a sliding time window.
//!
//! The window (`max_age`) is split into `age_buckets` sub-windows arranged as
//! a ring. Each sub-window keeps a fixed-size reservoir sample (Vitter's
//! Algorithm R) of the observations made during it, together with how many
//! observations it saw. A scrape merges the reservoirs of the live
//! sub-windows, weighting every retained sample by `seen / retained`, and
//! reads quantiles off the weighted, sorted samples. While a sub-window has
//! seen no more observations than its reservoir holds, the estimate is the
//! exact nearest-rank quantile.
//!
//! The hot path takes no lock. A sub-window's epoch tag and observation count
//! share one `AtomicU64`, so claiming a reservoir index and rotating a stale
//! sub-window are a single compare-and-swap; the sample itself is then
//! stored into an atomic slot. The replacement index for Algorithm R comes
//! from hashing the epoch and observation index rather than from a shared
//! RNG, which keeps the hot path contention-free and makes the estimate a
//! pure function of the observation sequence and the clock, so tests with
//! [`ManualTime`](crate::ManualTime) are fully deterministic.
//!
//! The price of lock-freedom is a benign race: a scrape can read a sample
//! slot that a concurrent observer has claimed but not yet written, and a
//! writer delayed across a rotation can leave one stale value in the new
//! sub-window. Either way at most one sample per in-flight observation is
//! off, which is well inside the estimator's sampling error.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::atomic::AtomicF64;
use crate::error::{Error, Result};
use crate::snapshot::{Quantile, SummaryValue};
use crate::time::{self, SharedTimeSource, TimeSource};
use crate::timer::{Observe, SummaryTimer, Timer};

const MAX_AGE_BUCKETS: u32 = 256;

const MAX_RESERVOIR_SIZE: usize = 1 << 16;

const MIN_SUB_WINDOW: Duration = Duration::from_millis(1);
