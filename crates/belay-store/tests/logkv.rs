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

#[test]
fn a_torn_tail_is_truncated_with_a_warning() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Never));
    write_batches(&kv, 1..=3);
    kv.flush().expect("flush");
    write_batches(&kv, 4..=6);
    let seg = support::last_segment(&fs, &dir());
    let crashed = fs.crash_copy(&Crash::TornWrite {
        path: seg.clone(),
        keep: 10,
    });

    let (kv, capture) = open_captured(opts(&crashed));
    let kv = kv.expect("a torn tail is not an error");
    assert_eq!(dump(&kv), states(3)[3]);
    let rec = kv.recovery();
    assert_eq!(rec.torn_tails, 1);
    assert_eq!(rec.torn_bytes, 10);
    assert!(rec.lost_data());
    assert_eq!(kv.stats().recovery, *rec);
    let warning = capture
        .records()
        .into_iter()
        .find(|r| r.message.contains("truncating torn log tail"))
        .expect("a warning is logged");
    assert_eq!(warning.field("bytes"), Some("10"));
    // The damage is gone: the next open is clean and numbering continues.
    assert_eq!(kv.write(batch(4)).expect("write"), 4);
    kv.close().expect("close");
    let kv = open(opts(&crashed));
    assert_eq!(kv.recovery().torn_tails, 0);
    assert_eq!(dump(&kv), states(4)[4]);
}

/// Offsets of the records in the segment at `path`.
fn record_offsets(fs: &MemFs, path: &std::path::Path) -> Vec<u64> {
    inspect::read_segment(fs, path, false)
        .expect("dump")
        .records
        .iter()
        .map(|r| r.offset)
        .collect()
}

#[test]
fn damage_in_a_sealed_segment_is_corruption() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).segment_size(256));
    write_batches(&kv, 1..=30);
    kv.close().expect("close");
    let files = inspect::list_files(&fs, &dir()).expect("list");
    let sealed = &files.segments[0].path;
    let offsets = record_offsets(&fs, sealed);
    fs.overwrite(sealed, offsets[1] as usize + 30, b"\x00\x00\x00")
        .expect("damage");

    match LogKv::open(dir(), opts(&fs)) {
        Err(StoreError::Corrupt {
            file,
            offset,
            reason,
        }) => {
            assert_eq!(&file, sealed);
            assert_eq!(offset, offsets[1]);
            assert!(reason.starts_with("checksum mismatch"), "{reason}");
        }
        other => panic!("expected corruption, got {other:?}"),
    }

    let (kv, capture) = open_captured(opts(&fs).recovery(RecoveryMode::SkipCorrupt));
    let kv = kv.expect("skip-corrupt opens");
    assert!(kv.recovery().skipped_regions >= 1);
    assert!(capture.contains("skipping corrupt data"));
    // Batch 2 is lost; every other batch was applied.
    let mut expected = states(1)[1].clone();
    for i in 3..=30 {
        support::apply(&mut expected, &batch(i));
    }
    assert_eq!(dump(&kv), expected);
    kv.close().expect("close");

    // The damage was retired by a checkpoint, so strict mode opens again.
    let kv = open(opts(&fs));
    assert_eq!(dump(&kv), expected);
    assert!(inspect::verify(&fs, &dir()).expect("verify").is_ok());
}

#[test]
fn damage_in_an_unsynced_group_is_a_torn_tail_even_with_intact_records_after_it() {
    // Out-of-order page writeback: batch 4's bytes never reached the disk
    // but batches 5 and 6, written later without a sync, did.
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Never));
    write_batches(&kv, 1..=3);
    kv.flush().expect("flush");
    write_batches(&kv, 4..=6);
    let crashed = fs.crash_copy(&Crash::ProcessExit);
    let seg = support::last_segment(&crashed, &dir());
    let offsets = record_offsets(&crashed, &seg);
    let len = (offsets[4] - offsets[3]) as usize;
    crashed
        .overwrite(&seg, offsets[3] as usize, &vec![0u8; len])
        .expect("zero batch 4");
    let kv = open(opts(&crashed));
    assert_eq!(dump(&kv), states(3)[3]);
    assert_eq!(kv.recovery().torn_tails, 1);
}

