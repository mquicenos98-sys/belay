//! Crash recovery: for every point at which a crash can cut the history
//! short, recovery must produce the state after some prefix of the
//! batches, and that prefix must include every batch that was
//! acknowledged as durable.

mod support;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::sync::lock;
use belay_store::vfs::{Crash, MemFs, Vfs};
use belay_store::{inspect, KeyRange, Kv, LogKv, RecoveryMode, StoreError, SyncPolicy, WriteBatch};

use support::{batch, dir, drop_quietly, dump, last_segment, open, opts, record_ends, states};

/// Highest sequence number whose record lies entirely within `len` bytes.
fn complete_through(ends: &[(u64, u64)], len: u64) -> u64 {
    ends.iter()
        .filter(|(_, end)| *end <= len)
        .map(|(seq, _)| *seq)
        .max()
        .unwrap_or(0)
}

#[test]
fn every_torn_offset_of_the_unsynced_tail_recovers_an_exact_prefix() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Never));
    for i in 1..=5 {
        kv.write(batch(i)).expect("write");
    }
    kv.flush().expect("flush");
    for i in 6..=12 {
        kv.write(batch(i)).expect("write");
    }
    let seg = last_segment(&fs, &dir());
    let synced = fs.synced_len(&seg).expect("synced");
    let len = fs.file_len(&seg).expect("len");
    let ends = record_ends(&fs, &seg);
    assert_eq!(complete_through(&ends, synced), 5);
    assert_eq!(complete_through(&ends, len), 12);
    let states = states(13);

    for keep in 0..=(len - synced) {
        let crashed = fs.crash_copy(&Crash::TornWrite {
            path: seg.clone(),
            keep: keep as usize,
        });
        let expected = complete_through(&ends, synced + keep);
        let kv = open(opts(&crashed));
        assert_eq!(kv.last_seq(), expected, "keep {keep}");
        assert_eq!(dump(&kv), states[expected as usize], "keep {keep}");
        let clean_cut = ends.iter().any(|(_, end)| *end == synced + keep);
        assert_eq!(
            kv.recovery().torn_tails,
            u64::from(!clean_cut),
            "keep {keep}"
        );
        // The store keeps working from there, and a second recovery agrees.
        assert_eq!(kv.write(batch(expected + 1)).expect("write"), expected + 1);
        kv.close().expect("close");
        let again = open(opts(&crashed));
        assert_eq!(
            dump(&again),
            states[expected as usize + 1],
            "reopen after keep {keep}"
        );
        assert_eq!(again.recovery().torn_tails, 0);
    }
}

#[test]
fn a_batch_whose_sync_failed_is_all_or_nothing() {
    let fs = MemFs::new();
    let kv = open(opts(&fs));
    for i in 1..=6 {
        kv.write(batch(i)).expect("acknowledged");
    }
    fs.fail_syncs(true);
    assert!(matches!(kv.write(batch(7)), Err(StoreError::Failed { .. })));
    let seg = last_segment(&fs, &dir());
    let unsynced = fs.file_len(&seg).expect("len") - fs.synced_len(&seg).expect("synced");
    assert!(unsynced > 0, "the failed batch was written but not synced");
    let states = states(7);
    for keep in 0..=unsynced {
        let crashed = fs.crash_copy(&Crash::TornWrite {
            path: seg.clone(),
            keep: keep as usize,
        });
        let kv = open(opts(&crashed));
        let expected = if keep == unsynced { 7 } else { 6 };
        assert_eq!(kv.last_seq(), expected, "keep {keep}");
        assert_eq!(dump(&kv), states[expected as usize]);
    }
    fs.fail_syncs(false);
    drop_quietly(kv);
}

#[test]
fn zeroed_or_missing_pages_anywhere_in_the_unsynced_tail_truncate_there() {
    // Unsynced pages reach the disk in any order, so any record of the
    // unsynced tail may be missing while later ones are intact.
    let fs = MemFs::new();
    let kv = open(opts(&fs).sync(SyncPolicy::Never));
    for i in 1..=4 {
        kv.write(batch(i)).expect("write");
    }
    kv.flush().expect("flush");
    for i in 5..=10 {
        kv.write(batch(i)).expect("write");
    }
    let seg = last_segment(&fs, &dir());
    let records = inspect::read_segment(&fs, &seg, true)
        .expect("dump")
        .records;
    let states = states(10);
    for victim in records.iter().filter(|r| r.seq > 4) {
        for fill in [0x00u8, 0xA5] {
            let crashed = fs.crash_copy(&Crash::ProcessExit);
            crashed
                .overwrite(
                    &seg,
                    victim.offset as usize,
                    &vec![fill; victim.len as usize],
                )
                .expect("damage");
            let kv = open(opts(&crashed));
            let expected = victim.seq - 1;
            assert_eq!(kv.last_seq(), expected, "victim {}", victim.seq);
            assert_eq!(dump(&kv), states[expected as usize]);
            assert_eq!(kv.recovery().torn_tails, 1);
        }
    }
    // A zero-filled extension past the last record is also just a torn tail.
    let crashed = fs.crash_copy(&Crash::ProcessExit);
    let mut f = crashed.open_append(&seg).expect("append");
    std::io::Write::write_all(&mut f, &[0u8; 4096]).expect("zeros");
    drop(f);
    let kv = open(opts(&crashed));
    assert_eq!(dump(&kv), states[10]);
    assert_eq!(kv.recovery().torn_bytes, 4096);
}

