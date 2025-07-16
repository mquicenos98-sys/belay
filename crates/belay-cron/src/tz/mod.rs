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

/// Keeps rule evaluation inside years 1-9999, where the calendar math is
/// exact and nothing overflows.
fn clamp(utc: i64) -> i64 {
    utc.clamp(MIN_NAIVE + 2 * 86_400, MAX_NAIVE - 2 * 86_400)
}

/// `+05:30`-style text; `sep` is `":"` or `""`. Seconds appear only when
/// non-zero (no real zone has them today).
fn format_offset(offset: i32, sep: &str) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    let (h, m, s) = (abs / 3600, abs / 60 % 60, abs % 60);
    if s == 0 {
        format!("{sign}{h:02}{sep}{m:02}")
    } else {
        format!("{sign}{h:02}{sep}{m:02}{sep}{s:02}")
    }
}

fn parse_fixed(input: &str, s: &str) -> Result<i32> {
    let fail = |reason| CronError::InvalidTz {
        input: input.to_string(),
        column: input.find(s).unwrap_or(0) + 1,
        reason,
    };
    let negative = s.starts_with('-');
    let body = &s[1..];
    let digits: String = body.chars().filter(|c| *c != ':').collect();
    let colon_ok = match body.find(':') {
        None => true,
        Some(2) => body.len() == 5 || (body.len() == 8 && body[5..].starts_with(':')),
        Some(_) => false,
    };
    if !colon_ok || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail("expected an offset like +05:30, -0800 or +01"));
    }
    let (h, m, sec) = match digits.len() {
        2 => (&digits[..2], "0", "0"),
        4 => (&digits[..2], &digits[2..4], "0"),
        6 => (&digits[..2], &digits[2..4], &digits[4..6]),
        _ => return Err(fail("expected an offset like +05:30, -0800 or +01")),
    };
    let parse = |p: &str| p.parse::<i32>().unwrap_or(i32::MAX);
    let (h, m, sec) = (parse(h), parse(m), parse(sec));
    if m > 59 || sec > 59 {
        return Err(fail("offset minutes and seconds must be 0-59"));
    }
    let total = h * 3600 + m * 60 + sec;
    if total > MAX_OFFSET {
        return Err(fail("offset must be within 24:59:59 of UTC"));
    }
    Ok(if negative { -total } else { total })
}

fn looks_like_zone_name(s: &str) -> bool {
    s.contains('/')
        || (!s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'-'))
}

impl fmt::Display for Tz {
    /// Prints the zone name when known, otherwise `+05:30` or the normalised
    /// POSIX rule. The output parses back with [`Tz::parse`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tz::Utc => f.write_str("UTC"),
            Tz::Fixed(offset) => f.write_str(&format_offset(*offset, ":")),
            Tz::Rule(rule) => match rule.name() {
                Some(name) => f.write_str(name),
                None => write!(f, "{rule}"),
            },
        }
    }
}

impl FromStr for Tz {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Tz> {
        Tz::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{days_from_civil, SECS_PER_DAY};