#[test]
fn a_corrupt_snapshot_falls_back_when_the_log_still_covers_it() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=10);
    kv.close().expect("close");
    // Take a snapshot on a copy and plant it next to the full log.
    let copy = fs.deep_copy();
    let kv = open(opts(&copy));
    kv.checkpoint().expect("checkpoint");
    kv.close().expect("close");
    let snap = inspect::list_files(&copy, &dir()).expect("list").snapshots[0].clone();
    let mut bytes = copy.read(&snap.path).expect("read");
    bytes[40] ^= 0xFF;
    let mut f = fs.create(&snap.path).expect("plant");
    f.write_all(&bytes).expect("write");
    drop(f);

    let kv = open(opts(&fs));
    assert_eq!(dump(&kv), states(10)[10]);
    assert_eq!(kv.recovery().bad_snapshots, 1);
    let files = inspect::list_files(&fs, &dir()).expect("list");
    assert!(
        files.other_files.iter().any(|n| n.ends_with(".db.corrupt")),
        "{files:?}"
    );
}

#[test]
fn a_corrupt_snapshot_without_log_coverage_is_data_loss() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=10);
    kv.checkpoint().expect("checkpoint");
    write_batches(&kv, 11..=12);
    kv.close().expect("close");
    let snap = inspect::list_files(&fs, &dir()).expect("list").snapshots[0].clone();
    fs.overwrite(&snap.path, 40, b"\xFF").expect("damage");

    let err = LogKv::open(dir(), opts(&fs)).expect_err("strict refuses");
    assert!(err.is_corruption(), "{err}");
    assert_eq!(err.path(), Some(snap.path.as_path()));

    let kv = open(opts(&fs).recovery(RecoveryMode::SkipCorrupt));
    let rec = kv.recovery().clone();
    assert_eq!(rec.bad_snapshots, 1);
    // Only what the log still holds: batches 11 and 12 on an empty base.
    let mut expected = support::Model::new();
    support::apply(&mut expected, &batch(11));
    support::apply(&mut expected, &batch(12));
    assert_eq!(dump(&kv), expected);
}

#[test]
fn a_missing_segment_is_a_gap() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).segment_size(256));
    write_batches(&kv, 1..=40);
    kv.close().expect("close");
    let files = inspect::list_files(&fs, &dir()).expect("list");
    assert!(files.segments.len() >= 3);
    fs.remove(&files.segments[1].path).expect("remove");
    let err = LogKv::open(dir(), opts(&fs)).expect_err("gap");
    assert!(err.to_string().contains("log gap"), "{err}");
    let kv = open(opts(&fs).recovery(RecoveryMode::SkipCorrupt));
    assert_eq!(kv.last_seq(), 40);
    assert!(kv.recovery().skipped_regions >= 1);
}

#[test]
fn leftover_temporary_snapshots_are_deleted_on_open() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=4);
    kv.close().expect("close");
    let tmp = dir().join("snap-00000000000000000004.db.tmp");
    let mut f = fs.create(&tmp).expect("create");
    f.write_all(b"BELAYSNP half a snapshot").expect("write");
    drop(f);
    let kv = open(opts(&fs));
    assert_eq!(dump(&kv), states(4)[4]);
    assert_eq!(kv.recovery().removed_files, 1);
    assert!(!fs.exists(&tmp).expect("exists"));
}

#[test]
fn a_failed_sync_stops_writes_until_reopen() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=2);
    fs.fail_syncs(true);
    let capture = belay_common::log::CaptureLogger::new();
    let err = capture
        .scope(|| kv.write(batch(3)))
        .expect_err("sync fails");
    assert!(matches!(err, StoreError::Failed { .. }), "{err}");
    assert!(err.to_string().contains("simulated sync failure"), "{err}");
    assert!(capture.contains("store stopped accepting writes"));
    fs.fail_syncs(false);
    // Stays failed even though the disk recovered: the kernel may have
    // dropped the unsynced pages.
    assert!(matches!(kv.write(batch(3)), Err(StoreError::Failed { .. })));
    assert!(matches!(kv.flush(), Err(StoreError::Failed { .. })));
    assert!(matches!(kv.checkpoint(), Err(StoreError::Failed { .. })));
    // Reads keep working.
    assert_eq!(dump(&kv), states(2)[2]);
    kv.close().expect("close");

    let kv = open(opts(&fs));
    let seq = kv.last_seq();
    assert!(
        seq == 2 || seq == 3,
        "the failed batch may or may not survive"
    );
    assert_eq!(dump(&kv), states(seq)[seq as usize]);
}

