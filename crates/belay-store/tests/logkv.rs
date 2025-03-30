//! Behavior of `LogKv` on the in-memory file system: persistence, sync
//! policies, segment rolling, checkpoints, recovery modes and error states.

mod support;

use std::io::Write as _;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_store::inspect;
use belay_store::vfs::{Crash, MemFs, Vfs};
use belay_store::{
    KeyRange, Kv, LogKv, Options, RecoveryMode, StoreError, SyncPolicy, WriteBatch, MAX_KEY_LEN,
};

use support::{batch, dir, dump, open, open_captured, opts, states};

fn write_batches(kv: &LogKv, range: std::ops::RangeInclusive<u64>) {
    for i in range {
        assert_eq!(kv.write(batch(i)).expect("write"), i);
    }
}

/// Polls `cond` for up to five seconds.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn reopening_restores_data_and_sequence_numbers() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=12);
    kv.close().expect("close");

    let kv = open(opts(&fs));
    assert_eq!(dump(&kv), states(12)[12]);
    assert_eq!(kv.last_seq(), 12);
    assert_eq!(kv.recovery().batches_replayed, 12);
    assert!(!kv.recovery().lost_data());
    assert_eq!(kv.write(batch(13)).expect("write"), 13);
}

#[test]
fn every_acknowledged_write_survives_power_loss_under_always() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).segment_size(300));
    for i in 1..=40 {
        kv.write(batch(i)).expect("write");
        if i % 7 == 0 {
            let after = fs.crash_copy(&Crash::PowerLoss);
            let recovered = open(opts(&after));
            assert_eq!(dump(&recovered), states(i)[i as usize], "after {i}");
        }
    }
    assert_eq!(kv.stats().durable_seq, 40);
}
