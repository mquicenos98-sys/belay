//! `store-write` and `store-recover`: [`LogKv`] on a real directory.
//!
//! Both use the file system under `--dir` (the system temp directory by
//! default), so they measure that device and its sync latency: the same
//! build gives very different `sync=always` numbers on an NVMe drive, a
//! laptop SSD and a network mount. Automatic checkpoints are off, so
//! writes measure the log alone.

use std::path::Path;
use std::time::{Duration, Instant};

use belay_common::Rng;
use belay_store::{Kv, KvStats, LogKv, Options, SyncPolicy, WriteBatch};

use crate::harness::{measure_parallel, Params, Plan, Sample, Work};
use crate::session::{CaseResult, Session};
use crate::units::format_size;
use crate::{Error, Result};

pub const WRITE: &str = "store-write";

pub const RECOVER: &str = "store-recover";

/// Length of every key: `job/` and 16 hex digits.
const KEY_LEN: usize = 20;

/// Random bytes that values are cut from, so producing a value costs a
/// copy rather than random number work inside the timed loop.
struct ValuePool(Vec<u8>);