#[test]
fn a_closed_store_rejects_everything() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    kv.put(b"k", b"v").expect("put");
    kv.close().expect("close");
    kv.close().expect("closing twice is fine");
    assert!(kv.is_closed());
    assert!(matches!(kv.get(b"k"), Err(StoreError::Closed)));
    assert!(matches!(kv.scan(KeyRange::all()), Err(StoreError::Closed)));
    assert!(matches!(kv.put(b"k", b"w"), Err(StoreError::Closed)));
    assert!(matches!(kv.flush(), Err(StoreError::Closed)));
    assert!(matches!(kv.checkpoint(), Err(StoreError::Closed)));
}

#[test]
fn the_directory_lock_keeps_a_second_handle_out() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    assert!(matches!(
        LogKv::open(dir(), opts(&fs)),
        Err(StoreError::Locked { .. })
    ));
    drop(kv);
    open(opts(&fs));
}

#[test]
fn missing_directories_are_created_unless_told_otherwise() {
    let fs = MemFs::new();
    assert!(matches!(
        LogKv::open(dir(), opts(&fs).create_if_missing(false)),
        Err(StoreError::NotFound { .. })
    ));
    let kv = LogKv::open(dir().join("nested"), opts(&fs)).expect("created");
    assert_eq!(kv.dir(), dir().join("nested"));
}

#[test]
fn invalid_options_are_rejected_before_touching_the_disk() {
    let fs = MemFs::new();
    let err = LogKv::open(dir(), opts(&fs).segment_size(1)).expect_err("invalid");
    assert!(matches!(err, StoreError::InvalidOptions(_)));
    assert!(fs.paths().is_empty());
}

#[test]
fn empty_and_oversized_batches_consume_no_sequence_number() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    assert_eq!(kv.write(WriteBatch::new()).expect("empty"), 0);
    let mut big = WriteBatch::new();
    big.put(vec![1u8; MAX_KEY_LEN + 1], "v");
    assert!(matches!(kv.write(big), Err(StoreError::TooLarge { .. })));
    assert_eq!(kv.put(b"a", b"1").expect("put"), 1);
    assert_eq!(kv.write(WriteBatch::new()).expect("empty"), 1);
    assert_eq!(kv.stats().batches, 1);
}

#[test]
fn stats_describe_the_store() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    write_batches(&kv, 1..=5);
    let stats = kv.stats();
    let model = &states(5)[5];
    assert_eq!(stats.keys, model.len() as u64);
    assert_eq!(
        stats.live_bytes,
        model
            .iter()
            .map(|(k, v)| (k.len() + v.len()) as u64)
            .sum::<u64>()
    );
    assert_eq!((stats.last_seq, stats.durable_seq), (5, 5));
    assert_eq!(stats.batches, 5);
    assert_eq!(
        stats.ops,
        (1..=5).map(|i| batch(i).len() as u64).sum::<u64>()
    );
    assert_eq!(stats.commit_groups, 5);
    assert!(stats.wal_bytes_written > 0);
    assert_eq!(stats.wal_segments, 1);
    assert_eq!(stats.checkpoints, 0);
    assert!(format!("{kv:?}").contains("seq: 5"));
    assert_eq!(kv.sync_policy(), SyncPolicy::Always);
}

#[test]
fn dropping_the_store_closes_it() {
    let fs = MemFs::new();
    {
        let kv = open(opts(&fs).sync(SyncPolicy::Never));
        write_batches(&kv, 1..=3);
    }
    // Drop synced the log and released the lock.
    let kv = open(opts(&fs.crash_copy(&Crash::PowerLoss)));
    assert_eq!(dump(&kv), states(3)[3]);
    drop(kv);
    open(opts(&fs));
}
