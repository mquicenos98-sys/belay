//! Every size and range limit enforced by [`Validate`](crate::Validate)
//! impls, in one place so that servers, clients and documentation agree.
//!
//! The limits exist to keep one bad client from hurting everyone else: job
//! documents are held in memory by the store, so a queue full of 50 MB
//! payloads is an outage waiting to happen. They are deliberately generous
//! for normal use.

use std::time::Duration;

const MINUTE: u64 = 60;

const HOUR: u64 = 60 * MINUTE;

const DAY: u64 = 24 * HOUR;

/// Largest job payload, measured as compact JSON.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Largest job result, measured as compact JSON.
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Lowest job priority.
pub const MIN_PRIORITY: i16 = -1000;

/// Highest job priority (runs first).
pub const MAX_PRIORITY: i16 = 1000;

/// Attempts a job gets when neither the spec nor its queue says otherwise.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 25;

/// Upper bound for `max_attempts`.
pub const MAX_ATTEMPTS: u32 = 1000;

/// Longest job kind (handler name).
pub const MAX_KIND_LEN: usize = 128;

/// Most tags per job.
pub const MAX_TAGS: usize = 16;

/// Longest tag.
pub const MAX_TAG_LEN: usize = 64;

/// Most `meta` entries per job or workflow.
pub const MAX_META_ENTRIES: usize = 32;

/// Longest `meta` key.
pub const MAX_META_KEY_LEN: usize = 64;
