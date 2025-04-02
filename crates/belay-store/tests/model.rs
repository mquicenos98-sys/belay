//! Randomized model-based tests: long runs of random writes, reads, scans,
//! checkpoints, reopens and crashes against `LogKv` on `MemFs`, checked
//! step by step against a plain `BTreeMap`.

mod support;

use std::ops::Bound;
use std::thread;
use std::time::{Duration, Instant};

use belay_common::Rng;
use belay_store::keys::KeyBuf;
use belay_store::vfs::{Crash, MemFs};
use belay_store::{inspect, KeyRange, Kv, LogKv, MemKv, Options, SyncPolicy, WriteBatch};

use support::{apply, dir, drop_quietly, dump, last_segment, opts, Model};

/// Keys drawn from a small space so that overwrites, deletes and range
/// deletes hit existing entries often.
fn random_key(rng: &mut Rng) -> Vec<u8> {
    match rng.gen_range(0..3u32) {
        0 => KeyBuf::new()
            .str(["job", "queue", "sched"][rng.gen_range(0..3usize)])
            .u64(rng.gen_range(0..30u64))
            .into_vec(),
        1 => vec![rng.gen_range(0..4u8), rng.gen_range(0..4u8)],
        _ => format!("k{}", rng.gen_range(0..40u32)).into_bytes(),
    }
}

fn random_batch(rng: &mut Rng) -> WriteBatch {
    let mut b = WriteBatch::new();
    for _ in 0..rng.gen_range(1..5usize) {
        match rng.gen_range(0..10u32) {
            0..=5 => {
                let mut value = vec![0u8; rng.gen_range(0..120usize)];
                rng.fill_bytes(&mut value);
                b.put(random_key(rng), value);
            }
            6 | 7 => {
                b.delete(random_key(rng));
            }
            8 => {
                let (a, z) = (random_key(rng), random_key(rng));
                b.delete_range(a.clone().min(z.clone()), a.max(z));
            }
            _ => {
                let key = random_key(rng);
                b.delete_prefix(key[..rng.gen_range(0..=key.len())].to_vec());
            }
        }
    }
    b
}

fn random_bound(rng: &mut Rng) -> Bound<Vec<u8>> {
    match rng.gen_range(0..3u32) {
        0 => Bound::Included(random_key(rng)),
        1 => Bound::Excluded(random_key(rng)),
        _ => Bound::Unbounded,
    }
}
