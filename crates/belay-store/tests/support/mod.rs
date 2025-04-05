//! Helpers shared by the integration tests.
// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use belay_common::log::CaptureLogger;
use belay_store::inspect;
use belay_store::vfs::MemFs;
use belay_store::{KeyRange, Kv, LogKv, Op, Options, WriteBatch};

pub type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// The store directory used on every `MemFs`.
pub fn dir() -> PathBuf {
    PathBuf::from("store")
}

/// Keeps the global logger from writing recovery and checkpoint lines to
/// stderr, which the test harness does not capture. Tests that assert on
/// log output use a scoped `CaptureLogger`, which ignores this setting.
pub fn quiet_logs() {
    belay_common::log::set_max_level(belay_common::log::LevelFilter::Off);
}

/// Options on `fs` with checkpoints only when a test asks for one.
pub fn opts(fs: &MemFs) -> Options {
    quiet_logs();
    Options::new().fs(fs.clone()).manual_checkpoints()
}

pub fn open(opts: Options) -> LogKv {
    LogKv::open(dir(), opts).expect("opening the store")
}

/// Opens with log output captured, returning the store and the capture.
pub fn open_captured(opts: Options) -> (belay_store::Result<LogKv>, CaptureLogger) {
    let capture = CaptureLogger::new();
    let kv = capture.scope(|| LogKv::open(dir(), opts));
    (kv, capture)
}

/// Drops a store whose file system crashed under it, keeping the
/// expected "closing store failed" warning out of the test output.
pub fn drop_quietly(kv: LogKv) {
    CaptureLogger::new().scope(move || drop(kv));
}

/// Every entry of a store.
pub fn dump(kv: &dyn Kv) -> Model {
    kv.scan(KeyRange::all())
        .expect("scanning the store")
        .into_iter()
        .collect()
}

/// A deterministic batch number `i`: a put of its own key, an overwrite of
/// a shared key and, every few batches, a deletion.
pub fn batch(i: u64) -> WriteBatch {
    let mut b = WriteBatch::new();
    b.put(
        format!("key/{i:04}"),
        format!("value {i}").repeat(1 + (i % 3) as usize),
    )
    .put("counter", i.to_string());
    if i % 4 == 3 {
        b.delete(format!("key/{:04}", i - 2));
    }
    if i % 10 == 9 {
        b.delete_prefix(format!("key/{:03}", i / 10));
    }
    b
}

/// Applies a batch to a model map the way the store does.
pub fn apply(model: &mut Model, batch: &WriteBatch) {
    for op in batch.ops() {
        match op {
            Op::Put { key, value } => {
                model.insert(key.clone(), value.clone());
            }
            Op::Delete { key } => {
                model.remove(key);
            }
            Op::DeleteRange { start, end } => {
                model.retain(|k, _| k < start || end.as_ref().is_some_and(|e| k >= e));
            }
        }
    }
}

/// `states[n]` is the model after batches `1..=n` of [`batch`].
pub fn states(n: u64) -> Vec<Model> {
    let mut out = vec![Model::new()];
    let mut model = Model::new();
    for i in 1..=n {
        apply(&mut model, &batch(i));
        out.push(model.clone());
    }
    out
}

/// Path of the newest log segment.
pub fn last_segment(fs: &MemFs, dir: &Path) -> PathBuf {
    let files = inspect::list_files(fs, dir).expect("listing");
    files.segments.last().expect("a segment").path.clone()
}

/// End offsets of the valid records in a segment, in order.
pub fn record_ends(fs: &MemFs, path: &Path) -> Vec<(u64, u64)> {
    inspect::read_segment(fs, path, true)
        .expect("reading segment")
        .records
        .iter()
        .map(|r| (r.seq, r.end()))
        .collect()
}
