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

/// A change of UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Transition {
    /// The first instant (Unix seconds) with the new offset.
    pub at: i64,
    /// Offset in seconds east of UTC before `at`.
    pub offset_before: i32,
    /// Offset in seconds east of UTC from `at` on.
    pub offset_after: i32,
}

impl Transition {
    /// Whether the clocks jump forward, skipping the readings in between.
    #[must_use]
    pub fn is_forward(&self) -> bool {
        self.offset_after > self.offset_before
    }
}

/// A stretch of time with a constant offset, bounded by the transitions on
/// either side (if any). The iteration code walks these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    pub offset: i32,
    pub start: Option<Transition>,
    pub end: Option<Transition>,
}

impl Tz {
    /// The zone for an IANA name from the embedded table (see
    /// [`zone_names`](crate::zone_names)). `UTC`, `Etc/UTC`, `GMT` and a few
    /// other aliases give [`Tz::Utc`].
    ///
    /// ```
    /// use belay_cron::Tz;
    ///
    /// assert_eq!(Tz::named("Asia/Kolkata")?.offset_at(0), 19_800);
    /// assert!(Tz::named("Asia/Calcuta").is_err());
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    pub fn named(name: &str) -> Result<Tz> {
        if zones::UTC_NAMES.contains(&name) {
            return Ok(Tz::Utc);
        }
        match zones::lookup(name) {
            Some((name, rule)) => Ok(Tz::Rule(PosixTz::parse(rule)?.with_name(name))),
            None => Err(CronError::UnknownZone {
                name: name.to_string(),
                suggestion: zones::suggest(name),
            }),
        }
    }

    /// Parses any supported spelling: a zone name, `Z`, a fixed offset
    /// (`+05:30`, `-0800`, `+01`) or a POSIX TZ rule (`CET-1CEST,M3.5.0,M10.5.0/3`,
    /// optionally prefixed with `:` for a name as in the `TZ` variable).
    ///
    /// Note that POSIX offsets count hours *west*: `UTC+3` is the rule for
    /// three hours behind UTC, while `+03:00` is three hours ahead.
    pub fn parse(input: &str) -> Result<Tz> {
        let s = input.trim();
        if s == "Z" || s == "z" {
            return Ok(Tz::Utc);
        }
        if zones::UTC_NAMES.contains(&s) || zones::lookup(s).is_some() {
            return Tz::named(s);
        }
        if let Some(name) = s.strip_prefix(':') {
            return Tz::named(name);
        }
        if s.starts_with(['+', '-']) {
            return parse_fixed(input, s).map(Tz::Fixed);
        }
        match PosixTz::parse(s) {
            Ok(rule) => Ok(Tz::Rule(rule)),
            // Something shaped like a zone name gets the friendlier error.
            Err(_) if looks_like_zone_name(s) => Err(CronError::UnknownZone {
                name: s.to_string(),
                suggestion: zones::suggest(s),
            }),
            Err(e) => Err(e),
        }
    }

    /// Parses a POSIX TZ rule, without the name lookup that [`Tz::parse`]
    /// tries first.
    pub fn posix(rule: &str) -> Result<Tz> {
        PosixTz::parse(rule).map(Tz::Rule)
    }

    /// A fixed offset in seconds east of UTC; at most 24:59:59 either way.
    pub fn fixed(offset_secs: i32) -> Result<Tz> {
        if offset_secs.unsigned_abs() > MAX_OFFSET as u32 {
            return Err(CronError::InvalidTz {
                input: offset_secs.to_string(),
                column: 1,
                reason: "offset must be within 24:59:59 of UTC",
            });
        }
        Ok(Tz::Fixed(offset_secs))
    }

