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
