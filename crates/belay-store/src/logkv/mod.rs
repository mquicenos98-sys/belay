//! The durable store: an in-memory table backed by a write-ahead log and
//! periodic snapshots.
//!
//! The whole dataset lives in a `BTreeMap`. That is a deliberate choice:
//! belay stores job metadata, queue configuration and schedules, which are
//! small, and payloads are expected to be modest, so memory is not the
//! constraint and reads never touch the disk. Durability comes from the
//! log (see the `wal` module for its format and the torn-write rules) and
//! from snapshots written by checkpoints (see `snapshot`), which also bound
//! the log's size and the time recovery takes.
//!
//! # Locking
//!
//! Lock order is `checkpoint_lock`, then `wal`, then `visible`. The
//! commit-queue mutex and the background-thread mutex come last: they may
//! be taken while holding the others, but nothing else is acquired while
//! holding them. Readers take only `visible` (a read lock), so reads
//! proceed while the log is being written and synced.

mod checkpoint;

mod commit;

mod options;
