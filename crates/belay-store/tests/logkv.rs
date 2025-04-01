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

#[test]
fn concurrent_writers_share_syncs_and_never_lose_acknowledged_writes() {
    let fs = MemFs::new();
    let kv = Arc::new(open(opts(&fs).segment_size(4096)));
    let writers: Vec<_> = (0..8)
        .map(|t| {
            let kv = Arc::clone(&kv);
            thread::spawn(move || {
                (0..50)
                    .map(|i| {
                        let mut b = WriteBatch::new();
                        b.put(format!("t{t}/{i:03}"), format!("{t}:{i}"));
                        kv.write(b).expect("write")
                    })
                    .collect::<Vec<u64>>()
            })
        })
        .collect();
    let mut all = Vec::new();
    for w in writers {
        let seqs = w.join().expect("writer thread");
        assert!(seqs.windows(2).all(|p| p[0] < p[1]), "per-thread order");
        all.extend(seqs);
    }
    all.sort_unstable();
    assert_eq!(all, (1..=400).collect::<Vec<u64>>());

    let stats = kv.stats();
    assert_eq!(stats.batches, 400);
    assert_eq!(stats.ops, 400);
    assert!(stats.commit_groups >= 1 && stats.commit_groups <= 400);
    // One sync per group, plus one per sealed segment.
    assert!(stats.wal_syncs <= stats.commit_groups + stats.wal_segments);
    assert_eq!(stats.durable_seq, 400);

    let recovered = open(opts(&fs.crash_copy(&Crash::PowerLoss)));
    assert_eq!(recovered.last_seq(), 400);
    assert_eq!(recovered.count(KeyRange::all()).expect("count"), 400);
}

#[test]
fn interval_policy_syncs_in_the_background() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Interval(Duration::from_millis(2))));
    let seq = kv.write(batch(1)).expect("write");
    eventually("the background sync", || kv.stats().durable_seq >= seq);
    let recovered = open(opts(&fs.crash_copy(&Crash::PowerLoss)));
    assert_eq!(dump(&recovered), states(1)[1]);
    assert!(kv.stats().wal_syncs >= 1);
}

#[test]
fn never_policy_syncs_only_when_asked() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Never));
    write_batches(&kv, 1..=3);
    assert_eq!(kv.stats().durable_seq, 0);
    // Visible and safe from a process crash, but not from power loss.
    assert_eq!(dump(&kv), states(3)[3]);
    assert!(dump(&open(opts(&fs.crash_copy(&Crash::PowerLoss)))).is_empty());
    assert_eq!(
        dump(&open(opts(&fs.crash_copy(&Crash::ProcessExit)))),
        states(3)[3]
    );
    kv.flush().expect("flush");
    assert_eq!(kv.stats().durable_seq, 3);
    assert_eq!(
        dump(&open(opts(&fs.crash_copy(&Crash::PowerLoss)))),
        states(3)[3]
    );
    write_batches(&kv, 4..=5);
    kv.close().expect("close syncs");
    assert_eq!(
        dump(&open(opts(&fs.crash_copy(&Crash::PowerLoss)))),
        states(5)[5]
    );
}

#[test]
fn checkpoints_replace_the_log_with_a_snapshot() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).segment_size(256));
    write_batches(&kv, 1..=30);
    let before = inspect::list_files(&fs, &dir())
        .expect("list")
        .segments
        .len() as u64;
    let info = kv.checkpoint().expect("checkpoint");
    assert!(info.written);
    assert_eq!(info.seq, 30);
    assert_eq!(info.keys, states(30)[30].len() as u64);
    assert_eq!(info.segments_removed, before);
    let files = inspect::list_files(&fs, &dir()).expect("list");
    assert_eq!(files.snapshots.len(), 1);
    assert_eq!(files.snapshots[0].seq, 30);
    assert_eq!(files.segments.len(), 1, "only the fresh active segment");
    assert_eq!(files.segments[0].seq, 31);
    assert_eq!(info.snapshot_bytes, files.snapshots[0].size);

    // Nothing new: nothing to do.
    let again = kv.checkpoint().expect("checkpoint");
    assert!(!again.written);
    assert_eq!(again.seq, 30);

    write_batches(&kv, 31..=35);
    let info = kv.checkpoint().expect("checkpoint");
    assert_eq!(info.seq, 35);
    let files = inspect::list_files(&fs, &dir()).expect("list");
    assert_eq!(files.snapshots.len(), 1, "the older snapshot is deleted");
    let stats = kv.stats();
    assert_eq!((stats.checkpoints, stats.checkpoint_seq), (2, 35));
    kv.close().expect("close");

    let kv = open(opts(&fs));
    assert_eq!(kv.recovery().snapshot_seq, 35);
    assert_eq!(kv.recovery().batches_replayed, 0);
    assert_eq!(dump(&kv), states(35)[35]);
}

#[test]
fn writes_continue_while_a_checkpoint_is_written() {
    let fs = MemFs::new();
    let kv = Arc::new(open(opts(&fs).segment_size(512)));
    write_batches(&kv, 1..=20);
    let writer = {
        let kv = Arc::clone(&kv);
        thread::spawn(move || {
            for i in 0..200u32 {
                let mut b = WriteBatch::new();
                b.put(format!("live/{i:03}"), "x");
                kv.write(b).expect("write during checkpoint");
            }
        })
    };
    for _ in 0..5 {
        kv.checkpoint().expect("checkpoint");
    }
    writer.join().expect("writer");
    let expected = dump(&*kv);
    let recovered = open(opts(&fs.crash_copy(&Crash::PowerLoss)));
    assert_eq!(dump(&recovered), expected);
    assert_eq!(recovered.last_seq(), 220);
}

#[test]
fn automatic_checkpoints_bound_the_log() {
    let fs = MemFs::new();
    let kv = open(
        Options::new()
            .fs(fs.clone())
            .segment_size(512)
            .checkpoint_every(2048),
    );
    write_batches(&kv, 1..=150);
    eventually("an automatic checkpoint", || kv.stats().checkpoints >= 1);
    kv.close().expect("close");
    let kv = open(opts(&fs));
    assert!(kv.recovery().snapshot_seq > 0);
    assert_eq!(dump(&kv), states(150)[150]);
}