    /// The IANA name for zones from the table, `"UTC"` for [`Tz::Utc`].
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Tz::Utc => Some("UTC"),
            Tz::Fixed(_) => None,
            Tz::Rule(rule) => rule.name(),
        }
    }

    /// Whether the zone ever changes its offset.
    #[must_use]
    pub fn has_transitions(&self) -> bool {
        matches!(self, Tz::Rule(rule) if rule.has_transitions())
    }

    /// UTC offset in seconds east at the instant `utc`.
    #[must_use]
    pub fn offset_at(&self, utc: i64) -> i32 {
        match self {
            Tz::Utc => 0,
            Tz::Fixed(offset) => *offset,
            Tz::Rule(rule) => rule.offset_at(clamp(utc)),
        }
    }

    /// Whether daylight saving time is in force at `utc`, as the rule
    /// defines it (see [`PosixTz::is_dst_at`]).
    #[must_use]
    pub fn is_dst_at(&self, utc: i64) -> bool {
        match self {
            Tz::Rule(rule) => rule.is_dst_at(clamp(utc)),
            _ => false,
        }
    }

    /// Abbreviation in force at `utc` (`"EST"`, `"+0530"`, `"UTC"`).
    #[must_use]
    pub fn abbreviation_at(&self, utc: i64) -> Cow<'_, str> {
        match self {
            Tz::Utc => Cow::Borrowed("UTC"),
            Tz::Fixed(offset) => Cow::Owned(format_offset(*offset, "")),
            Tz::Rule(rule) => Cow::Borrowed(rule.abbreviation_at(clamp(utc))),
        }
    }

    /// Wall-clock reading at `utc`. Instants beyond years 1-9999 clamp.
    #[must_use]
    pub fn to_local(&self, utc: i64) -> LocalDateTime {
        let utc = clamp(utc);
        LocalDateTime::from_naive(utc + i64::from(self.offset_at(utc)))
    }

    /// Instants with the given wall-clock reading.
    ///
    /// ```
    /// use belay_cron::{LocalDateTime, LocalResult, Tz};
    ///
    /// let ny = Tz::named("America/New_York")?;
    /// let skipped = LocalDateTime::new(2025, 3, 9, 2, 30, 0).unwrap();
    /// // 03:00 EDT, the first instant after the gap, is 07:00 UTC.
    /// assert_eq!(ny.from_local(skipped).earliest(), 1_741_503_600);
    /// let twice = LocalDateTime::new(2025, 11, 2, 1, 30, 0).unwrap();
    /// assert_eq!(
    ///     ny.from_local(twice),
    ///     LocalResult::Ambiguous(1_762_061_400, 1_762_065_000)
    /// );
    /// # Ok::<(), belay_cron::CronError>(())
    /// ```
    #[must_use]
    pub fn from_local(&self, local: LocalDateTime) -> LocalResult {
        self.resolve_naive(local.to_naive())
    }

    pub(crate) fn resolve_naive(&self, naive: i64) -> LocalResult {
        let rule = match self {
            Tz::Utc => return LocalResult::Single(naive),
            Tz::Fixed(offset) => return LocalResult::Single(naive - i64::from(*offset)),
            Tz::Rule(rule) => rule,
        };
        let std = i64::from(rule.std_offset());
        let dst = rule.dst_offset().map_or(std, i64::from);
        let (a, b) = (naive - std, naive - dst);
        let a_ok = i64::from(rule.offset_at(a)) == std;
        let b_ok = i64::from(rule.offset_at(b)) == dst;
        match (a_ok, b_ok) {
            (true, true) if a == b => LocalResult::Single(a),
            (true, true) => LocalResult::Ambiguous(a.min(b), a.max(b)),
            (true, false) => LocalResult::Single(a),
            (false, true) => LocalResult::Single(b),
            (false, false) => {
                // Neither offset reproduces the reading, so the offset changes
                // somewhere in (lo, hi]; a bisection finds the exact instant
                // without assuming anything about how the rule is phrased.
                let (mut lo, mut hi) = (a.min(b), a.max(b));
                let target = rule.offset_at(hi);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if rule.offset_at(mid) == target {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                LocalResult::Gap {
                    before: hi - 1,
                    after: hi,
                }
            }
        }
    }

    /// The first offset change strictly after `utc`.
    #[must_use]
    pub fn next_transition(&self, utc: i64) -> Option<Transition> {
        match self {
            Tz::Rule(rule) => rule
                .next_transition_at(clamp(utc))
                .map(|at| transition(rule, at)),
            _ => None,
        }
    }

    /// The last offset change at or before `utc`.
    #[must_use]
    pub fn prev_transition(&self, utc: i64) -> Option<Transition> {
        match self {
            Tz::Rule(rule) => rule
                .prev_transition_at(clamp(utc))
                .map(|at| transition(rule, at)),
            _ => None,
        }
    }

    /// The smallest and largest offsets the zone ever uses.
    pub(crate) fn offset_bounds(&self) -> (i32, i32) {
        match self {
            Tz::Utc => (0, 0),
            Tz::Fixed(offset) => (*offset, *offset),
            Tz::Rule(rule) => {
                let std = rule.std_offset();
                let dst = rule.dst_offset().unwrap_or(std);
                (std.min(dst), std.max(dst))
            }
        }
    }

    pub(crate) fn segment_at(&self, utc: i64) -> Segment {
        let (start, end) = match self {
            Tz::Rule(rule) => {
                let (prev, next) = rule.neighbours(clamp(utc));
                (
                    prev.map(|at| transition(rule, at)),
                    next.map(|at| transition(rule, at)),
                )
            }
            _ => (None, None),
        };
        Segment {
            offset: self.offset_at(utc),
            start,
            end,
        }
    }

    /// RFC 3339 text for `utc` in this zone, e.g. `2025-03-09T03:00:00-04:00`
    /// (`Z` for UTC itself).
    #[must_use]
    pub fn format_rfc3339(&self, utc: i64) -> String {
        let local = self.to_local(utc);
        if *self == Tz::Utc {
            return format!("{local}Z");
        }
        format!("{local}{}", format_offset(self.offset_at(utc), ":"))
    }
}

fn transition(rule: &PosixTz, at: i64) -> Transition {
    Transition {
        at,
        offset_before: rule.offset_at(at - 1),
        offset_after: rule.offset_at(at),
    }
}
