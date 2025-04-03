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

fn random_range(rng: &mut Rng) -> KeyRange {
    let mut range = if rng.gen_bool(0.3) {
        let key = random_key(rng);
        KeyRange::prefix(&key[..rng.gen_range(0..=key.len())])
    } else {
        KeyRange::from_bounds(random_bound(rng), random_bound(rng))
    };
    if rng.gen_bool(0.5) {
        range = range.limit(rng.gen_range(0..8usize));
    }
    if rng.gen_bool(0.5) {
        range = range.reverse();
    }
    range
}

/// What a scan of `range` over `model` must return.
fn expected_scan(model: &Model, range: &KeyRange) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = model
        .iter()
        .filter(|(k, _)| range.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if range.is_reverse() {
        entries.reverse();
    }
    entries.truncate(range.max_results().unwrap_or(usize::MAX));
    entries
}

/// Checks reads against the model: a few gets and one scan.
fn check_reads(kv: &dyn Kv, model: &Model, rng: &mut Rng, context: &str) {
    for _ in 0..3 {
        let key = random_key(rng);
        assert_eq!(
            kv.get(&key).expect("get"),
            model.get(&key).cloned(),
            "{context}"
        );
    }
    let range = random_range(rng);
    assert_eq!(
        kv.scan(range.clone()).expect("scan"),
        expected_scan(model, &range),
        "{context}: {range}"
    );
    assert_eq!(
        kv.count(range.clone()).expect("count"),
        expected_scan(model, &range).len(),
        "{context}: {range}"
    );
}

fn options(fs: &MemFs, segment_size: u64, sync: SyncPolicy) -> Options {
    opts(fs).segment_size(segment_size).sync(sync)
}

/// Runs random operations with the `Always` policy, under which every
/// acknowledged write must survive every kind of crash.
fn run_durable(seed: u64, steps: usize) {
    let mut rng = Rng::seed_from_u64(seed);
    let fs = MemFs::new();
    let segment_size = rng.gen_range(128..2048u64);
    let open = |fs: &MemFs| LogKv::open(dir(), options(fs, segment_size, SyncPolicy::Always));
    let mut kv = open(&fs).expect("open");
    let mut model = Model::new();
    let mut seq = 0;

    for step in 0..steps {
        let context = format!("seed {seed} step {step}");
        match rng.gen_range(0..100u32) {
            0..=54 => {
                let batch = random_batch(&mut rng);
                apply(&mut model, &batch);
                seq += 1;
                assert_eq!(kv.write(batch).expect("write"), seq, "{context}");
            }
            55..=79 => check_reads(&kv, &model, &mut rng, &context),
            80..=86 => {
                let info = kv.checkpoint().expect("checkpoint");
                assert_eq!(info.seq, seq, "{context}");
            }
            87..=91 => {
                kv.close().expect("close");
                kv = open(&fs).expect("reopen");
            }
            92..=96 => {
                fs.crash(&Crash::PowerLoss);
                drop_quietly(kv);
                kv = open(&fs).expect("recover after power loss");
            }
            _ => {
                // Everything was synced, so a torn write finds nothing to
                // tear.
                let path = last_segment(&fs, &dir());
                fs.crash(&Crash::TornWrite {
                    path,
                    keep: rng.gen_range(0..64usize),
                });
                drop_quietly(kv);
                kv = open(&fs).expect("recover after torn write");
            }
        }
        assert_eq!(kv.last_seq(), seq, "{context}");
    }
    assert_eq!(dump(&kv), model, "seed {seed}: final state");
    kv.close().expect("close");
    let report = inspect::verify(&fs, &dir()).expect("verify");
    assert!(report.is_ok(), "seed {seed}: {report}");
    assert_eq!(report.last_seq, seq);
}

#[test]
fn random_workloads_match_the_model_under_always() {
    for seed in 0..10 {
        run_durable(seed, 400);
    }
}

