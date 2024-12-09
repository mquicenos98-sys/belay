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
/// Longest `meta` value, in bytes.
pub const MAX_META_VALUE_BYTES: usize = 1024;
/// Longest uniqueness key, in bytes.
pub const MAX_UNIQUE_KEY_BYTES: usize = 256;
/// Longest period a `for:` uniqueness scope may cover.
pub const MAX_UNIQUE_FOR: Duration = Duration::from_secs(30 * DAY);

/// Longest `delay` (and longest `retry_in`) accepted.
pub const MAX_DELAY: Duration = Duration::from_secs(365 * DAY);
/// Shortest per-attempt execution timeout.
pub const MIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest per-attempt execution timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(7 * DAY);
/// Queue default for the per-attempt execution timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * MINUTE);

/// Error history kept per job; older entries are dropped first.
pub const MAX_ERROR_HISTORY: usize = 10;
/// Error messages are truncated to this many bytes when recorded.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 4096;
/// Longest error kind label.
pub const MAX_ERROR_KIND_LEN: usize = 64;
/// Longest progress message, in bytes.
pub const MAX_PROGRESS_MESSAGE_BYTES: usize = 1024;

/// Most jobs in one enqueue request.
pub const MAX_ENQUEUE_BATCH: usize = 1000;
/// Most jobs handed out by one lease request.
pub const MAX_LEASE_JOBS: u32 = 100;
/// Most queues named in one lease request.
pub const MAX_LEASE_QUEUES: usize = 32;
/// Shortest lease.
pub const MIN_LEASE: Duration = Duration::from_secs(1);
/// Longest lease (heartbeats extend it).
pub const MAX_LEASE: Duration = Duration::from_secs(HOUR);
/// Lease length when a request does not say.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(30);
/// Longest long-poll wait for a lease request.
pub const MAX_LEASE_WAIT: Duration = Duration::from_secs(60);

/// Page size when a list request does not say.
pub const DEFAULT_PAGE_LIMIT: u32 = 100;
/// Largest page size.
pub const MAX_PAGE_LIMIT: u32 = 1000;

/// Longest cron expression.
pub const MAX_CRON_LEN: usize = 256;
/// Longest time zone name or POSIX TZ string.
pub const MAX_TIMEZONE_LEN: usize = 64;
/// Largest random delay added to schedule fires.
pub const MAX_SCHEDULE_JITTER: Duration = Duration::from_secs(HOUR);
/// Largest `fire_all:<max>` catch-up count.
pub const MAX_MISFIRE_CATCH_UP: u32 = 1000;
/// Longest free-text description.
pub const MAX_DESCRIPTION_BYTES: usize = 1024;
/// Most excluded dates in a calendar.
pub const MAX_CALENDAR_DATES: usize = 1000;
/// Most time windows in a calendar.
pub const MAX_CALENDAR_WINDOWS: usize = 64;

/// Most steps in a workflow.
pub const MAX_WORKFLOW_STEPS: usize = 256;
/// Longest step or workflow name.
pub const MAX_STEP_NAME_LEN: usize = 64;
