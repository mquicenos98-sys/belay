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

impl ValuePool {
    fn new(rng: &mut Rng) -> ValuePool {
        let mut bytes = vec![0; 1 << 20];
        rng.fill_bytes(&mut bytes);
        ValuePool(bytes)
    }

    /// A `len`-byte value whose position in the pool depends on `seq`.
    fn value(&self, seq: u64, len: usize) -> &[u8] {
        let span = (self.0.len() - len + 1) as u64;
        let start = (scramble(seq) % span) as usize;
        &self.0[start..start + len]
    }
}

/// A key for sequence number `seq`, shaped like belay's job keys. Keys
/// arrive in scrambled order, so inserts land all over the table instead
/// of appending at its end.
fn key(seq: u64) -> Vec<u8> {
    format!("job/{:016x}", scramble(seq)).into_bytes()
}

/// The SplitMix64 finalizer: a bijection on `u64`, so distinct sequence
/// numbers always give distinct keys.
fn scramble(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}