/// Runs random operations with a lazy policy, where a power loss may take
/// back acknowledged but unsynced batches: recovery must land exactly on
/// the state after some batch between the last sync and the last write.
fn run_lazy(seed: u64, steps: usize) {
    let mut rng = Rng::seed_from_u64(seed);
    let fs = MemFs::new();
    let segment_size = rng.gen_range(128..2048u64);
    let open = |fs: &MemFs| LogKv::open(dir(), options(fs, segment_size, SyncPolicy::Never));
    let mut kv = open(&fs).expect("open");
    // history[n] is the state after batch n.
    let mut history = vec![Model::new()];

    for step in 0..steps {
        let context = format!("seed {seed} step {step}");
        let current = history.len() as u64 - 1;
        match rng.gen_range(0..100u32) {
            0..=59 => {
                let batch = random_batch(&mut rng);
                let mut next = history.last().expect("non-empty").clone();
                apply(&mut next, &batch);
                history.push(next);
                assert_eq!(kv.write(batch).expect("write"), current + 1, "{context}");
            }
            60..=74 => {
                let model = history.last().expect("non-empty");
                check_reads(&kv, model, &mut rng, &context);
            }
            75..=79 => kv.flush().expect("flush"),
            80..=84 => {
                kv.checkpoint().expect("checkpoint");
            }
            85..=89 => {
                kv.close().expect("close");
                kv = open(&fs).expect("reopen");
                assert_eq!(kv.last_seq(), current, "{context}: close syncs");
            }
            _ => {
                let durable = kv.stats().durable_seq;
                let crash = if rng.gen_bool(0.5) {
                    Crash::PowerLoss
                } else {
                    Crash::TornWrite {
                        path: last_segment(&fs, &dir()),
                        keep: rng.gen_range(0..400usize),
                    }
                };
                fs.crash(&crash);
                drop_quietly(kv);
                kv = open(&fs).expect("recover");
                let recovered = kv.last_seq();
                assert!(
                    (durable..=current).contains(&recovered),
                    "{context}: recovered {recovered}, durable {durable}, written {current}"
                );
                history.truncate(recovered as usize + 1);
                assert_eq!(
                    dump(&kv),
                    history[recovered as usize],
                    "{context}: {crash:?}"
                );
            }
        }
    }
    assert_eq!(
        &dump(&kv),
        history.last().expect("non-empty"),
        "seed {seed}"
    );
}

#[test]
fn random_workloads_with_lazy_syncs_recover_a_prefix() {
    for seed in 100..110 {
        run_lazy(seed, 400);
    }
}

#[test]
fn background_checkpoints_do_not_disturb_the_model() {
    let mut rng = Rng::seed_from_u64(7);
    let fs = MemFs::new();
    let open = |fs: &MemFs| LogKv::open(dir(), opts(fs).segment_size(256).checkpoint_every(1024));
    let mut kv = open(&fs).expect("open");
    let mut model = Model::new();
    for step in 0..1500 {
        let batch = random_batch(&mut rng);
        apply(&mut model, &batch);
        kv.write(batch).expect("write");
        if step % 300 == 150 {
            // The log has long passed the threshold; make sure the
            // background thread got to it before crashing.
            let deadline = Instant::now() + Duration::from_secs(5);
            while kv.stats().checkpoints == 0 {
                assert!(Instant::now() < deadline, "no background checkpoint");
                thread::yield_now();
            }
        }
        if step % 300 == 299 {
            // Crash wherever the next background checkpoint happens to be.
            fs.crash(&Crash::PowerLoss);
            drop_quietly(kv);
            kv = open(&fs).expect("recover");
            assert_eq!(dump(&kv), model, "step {step}");
            assert!(kv.recovery().snapshot_seq > 0, "step {step}");
        }
    }
}

#[test]
fn memkv_and_logkv_agree() {
    let mut rng = Rng::seed_from_u64(42);
    let fs = MemFs::new();
    let log = LogKv::open(dir(), opts(&fs)).expect("open");
    let mem = MemKv::new();
    for _ in 0..300 {
        let batch = random_batch(&mut rng);
        assert_eq!(
            log.write(batch.clone()).expect("log write"),
            mem.write(batch).expect("mem write")
        );
        let range = random_range(&mut rng);
        assert_eq!(
            log.scan(range.clone()).expect("scan"),
            mem.scan(range).expect("scan")
        );
    }
    let (a, b) = (log.stats(), mem.stats());
    assert_eq!(
        (a.keys, a.live_bytes, a.last_seq),
        (b.keys, b.live_bytes, b.last_seq)
    );
}
