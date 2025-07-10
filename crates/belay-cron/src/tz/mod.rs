//! Time zones: UTC, fixed offsets and POSIX rules, with an embedded table of
//! IANA names.
//!
//! A [`Tz`] converts between Unix seconds and wall-clock [`LocalDateTime`]s.
//! Only the *current* rule of a zone is modelled (see the [`zones`] table),
//! which is what a scheduler firing jobs from now on needs; past rule changes
//! are not.
//!
//! Local readings around a transition fall into three cases, reported by
//! [`Tz::from_local`] as a [`LocalResult`]: a normal reading maps to one
//! instant, a reading inside a backward jump (autumn) maps to two, and a
//! reading inside a forward jump (spring) maps to none.

mod posix;

pub(crate) mod zones;

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::civil::{LocalDateTime, MAX_NAIVE, MIN_NAIVE};
use crate::error::{CronError, Result};

pub use posix::PosixTz;
use posix::MAX_OFFSET;

/// A time zone.
///
/// Equality compares representations: a zone from the table carries its
/// name, so it differs from an unnamed [`Tz::Rule`] with the same rule.
///
/// ```
/// use belay_cron::{LocalDateTime, Tz};
///
/// let ny = Tz::named("America/New_York")?;
/// // 2025-07-04T16:00:00Z is noon EDT.
/// assert_eq!(ny.offset_at(1_751_644_800), -4 * 3600);
/// assert_eq!(ny.to_local(1_751_644_800).to_string(), "2025-07-04T12:00:00");
/// assert_eq!(ny.format_rfc3339(1_751_644_800), "2025-07-04T12:00:00-04:00");
/// # Ok::<(), belay_cron::CronError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Tz {
    /// Coordinated Universal Time.
    #[default]
    Utc,
    /// A constant offset in seconds east of UTC.
    Fixed(i32),
    /// A POSIX TZ rule, possibly with daylight saving time.
    Rule(PosixTz),
}

/// How a wall-clock reading maps onto instants in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalResult {
    /// Exactly one instant has this reading.
    Single(i64),
    /// The clocks went back and the reading occurred twice; earlier first.
    Ambiguous(i64, i64),
    /// The clocks jumped forward over this reading, so no instant has it.
    ///
    /// `after` is the transition instant, the first moment whose reading lies
    /// past the gap; `before` is the second just before it (`after - 1`).
    Gap {
        /// The last instant before the jump.
        before: i64,
        /// The transition instant, the first instant after the jump.
        after: i64,
    },
}

impl LocalResult {
    /// The instant, if the reading is unambiguous.
    #[must_use]
    pub fn single(self) -> Option<i64> {
        match self {
            LocalResult::Single(t) => Some(t),
            _ => None,
        }
    }

    /// The first instant at or past the reading: the single match, the
    /// earlier of two, or the end of a gap. This is the instant a cron
    /// schedule fires for the reading.
    #[must_use]
    pub fn earliest(self) -> i64 {
        match self {
            LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t,
            LocalResult::Gap { after, .. } => after,
        }
    }

    /// The single match, the later of two, or the last instant before a gap.
    #[must_use]
    pub fn latest(self) -> i64 {
        match self {
            LocalResult::Single(t) | LocalResult::Ambiguous(_, t) => t,
            LocalResult::Gap { before, .. } => before,
        }
    }
}
