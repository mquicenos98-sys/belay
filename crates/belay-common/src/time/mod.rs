//! Wall-clock time as milliseconds since the Unix epoch.
//!
//! [`Timestamp`] is the single time type shared by the belay crates: it is what
//! gets persisted, compared and sent over the wire. Its range is limited to the
//! years 0000 through 9999 so that every value has an RFC 3339 form; arithmetic
//! saturates at those bounds instead of panicking.
//!
//! Time sources sit behind the [`Clock`] trait so that leases, schedules and
//! retention can be driven by a [`ManualClock`] in tests.

mod civil;

mod clock;

mod rfc3339;

use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use civil::{civil_from_days, days_from_civil, days_in_month, is_leap_year, Civil, Weekday};
pub use clock::{system_clock, Clock, ManualClock, SharedClock, SystemClock};

/// Milliseconds of 0000-01-01T00:00:00.000Z.
const MIN_MILLIS: i64 = -62_167_219_200_000;

/// Milliseconds of 9999-12-31T23:59:59.999Z.
const MAX_MILLIS: i64 = 253_402_300_799_999;