#[test]
fn any_damaged_byte_in_a_sealed_segment_is_reported() {
    let fs = MemFs::new();
    let kv = open(opts(&fs).segment_size(200));
    for i in 1..=12 {
        kv.write(batch(i)).expect("write");
    }
    kv.close().expect("close");
    let files = inspect::list_files(&fs, &dir()).expect("list");
    assert!(files.segments.len() >= 3);
    let sealed = files.segments[0].path.clone();
    let len = fs.file_len(&sealed).expect("len") as usize;
    let original = fs.read(&sealed).expect("read");
    for offset in 0..len {
        let damaged = fs.deep_copy();
        damaged
            .overwrite(&sealed, offset, &[original[offset] ^ 0x40])
            .expect("flip");
        match LogKv::open(dir(), opts(&damaged)) {
            Err(StoreError::Corrupt { file, .. }) => assert_eq!(file, sealed, "offset {offset}"),
            other => panic!("offset {offset}: expected corruption, got {other:?}"),
        }
        let salvaged = LogKv::open(dir(), opts(&damaged).recovery(RecoveryMode::SkipCorrupt))
            .expect("skip-corrupt opens");
        assert!(salvaged.recovery().skipped_regions > 0, "offset {offset}");
        assert_eq!(salvaged.last_seq(), 12, "later segments are still replayed");
    }
}

#[test]
fn a_crash_after_any_io_operation_keeps_every_acknowledged_write() {
    let states = states(14);
    let mut finished = false;
    for budget in 0..400 {
        let fs = MemFs::new();
        fs.fail_after_ops(Some(budget));
        let mut acked = 0;
        // Opening counts against the budget too, so crashes while the
        // store is being created are covered.
        let kv = LogKv::open(dir(), opts(&fs).segment_size(200)).ok();
        if let Some(kv) = &kv {
            for i in 1..=14 {
                match kv.write(batch(i)) {
                    Ok(seq) => {
                        assert_eq!(seq, i);
                        acked = i;
                    }
                    Err(_) => break,
                }
                if i == 7 && kv.checkpoint().is_err() {
                    break;
                }
            }
        }
        for crash in [Crash::PowerLoss, Crash::ProcessExit] {
            let after = fs.crash_copy(&crash);
            let recovered = open(opts(&after).segment_size(200));
            let seq = recovered.last_seq();
            assert!(
                seq >= acked,
                "budget {budget} {crash:?}: lost acknowledged batches"
            );
            assert_eq!(
                dump(&recovered),
                states[seq as usize],
                "budget {budget} {crash:?}"
            );
        }
        if let Some(kv) = kv {
            fs.crash(&Crash::PowerLoss);
            drop_quietly(kv);
        }
        if acked == 14 {
            finished = true;
            break;
        }
    }
    assert!(finished, "the whole workload fits in the operation budget");
}

#[test]
fn concurrent_writers_interrupted_by_power_loss_recover_a_prefix() {
    let fs = MemFs::new();
    let kv = Arc::new(open(opts(&fs).segment_size(1024)));
    let acked: Arc<Mutex<Vec<(u64, String)>>> = Arc::default();
    let writers: Vec<_> = (0..4)
        .map(|t| {
            let kv = Arc::clone(&kv);
            let acked = Arc::clone(&acked);
            thread::spawn(move || {
                for i in 0..20_000 {
                    let key = format!("t{t}/{i:04}");
                    let mut b = WriteBatch::new();
                    b.put(key.clone(), "x");
                    match kv.write(b) {
                        Ok(seq) => lock(&acked).push((seq, key)),
                        Err(_) => return,
                    }
                }
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    // Spin rather than sleep: a coarse sleep could let the writers finish
    // before the crash.
    while lock(&acked).len() < 200 {
        assert!(Instant::now() < deadline, "writers made no progress");
        thread::yield_now();
    }
    fs.crash(&Crash::PowerLoss);
    for w in writers {
        w.join().expect("writer");
    }
    let kv = Arc::try_unwrap(kv).expect("writers are done");
    assert!(matches!(
        kv.put(b"late", b"x"),
        Err(StoreError::Failed { .. })
    ));
    drop_quietly(kv);

    let recovered = open(opts(&fs));
    let last = recovered.last_seq();
    let acked = lock(&acked).clone();
    assert!(acked.len() < 80_000, "the crash interrupted the writers");
    for (seq, key) in &acked {
        assert!(
            *seq <= last,
            "acknowledged batch {seq} lost (recovered through {last})"
        );
        assert!(recovered.contains(key.as_bytes()).expect("get"), "{key}");
    }
    // Every batch adds one distinct key, so exactly `last` keys means
    // exactly batches 1..=last: a prefix with no holes.
    assert_eq!(
        recovered.count(KeyRange::all()).expect("count"),
        last as usize
    );
}
