//! RFC 3339 (`date-time` production of section 5.6) text form of [`Timestamp`].
//!
//! Output is always UTC with exactly three fractional digits so that the
//! strings have a fixed width and sort like the instants they denote. Input
//! is more permissive, as the RFC allows: any offset, a space instead of `T`,
//! lowercase `t`/`z`, and up to nine fractional digits.

use std::fmt;

use super::{check_field, days_from_civil, days_in_month, TimeError, Timestamp};
use super::{MILLIS_PER_DAY, MILLIS_PER_SEC};

pub(super) fn write(ts: Timestamp, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let c = ts.to_civil();
    // The clamped range guarantees 0 <= year <= 9999, so `{:04}` is exact.
    write!(
        f,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.millis
    )
}