    fn ts(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * SECS_PER_DAY + h * 3600 + mi * 60
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> LocalDateTime {
        LocalDateTime::new(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parse_accepts_names_offsets_and_rules() {
        assert_eq!(Tz::parse("UTC").unwrap(), Tz::Utc);
        assert_eq!(Tz::parse("Etc/UTC").unwrap(), Tz::Utc);
        assert_eq!(Tz::parse("GMT").unwrap(), Tz::Utc);
        assert_eq!(Tz::parse("Z").unwrap(), Tz::Utc);
        assert_eq!(Tz::parse("+05:30").unwrap(), Tz::Fixed(19_800));
        assert_eq!(Tz::parse("-0800").unwrap(), Tz::Fixed(-28_800));
        assert_eq!(Tz::parse("+01").unwrap(), Tz::Fixed(3600));
        assert_eq!(Tz::parse("-03:00:30").unwrap(), Tz::Fixed(-10_830));
        assert_eq!(
            Tz::parse(":Europe/Paris").unwrap().name(),
            Some("Europe/Paris")
        );
        let rule = Tz::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        assert_eq!(rule.name(), None);
        assert!(rule.has_transitions());
        assert_eq!(Tz::parse("UTC+3").unwrap().offset_at(0), -3 * 3600);
    }

    #[test]
    fn parse_rejects_bad_offsets_and_unknown_names() {
        for bad in ["+5:30", "+05:3", "+123", "+25:00", "+05:60", "-", "+05:30:"] {
            assert!(
                matches!(Tz::parse(bad), Err(CronError::InvalidTz { .. })),
                "{bad}"
            );
        }
        assert_eq!(
            Tz::parse("Europe/Berln").unwrap_err(),
            CronError::UnknownZone {
                name: "Europe/Berln".into(),
                suggestion: Some("Europe/Berlin"),
            }
        );
        assert!(matches!(
            Tz::parse("Pacific_Time"),
            Err(CronError::UnknownZone { .. })
        ));
        assert!(matches!(
            Tz::parse("EST5EDT,M3"),
            Err(CronError::InvalidTz { .. })
        ));
    }

    #[test]
    fn display_round_trips_through_parse() {
        for s in [
            "UTC",
            "+05:30",
            "-08:00",
            "America/New_York",
            "Etc/GMT+5",
            "EST5EDT,M3.2.0,M11.1.0",
            "<+0330>-3:30",
        ] {
            let tz = Tz::parse(s).unwrap();
            assert_eq!(tz.to_string(), s);
            assert_eq!(Tz::parse(&tz.to_string()).unwrap(), tz);
        }
    }

    #[test]
    fn fixed_rejects_offsets_beyond_a_day() {
        assert!(Tz::fixed(14 * 3600).is_ok());
        assert!(Tz::fixed(-(MAX_OFFSET)).is_ok());
        assert!(Tz::fixed(MAX_OFFSET + 1).is_err());
    }

    #[test]
    fn from_local_classifies_gap_overlap_and_normal_readings() {
        let ny = Tz::named("America/New_York").unwrap();
        assert_eq!(
            ny.from_local(local(2025, 7, 4, 12, 0)),
            LocalResult::Single(ts(2025, 7, 4, 16, 0))
        );
        let gap = ny.from_local(local(2025, 3, 9, 2, 30));
        let t = ts(2025, 3, 9, 7, 0);
        assert_eq!(
            gap,
            LocalResult::Gap {
                before: t - 1,
                after: t
            }
        );
        assert_eq!(gap.single(), None);
        assert_eq!(gap.latest(), t - 1);
        let overlap = ny.from_local(local(2025, 11, 2, 1, 30));
        assert_eq!(
            overlap,
            LocalResult::Ambiguous(ts(2025, 11, 2, 5, 30), ts(2025, 11, 2, 6, 30))
        );
        assert_eq!(overlap.earliest(), ts(2025, 11, 2, 5, 30));
        assert_eq!(overlap.latest(), ts(2025, 11, 2, 6, 30));
        // The readings bounding the gap and the overlap are ordinary.
        assert_eq!(
            ny.from_local(local(2025, 3, 9, 3, 0)),
            LocalResult::Single(t)
        );
        assert_eq!(
            ny.from_local(LocalDateTime::new(2025, 3, 9, 1, 59, 59).unwrap()),
            LocalResult::Single(t - 1)
        );
        assert_eq!(
            ny.from_local(local(2025, 11, 2, 2, 0)),
            LocalResult::Single(ts(2025, 11, 2, 7, 0))
        );
    }

    #[test]
    fn from_local_handles_negative_dst_and_half_hour_shifts() {
        let dublin = Tz::named("Europe/Dublin").unwrap();
        let t = ts(2025, 3, 30, 1, 0);
        assert_eq!(
            dublin.from_local(local(2025, 3, 30, 1, 30)),
            LocalResult::Gap {
                before: t - 1,
                after: t
            }
        );
        assert_eq!(
            dublin.from_local(local(2025, 10, 26, 1, 30)),
            LocalResult::Ambiguous(ts(2025, 10, 26, 0, 30), ts(2025, 10, 26, 1, 30))
        );
        let lord_howe = Tz::named("Australia/Lord_Howe").unwrap();
        // DST starts 2025-10-05 02:00 +10:30 (15:30 UTC) and skips 30 minutes.
        let t = ts(2025, 10, 4, 15, 30);
        assert_eq!(
            lord_howe.from_local(local(2025, 10, 5, 2, 15)),
            LocalResult::Gap {
                before: t - 1,
                after: t
            }
        );
        assert_eq!(
            lord_howe.from_local(local(2025, 10, 5, 2, 30)),
            LocalResult::Single(t)
        );
    }

    #[test]
    fn transitions_report_offsets_on_both_sides() {
        let london = Tz::named("Europe/London").unwrap();
        let tr = london.next_transition(ts(2025, 1, 1, 0, 0)).unwrap();
        assert_eq!(
            tr,
            Transition {
                at: ts(2025, 3, 30, 1, 0),
                offset_before: 0,
                offset_after: 3600,
            }
        );
        assert!(tr.is_forward());
        let back = london.next_transition(tr.at).unwrap();
        assert_eq!(back.at, ts(2025, 10, 26, 1, 0));
        assert!(!back.is_forward());
        assert_eq!(london.prev_transition(back.at), Some(back));
        assert_eq!(london.prev_transition(back.at - 1), Some(tr));
        assert_eq!(Tz::Utc.next_transition(0), None);
        assert_eq!(Tz::named("Asia/Kolkata").unwrap().next_transition(0), None);
    }

    #[test]
    fn abbreviations_and_rfc3339_follow_the_offset() {
        let ny = Tz::named("America/New_York").unwrap();
        assert_eq!(ny.abbreviation_at(ts(2025, 1, 1, 0, 0)), "EST");
        assert_eq!(ny.abbreviation_at(ts(2025, 7, 1, 0, 0)), "EDT");
        assert_eq!(
            ny.format_rfc3339(ts(2025, 3, 9, 7, 0)),
            "2025-03-09T03:00:00-04:00"
        );
        assert_eq!(
            Tz::Utc.format_rfc3339(ts(2025, 3, 9, 7, 0)),
            "2025-03-09T07:00:00Z"
        );
        let kathmandu = Tz::named("Asia/Kathmandu").unwrap();
        assert_eq!(kathmandu.format_rfc3339(0), "1970-01-01T05:45:00+05:45");
        assert_eq!(Tz::Fixed(-9000).abbreviation_at(0), "-0230");
    }

    #[test]
    fn extreme_instants_clamp_instead_of_overflowing() {
        let ny = Tz::named("America/New_York").unwrap();
        assert_eq!(ny.to_local(i64::MAX).year(), 9999);
        assert_eq!(ny.to_local(i64::MIN).year(), 1);
        let _ = ny.next_transition(i64::MAX);
        let _ = ny.prev_transition(i64::MIN);
        assert_eq!(Tz::Fixed(3600).to_local(i64::MAX).year(), 9999);
    }
}
