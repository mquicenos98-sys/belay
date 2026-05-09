//! Log output in tests: a quiet default, and [`LogCapture`] for asserting
//! on what the server, the engine and workers logged.
//!
//! belay logs through one process-wide logger, and a harness runs its
//! server and workers on threads of their own, so thread-scoped capture
//! ([`CaptureLogger::scope`]) cannot see their records. The kit installs a
//! tee as the global logger instead: every record goes to each active
//! [`LogCapture`] and, if its level passes `BELAY_TEST_LOG`, to stderr.
//!
//! `BELAY_TEST_LOG` takes the usual level directives (`debug`,
//! `info,belay_queue=trace`); unset, nothing reaches stderr, because the
//! failures end-to-end tests provoke on purpose (a worker cut off with a
//! result it cannot deliver, a torn log tail after a simulated crash) are
//! logged as the errors they would be in production.
//!
//! Tests in one binary run in parallel and share the logger, so a capture
//! also sees other tests' records: assert on records that carry something
//! unique to the test (a job id, a token name, a queue).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use belay_common::log::{
    self, CaptureLogger, CapturedRecord, Filter, Level, LevelFilter, Logger, Metadata, Record,
    StreamLogger,
};
use belay_common::sync::lock;

use crate::error::Result;
use crate::wait::eventually_some;

/// The environment variable selecting what the kit logs to stderr.
pub const ENV_TEST_LOG: &str = "BELAY_TEST_LOG";

/// Installs the kit's global logger, once per process. Harnesses call it
/// when they start; call it directly in tests that log without one.
pub fn init_test_logging() {
    tee();
}

/// Records everything logged at `level` or above, on any thread, while it
/// lives.
///
/// ```
/// use belay_testkit::LogCapture;
///
/// let capture = LogCapture::start();
/// belay_common::info!("lease expired"; job = "01JABC");
/// let record = capture.find(|r| r.field("job") == Some("01JABC")).expect("captured");
/// assert_eq!(record.message, "lease expired");
/// ```
pub struct LogCapture {
    id: u64,
    logger: CaptureLogger,
}
