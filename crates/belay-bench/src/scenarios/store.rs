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

pub fn run_write(s: &mut Session<'_>) -> Result<()> {
    let policies = [
        SyncPolicy::Always,
        SyncPolicy::Interval(Duration::from_millis(10)),
        SyncPolicy::Never,
    ];
    let values: &[usize] = s.pick(&[16, 128, 1024, 8192], &[128, 1024], &[128]);
    let pool = ValuePool::new(&mut s.rng(WRITE));
    for policy in policies {
        for batch in [1, 16, 256] {
            for &value in values {
                write_case(s, &pool, policy, batch, value, 1)?;
            }
        }
    }
    // Concurrent writers under `always` share syncs (group commit); the
    // `group` note shows how many batches each sync carried.
    for threads in s.thread_counts().into_iter().filter(|&t| t > 1) {
        write_case(s, &pool, SyncPolicy::Always, 1, 128, threads)?;
    }
    Ok(())
}

fn write_case(
    s: &mut Session<'_>,
    pool: &ValuePool,
    policy: SyncPolicy,
    batch: usize,
    value: usize,
    threads: usize,
) -> Result<()> {
    let params = Params::new()
        .with("sync", policy)
        .with("batch", batch)
        .with("value", format_size(value as u64))
        .with("threads", threads);
    if !s.wants(WRITE, &params) {
        return Ok(());
    }
    let dir = s.temp_dir(WRITE)?;
    let kv = LogKv::open(dir.path(), Options::new().sync(policy).manual_checkpoints())?;
    let op_bytes = (batch * (KEY_LEN + value)) as u64;
    let plan = Plan::new(write_iterations(s, policy, op_bytes, batch as u64));
    // Each thread's closure copies this reference instead of borrowing
    // the builder closure's capture.
    let store = &kv;
    let sample = measure_parallel(plan, threads, |_| {
        Ok(move |seq: u64| {
            let mut writes = WriteBatch::with_capacity(batch);
            for i in 0..batch as u64 {
                let n = seq * batch as u64 + i;
                writes.put(key(n), pool.value(n, value));
            }
            store.write(writes)?;
            Ok(Work::items(batch as u64).with_bytes(op_bytes))
        })
    })?;
    let stats = kv.stats();
    kv.close()?;
    let mut result = CaseResult::new(WRITE, params, sample).note(
        "syncs/write",
        format!("{:.2}", ratio(stats.wal_syncs, stats.batches)),
    );
    if threads > 1 {
        result = result.note(
            "group",
            format!("{:.1}", ratio(stats.batches, stats.commit_groups)),
        );
    }
    s.record(result);
    Ok(())
}

/// Enough writes for stable percentiles, bounded by the number of keys,
/// by the bytes written (large values fill a disk fast) and, under
/// `always`, by the number of syncs (tens of seconds on slow devices).
fn write_iterations(s: &Session<'_>, policy: SyncPolicy, op_bytes: u64, batch: u64) -> u64 {
    let keys: u64 = s.pick(1 << 18, 1 << 15, 4);
    let bytes: u64 = s.pick(128 << 20, 16 << 20, 1 << 20);
    let cap = if policy == SyncPolicy::Always {
        s.pick(2_000, 200, 2)
    } else {
        s.pick(50_000, 5_000, 2)
    };
    (keys / batch).min(bytes / op_bytes).clamp(1, cap)
}

pub fn run_recover(s: &mut Session<'_>) -> Result<()> {
    let sizes: &[u64] = s.pick(
        &[4 << 20, 32 << 20, 128 << 20],
        &[1 << 20, 8 << 20],
        &[64 << 10],
    );
    let opens = s.pick(10, 5, 1);
    let pool = ValuePool::new(&mut s.rng(RECOVER));
    for &size in sizes {
        let from_log = Params::new()
            .with("from", "log")
            .with("size", format_size(size));
        let from_snapshot = Params::new()
            .with("from", "snapshot")
            .with("size", format_size(size));
        let (want_log, want_snapshot) = (
            s.wants(RECOVER, &from_log),
            s.wants(RECOVER, &from_snapshot),
        );
        if !want_log && !want_snapshot {
            continue;
        }
        let dir = s.temp_dir(RECOVER)?;
        let keys = fill_log(dir.path(), &pool, size)?;
        if want_log {
            let (sample, stats) = measure_opens(dir.path(), opens, keys, |st| st.wal_bytes)?;
            let replayed = stats.recovery.batches_replayed;
            s.record(
                CaseResult::new(RECOVER, from_log, sample)
                    .note("keys", keys)
                    .note("batches", replayed)
                    .note("segments", stats.recovery.segments_replayed),
            );
        }
        if want_snapshot {
            let kv = LogKv::open(dir.path(), recover_options())?;
            kv.checkpoint()?;
            kv.close()?;
            let (sample, stats) = measure_opens(dir.path(), opens, keys, |st| st.snapshot_bytes)?;
            s.record(
                CaseResult::new(RECOVER, from_snapshot, sample)
                    .note("keys", keys)
                    .note("snapshot", format_size(stats.snapshot_bytes)),
            );
        }
    }
    Ok(())
}

fn recover_options() -> Options {
    Options::new().sync(SyncPolicy::Never).manual_checkpoints()
}

/// Writes batches of 64 keys with 256-byte values until the log holds at
/// least `size` bytes; returns the number of keys.
fn fill_log(dir: &Path, pool: &ValuePool, size: u64) -> Result<u64> {
    const BATCH: u64 = 64;
    let kv = LogKv::open(dir, recover_options())?;
    let mut seq = 0;
    while kv.stats().wal_bytes < size {
        let mut writes = WriteBatch::with_capacity(BATCH as usize);
        for _ in 0..BATCH {
            writes.put(key(seq), pool.value(seq, 256));
            seq += 1;
        }
        kv.write(writes)?;
    }
    kv.close()?;
    Ok(seq)
}

/// Opens the store `iterations` times after one unrecorded open that
/// warms the page cache, timing the open alone (not the close). Throughput
/// is keys and bytes loaded per second of opening.
fn measure_opens(
    dir: &Path,
    iterations: u64,
    expect_keys: u64,
    bytes: impl Fn(&KvStats) -> u64,
) -> Result<(Sample, KvStats)> {
    let mut sample = Sample::new();
    let mut last = KvStats::default();
    for i in 0..=iterations {
        let started = Instant::now();
        let kv = LogKv::open(dir, recover_options())?;
        let took = started.elapsed();
        let stats = kv.stats();
        kv.close()?;
        if stats.keys != expect_keys {
            return Err(Error::Invalid(format!(
                "recovery loaded {} keys, expected {expect_keys}",
                stats.keys
            )));
        }
        if i > 0 {
            sample.record(took, Work::items(stats.keys).with_bytes(bytes(&stats)));
        }
        last = stats;
    }
    sample.elapsed = sample.busy;
    Ok((sample, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_are_unique_and_fixed_length() {
        let keys: HashSet<Vec<u8>> = (0..10_000).map(key).collect();
        assert_eq!(keys.len(), 10_000);
        assert!(keys.iter().all(|k| k.len() == KEY_LEN));
    }

    #[test]
    fn values_come_from_inside_the_pool() {
        let pool = ValuePool::new(&mut Rng::seed_from_u64(1));
        for seq in 0..1_000 {
            assert_eq!(pool.value(seq, 8192).len(), 8192);
        }
        assert_eq!(pool.value(7, pool.0.len()).len(), pool.0.len());
    }
}
