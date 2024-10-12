//! Shared foundations for the belay crates.
//!
//! Everything here is built on the standard library alone and is small
//! enough to read in one sitting:
//!
//! | module | contents |
//! |---|---|
//! | [`time`] | [`Timestamp`] (UTC milliseconds, RFC 3339), civil calendar math, [`Clock`] / [`ManualClock`] |
//! | [`duration`] | `"1h30m"` style parsing and formatting, [`humanize`](duration::humanize) |
//! | [`id`] | [`Ulid`] and monotonic generators |
//! | [`rand`] | [`Rng`] (xoshiro256\*\*), [`thread_rng`](rand::thread_rng) |
//! | [`backoff`] | retry delay curves with jitter, text form `exp:1s*2..10m` |
//! | [`hash`] | FNV-1a, CRC-32C, XXH64, jump consistent hash |
//! | [`codec`] | little-endian / varint binary [`Encoder`](codec::Encoder) and [`Decoder`](codec::Decoder) |
//! | [`log`](mod@log) | structured logging and the [`info!`] family of macros |
//! | [`sync`] | [`Shutdown`], wait group, thread pool, ticker, poison-tolerant locking |
//! | [`fs`] | atomic file replacement, temp dirs, lock files |
//! | [`text`] | aligned tables and other command-line formatting |
//!
//! The most used items are re-exported at the crate root.
//!
//! ```
//! use belay_common::{info, parse_duration, Clock, ManualClock, Timestamp, Ulid};
//!
//! let clock = ManualClock::new("2025-03-04T05:06:07Z".parse::<Timestamp>().unwrap());
//! let lease = clock.now() + parse_duration("30s").unwrap();
//! let job = Ulid::generate();
//! info!("job leased"; job, expires = lease);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backoff;
pub mod codec;
pub mod duration;
mod error;
pub mod fs;
pub mod hash;
pub mod id;
pub mod log;
pub mod rand;
pub mod sync;
pub mod text;
pub mod time;

pub use backoff::BackoffPolicy;
pub use duration::{format_duration, parse_duration};
pub use error::{Error, Result};
pub use id::Ulid;
pub use rand::Rng;
pub use sync::Shutdown;
pub use time::{system_clock, Clock, ManualClock, SharedClock, SystemClock, Timestamp};
