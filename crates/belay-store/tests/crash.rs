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
