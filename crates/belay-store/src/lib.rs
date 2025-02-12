//! Storage for belay: a sorted, byte-oriented key-value store with atomic
//! batch writes, and a durable implementation built on a write-ahead log.
//!
//! The store knows nothing about jobs. Keys and values are byte strings,
//! keys sort bytewise, and every write is a [`WriteBatch`] applied
//! atomically under the next sequence number. The rest of belay programs
//! against the [`Kv`] trait:
//!
//! | item | purpose |
//! |---|---|
//! | [`Kv`] | get, scan, atomic batch writes, flush, checkpoint, stats |
//! | [`MemKv`] | memory only, for tests and `belay dev` |
//! | [`LogKv`] | durable: in-memory table, write-ahead log, snapshots |
//! | [`WriteBatch`], [`KeyRange`] | describing writes and scans |
//! | [`keys`] | order-preserving encoding of composite keys |
//! | [`Tree`], [`Namespace`] | prefix-scoped views of a store |
//! | [`vfs`] | the file-system seam, with the crash-simulating [`MemFs`](vfs::MemFs) |
//! | [`inspect`] | offline listing, dumping and verification of a store directory |
//! | [`format`](mod@format) | the on-disk format and recovery rules |
//!
//! ```
//! use std::sync::Arc;
//! use belay_store::keys::KeyBuf;
//! use belay_store::vfs::MemFs;
//! use belay_store::{KeyRange, Kv, LogKv, Options, Tree};
//!
//! let kv: Arc<dyn Kv> = Arc::new(LogKv::open("data", Options::new().fs(MemFs::new()))?);
//! let jobs = Tree::open(Arc::clone(&kv), "job");
//! for id in [3u64, 1, 2] {
//!     let key = KeyBuf::new().u64(id).into_vec();
//!     jobs.put(&key, format!("job {id}").as_bytes())?;
//! }
//! // Keys encoded with KeyBuf sort numerically.
//! let newest = jobs.scan(KeyRange::all().reverse().limit(2))?;
//! assert_eq!(newest[0].1, b"job 3");
//! assert_eq!(newest[1].1, b"job 2");
//! # Ok::<(), belay_store::StoreError>(())
//! ```
//!
//! # Design
//!
//! [`LogKv`] keeps the whole dataset in a `BTreeMap`: belay's records are
//! small, so reads never touch the disk and the code stays simple. Writes
//! go to a log of CRC-32C-framed records in rolling segment files, with
//! group commit so that concurrent writers share syncs. Checkpoints write
//! a snapshot and delete the log it covers; opening loads the newest valid
//! snapshot and replays the log after it, truncating a torn tail left by a
//! crash. [`SyncPolicy`] trades durability for latency; every policy
//! recovers the state after a *prefix* of the acknowledged batches.
//!
//! All file access goes through [`vfs::Vfs`], and [`vfs::MemFs`] can drop
//! unsynced data or tear the last write at any byte, which is how the
//! tests check recovery at every possible crash point.
//!
//! # Limitations
//!
//! - The dataset must fit in memory, twice over while a checkpoint copies
//!   the table.
//! - One process per directory, enforced by a `LOCK` file. A lock left by
//!   a crashed process is detected as stale only on Linux; elsewhere the
//!   file must be deleted by hand.
//! - Scans return materialized vectors; page through large ranges with
//!   [`KeyRange::limit`] and [`KeyRange::after`].
//! - Damage confined to the very last record of the log looks like a torn
//!   write and is truncated with a warning rather than reported (see
//!   [`format`](mod@format)).
//! - After a failed log write or sync the store refuses further writes
//!   until it is reopened.
//! - No compression, encryption or replication.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod batch;
mod error;
mod files;
pub mod format;
pub mod inspect;
pub mod keys;
mod kv;
mod logkv;
mod mem;
mod range;
mod snapshot;
mod table;
pub mod tree;
pub mod vfs;
mod wal;

pub use batch::{Op, WriteBatch, MAX_BATCH_LEN, MAX_KEY_LEN, MAX_VALUE_LEN};
pub use error::{Error, Result, StoreError};
pub use kv::{CheckpointInfo, Kv, KvStats, RecoveryStats};
pub use logkv::{LogKv, Options, ParseOptionError, RecoveryMode, SyncPolicy};
pub use mem::MemKv;
pub use range::KeyRange;
pub use tree::{Namespace, Tree};
