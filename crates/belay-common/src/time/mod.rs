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

const MILLIS_PER_SEC: i64 = 1_000;

const MILLIS_PER_DAY: i64 = 86_400_000;

/// A UTC instant with millisecond precision.
///
/// Internally an `i64` count of milliseconds since 1970-01-01T00:00:00Z,
/// clamped to `0000-01-01T00:00:00.000Z ..= 9999-12-31T23:59:59.999Z`.
/// Values before 1970 are negative. `Display` and `FromStr` use RFC 3339.
///
/// ```
/// use belay_common::time::Timestamp;
/// use std::time::Duration;
///
/// let t: Timestamp = "2025-03-04T05:06:07.089Z".parse().unwrap();
/// assert_eq!(t.as_millis(), 1_741_064_767_089);
/// assert_eq!((t + Duration::from_secs(60)).to_string(), "2025-03-04T05:07:07.089Z");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 1970-01-01T00:00:00.000Z.
    pub const EPOCH: Timestamp = Timestamp(0);
    /// 0000-01-01T00:00:00.000Z, the earliest representable instant.
    pub const MIN: Timestamp = Timestamp(MIN_MILLIS);
    /// 9999-12-31T23:59:59.999Z, the latest representable instant. Useful as
    /// "never" for deadlines.
    pub const MAX: Timestamp = Timestamp(MAX_MILLIS);

    /// Creates a timestamp from milliseconds since the epoch, saturating to
    /// [`MIN`](Self::MIN)/[`MAX`](Self::MAX) outside the supported range.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Timestamp {
        if ms < MIN_MILLIS {
            Timestamp::MIN
        } else if ms > MAX_MILLIS {
            Timestamp::MAX
        } else {
            Timestamp(ms)
        }
    }

    /// Like [`from_millis`](Self::from_millis) but returns `None` instead of
    /// saturating.
    #[must_use]
    pub const fn try_from_millis(ms: i64) -> Option<Timestamp> {
        if ms < MIN_MILLIS || ms > MAX_MILLIS {
            None
        } else {
            Some(Timestamp(ms))
        }
    }

    /// Creates a timestamp from whole seconds since the epoch (saturating).
    #[must_use]
    pub const fn from_secs(secs: i64) -> Timestamp {
        Timestamp::from_millis(secs.saturating_mul(MILLIS_PER_SEC))
    }

    /// Milliseconds since the epoch (negative before 1970).
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// Whole seconds since the epoch, rounded towards negative infinity so
    /// that `-1ms` is second `-1`, consistent with the civil breakdown.
    #[must_use]
    pub const fn as_secs(self) -> i64 {
        self.0.div_euclid(MILLIS_PER_SEC)
    }

    /// The millisecond within the current second (`0..=999`).
    #[must_use]
    pub const fn subsec_millis(self) -> u32 {
        self.0.rem_euclid(MILLIS_PER_SEC) as u32
    }

    /// Reads the system clock.
    ///
    /// Code that needs to be testable should take a [`SharedClock`] instead.
    #[must_use]
    pub fn now() -> Timestamp {
        Timestamp::from_system_time(SystemTime::now())
    }

    /// Converts from `SystemTime`, flooring to the millisecond (also before
    /// the epoch) and saturating outside the supported range.
    #[must_use]
    pub fn from_system_time(t: SystemTime) -> Timestamp {
        match t.duration_since(UNIX_EPOCH) {
            Ok(after) => Timestamp::from_millis(millis_i64(after)),
            Err(err) => {
                let before = err.duration();
                let partial = u128::from(before.subsec_nanos() % 1_000_000 != 0);
                let ms = before.as_millis().saturating_add(partial);
                Timestamp::from_millis(i64::try_from(ms).map_or(i64::MIN, |m| -m))
            }
        }
    }

    /// Converts to `SystemTime`.
    #[must_use]
    pub fn to_system_time(self) -> SystemTime {
        let magnitude = Duration::from_millis(self.0.unsigned_abs());
        if self.0 >= 0 {
            UNIX_EPOCH + magnitude
        } else {
            UNIX_EPOCH - magnitude
        }
    }

    /// Adds `d` (truncated to whole milliseconds), or `None` past [`MAX`](Self::MAX).
    #[must_use]
    pub fn checked_add(self, d: Duration) -> Option<Timestamp> {
        let ms = i64::try_from(d.as_millis()).ok()?;
        Timestamp::try_from_millis(self.0.checked_add(ms)?)
    }

    /// Subtracts `d` (truncated to whole milliseconds), or `None` before [`MIN`](Self::MIN).
    #[must_use]
    pub fn checked_sub(self, d: Duration) -> Option<Timestamp> {
        let ms = i64::try_from(d.as_millis()).ok()?;
        Timestamp::try_from_millis(self.0.checked_sub(ms)?)
    }

    /// Adds `d`, saturating at [`MAX`](Self::MAX).
    #[must_use]
    pub fn saturating_add(self, d: Duration) -> Timestamp {
        Timestamp::from_millis(self.0.saturating_add(millis_i64(d)))
    }

    /// Subtracts `d`, saturating at [`MIN`](Self::MIN).
    #[must_use]
    pub fn saturating_sub(self, d: Duration) -> Timestamp {
        Timestamp::from_millis(self.0.saturating_sub(millis_i64(d)))
    }

    /// The time elapsed from `earlier` to `self`, or `None` if `earlier` is
    /// later than `self`.
    #[must_use]
    pub fn duration_since(self, earlier: Timestamp) -> Option<Duration> {
        // Both values are inside the clamped range, so this cannot overflow.
        let diff = self.0 - earlier.0;
        u64::try_from(diff).ok().map(Duration::from_millis)
    }

    /// Like [`duration_since`](Self::duration_since) but returns zero instead
    /// of `None`.
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Timestamp) -> Duration {
        self.duration_since(earlier).unwrap_or(Duration::ZERO)
    }

    /// Formats as RFC 3339 in UTC with millisecond precision, e.g.
    /// `2025-03-04T05:06:07.089Z`. The output always has the same length, so
    /// formatted timestamps sort chronologically as strings.
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        self.to_string()
    }

    /// Parses an RFC 3339 date-time.
    ///
    /// Accepts `T`, `t` or a space between date and time; `Z`, `z`, `+hh:mm`
    /// or `-hh:mm` offsets; and 1 to 9 fractional-second digits, which are
    /// truncated to milliseconds. Leap seconds (`:60`) are rejected because
    /// they cannot be represented.
    ///
    /// ```
    /// use belay_common::time::Timestamp;
    ///
    /// let a = Timestamp::parse_rfc3339("2024-02-29T23:30:00-01:30").unwrap();
    /// assert_eq!(a.to_rfc3339(), "2024-03-01T01:00:00.000Z");
    /// assert!(Timestamp::parse_rfc3339("2023-02-29T00:00:00Z").is_err());
    /// ```
    pub fn parse_rfc3339(s: &str) -> Result<Timestamp, TimeError> {
        rfc3339::parse(s)
    }

    /// Parses a calendar date `YYYY-MM-DD` as midnight UTC.
    pub fn parse_date(s: &str) -> Result<Timestamp, TimeError> {
        rfc3339::parse_date_only(s)
    }

    /// Breaks the timestamp down into UTC calendar fields.
    #[must_use]
    pub fn to_civil(self) -> Civil {
        let days = self.0.div_euclid(MILLIS_PER_DAY);
        let ms_of_day = self.0.rem_euclid(MILLIS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let secs = ms_of_day / MILLIS_PER_SEC;
        Civil {
            year,
            month,
            day,
            hour: (secs / 3600) as u8,
            minute: (secs / 60 % 60) as u8,
            second: (secs % 60) as u8,
            millis: (ms_of_day % MILLIS_PER_SEC) as u16,
            weekday: Weekday::from_days_since_epoch(days),
        }
    }

    /// Builds a timestamp from UTC calendar fields, validating each one.
    ///
    /// ```
    /// use belay_common::time::Timestamp;
    ///
    /// let t = Timestamp::from_civil(2000, 1, 1, 0, 0, 0, 0).unwrap();
    /// assert_eq!(t.as_secs(), 946_684_800);
    /// assert!(Timestamp::from_civil(2000, 13, 1, 0, 0, 0, 0).is_err());
    /// ```
    pub fn from_civil(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millis: u16,
    ) -> Result<Timestamp, TimeError> {
        check_field("year", i64::from(year), 0, 9999)?;
        check_field("month", i64::from(month), 1, 12)?;
        let dim = days_in_month(year, month);
        check_field("day", i64::from(day), 1, i64::from(dim))?;
        check_field("hour", i64::from(hour), 0, 23)?;
        check_field("minute", i64::from(minute), 0, 59)?;
        if second == 60 {
            return Err(TimeError::LeapSecond);
        }
        check_field("second", i64::from(second), 0, 59)?;
        check_field("millisecond", i64::from(millis), 0, 999)?;
        let days = days_from_civil(year, month, day);
        let secs = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
        let ms = days * MILLIS_PER_DAY + secs * MILLIS_PER_SEC + i64::from(millis);
        Timestamp::try_from_millis(ms).ok_or(TimeError::OutOfRange)
    }

    /// Midnight UTC at the start of the given date.
    pub fn from_date(year: i32, month: u8, day: u8) -> Result<Timestamp, TimeError> {
        Timestamp::from_civil(year, month, day, 0, 0, 0, 0)
    }
}

/// Converts a duration to whole milliseconds, saturating at `i64::MAX`.
fn millis_i64(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn check_field(field: &'static str, value: i64, min: i64, max: i64) -> Result<(), TimeError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(TimeError::FieldRange {
            field,
            value,
            min,
            max,
        })
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        rfc3339::write(*self, f)
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({self})")
    }
}

impl FromStr for Timestamp {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Timestamp::parse_rfc3339(s)
    }
}

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        Timestamp::from_system_time(t)
    }
}

/// Saturates at [`Timestamp::MAX`] rather than panicking.
impl Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Timestamp {
        self.saturating_add(rhs)
    }
}

/// Saturates at [`Timestamp::MIN`] rather than panicking.
impl Sub<Duration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: Duration) -> Timestamp {
        self.saturating_sub(rhs)
    }
}

impl AddAssign<Duration> for Timestamp {
    fn add_assign(&mut self, rhs: Duration) {
        *self = self.saturating_add(rhs);
    }
}
