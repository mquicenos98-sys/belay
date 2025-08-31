//! Calendars bound to their time zone.
//!
//! [`CalendarSpec::allows`] works on a local date and time of day; this
//! module turns instants into those readings in the calendar's own zone
//! (which may differ from the schedule's) and answers the question the
//! fire-time search needs: when does the calendar next allow anything?

use std::fmt;

use belay_common::time::{civil_from_days, days_from_civil};
use belay_common::Timestamp;
use belay_cron::{LocalDateTime, Tz};
use belay_proto::schedule::{CalendarDate, TimeOfDay, TimeWindow};
use belay_proto::CalendarSpec;

/// How far ahead [`CompiledCalendar::next_open`] looks. A calendar closed
/// for longer than this (every date excluded for three years, say) is
/// treated as closed for good.
const HORIZON_DAYS: i64 = 3 * 366;

/// A reading can land in a DST gap and resolve to an instant the calendar
/// does not allow; the search then resumes after it. A handful of retries
/// covers every real zone.
const MAX_RESOLVE_ATTEMPTS: usize = 8;

const MINUTES_PER_DAY: u16 = 24 * 60;

/// A [`CalendarSpec`] with its time zone parsed.
#[derive(Clone)]
pub(crate) struct CompiledCalendar {
    spec: CalendarSpec,
    tz: Tz,
}

impl CompiledCalendar {
    /// Parses the calendar's zone; the message is suitable for a
    /// `timezone` violation.
    pub(crate) fn compile(spec: CalendarSpec) -> Result<CompiledCalendar, String> {
        let tz = crate::schedule::plan::parse_timezone(&spec.timezone)?;
        Ok(CompiledCalendar { spec, tz })
    }

    pub(crate) fn spec(&self) -> &CalendarSpec {
        &self.spec
    }

    /// Whether the calendar allows a fire at `t`.
    pub(crate) fn allows(&self, t: Timestamp) -> bool {
        let local = self.tz.to_local(t.as_secs());
        match reading(&local) {
            Some((date, time)) => self.spec.allows(date, time),
            // Outside the years a calendar date can express.
            None => false,
        }
    }

    /// The first instant at or after `t` (to the second) that the calendar
    /// allows, or `None` if there is none within about three years.
    pub(crate) fn next_open(&self, t: Timestamp) -> Option<Timestamp> {
        let mut from = t.as_secs();
        for _ in 0..MAX_RESOLVE_ATTEMPTS {
            let candidate = self.first_open_reading(from)?;
            if self.allows(Timestamp::from_secs(candidate)) {
                return Some(Timestamp::from_secs(candidate));
            }
            // The reading resolved into a stretch the calendar rejects
            // (a DST gap); look again from the next minute.
            from = candidate - candidate.rem_euclid(60) + 60;
        }
        None
    }

    /// The first instant at or after `from` whose local reading starts an
    /// allowed minute, judged on local readings alone.
    fn first_open_reading(&self, from: i64) -> Option<i64> {
        let local = self.tz.to_local(from);
        let first_day = days_from_civil(local.year(), local.month() as u8, local.day() as u8);
        let now_minute = (local.hour() * 60 + local.minute()) as u16;
        for offset in 0..HORIZON_DAYS {
            let days = first_day + offset;
            let (year, month, day) = civil_from_days(days);
            let Ok(date) = CalendarDate::new(year, month, day) else {
                return None;
            };
            if self.excludes(date) {
                continue;
            }
            let earliest = if offset == 0 { now_minute } else { 0 };
            let mut openings = self.openings(date);
            if offset == 0 {
                openings.push(now_minute);
            }
            openings.sort_unstable();
            openings.dedup();
            for minute in openings {
                if minute < earliest || minute >= MINUTES_PER_DAY {
                    continue;
                }
                let Ok(time) = TimeOfDay::new((minute / 60) as u8, (minute % 60) as u8) else {
                    continue;
                };
                if !self.spec.allows(date, time) {
                    continue;
                }
                if offset == 0 && minute == now_minute {
                    return Some(from);
                }
                let reading = LocalDateTime::new(
                    year,
                    u32::from(month),
                    u32::from(day),
                    u32::from(minute / 60),
                    u32::from(minute % 60),
                    0,
                )?;
                let resolved = self.tz.from_local(reading);
                // In a repeated hour the earlier pass may already be behind
                // us; the later one is then the next occurrence.
                let instant = if resolved.earliest() >= from {
                    resolved.earliest()
                } else {
                    resolved.latest()
                };
                if instant >= from {
                    return Some(instant);
                }
            }
        }
        None
    }

    fn excludes(&self, date: CalendarDate) -> bool {
        self.spec.exclude_dates.contains(&date)
            || self.spec.exclude_weekdays.contains(date.weekday())
    }

    /// Minutes of `date` at which an allowed stretch can begin: the start
    /// of each window opening that day, and midnight when a window from the
    /// day before runs past it (or when there are no windows at all).
    fn openings(&self, date: CalendarDate) -> Vec<u16> {
        if self.spec.windows.is_empty() {
            return vec![0];
        }
        let today = date.weekday();
        let yesterday = date.pred().weekday();
        let mut out = Vec::with_capacity(self.spec.windows.len() + 1);
        for w in &self.spec.windows {
            if starts_on(w, today) {
                out.push(w.start.minutes());
            }
            if w.start >= w.end && starts_on(w, yesterday) {
                out.push(0);
            }
        }
        out
    }
}

/// Mirrors the rule in [`TimeWindow::contains`]: an empty day set means
/// every day.
fn starts_on(w: &TimeWindow, day: belay_common::time::Weekday) -> bool {
    w.days.is_empty() || w.days.contains(day)
}

/// The calendar's view of a local reading: its date and its minute.
fn reading(local: &LocalDateTime) -> Option<(CalendarDate, TimeOfDay)> {
    let date = CalendarDate::new(local.year(), local.month() as u8, local.day() as u8).ok()?;
    let time = TimeOfDay::new(local.hour() as u8, local.minute() as u8).ok()?;
    Some((date, time))
}

impl fmt::Debug for CompiledCalendar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledCalendar")
            .field("name", &self.spec.name)
            .field("tz", &self.tz.to_string())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn calendar(json: &str) -> CompiledCalendar {
        CompiledCalendar::compile(belay_json::from_str(json).unwrap()).unwrap()
    }

    /// Weekdays 09:00-17:00 in Bogota (UTC-5 all year), closed on
    /// Christmas.
    fn office() -> CompiledCalendar {
        calendar(
            r#"{"name": "office", "timezone": "America/Bogota",
                "exclude_dates": ["2025-12-25"],
                "windows": [{"days": ["mon-fri"], "start": "09:00", "end": "17:00"}]}"#,
        )
    }

    #[test]
    fn readings_are_taken_in_the_calendars_zone() {
        let cal = office();
        // 14:00Z is 09:00 in Bogota: open. 13:59Z is 08:59: closed.
        assert!(cal.allows(ts("2025-12-23T14:00:00Z")));
        assert!(!cal.allows(ts("2025-12-23T13:59:59Z")));
        // 21:59Z is 16:59 local, 22:00Z is 17:00 local (end is exclusive).
        assert!(cal.allows(ts("2025-12-23T21:59:59Z")));
        assert!(!cal.allows(ts("2025-12-23T22:00:00Z")));
        // Christmas is closed all day.
        assert!(!cal.allows(ts("2025-12-25T15:00:00Z")));
    }

    #[test]
    fn next_open_is_the_instant_itself_when_open() {
        let cal = office();
        let t = ts("2025-12-23T15:30:42Z");
        assert_eq!(cal.next_open(t), Some(t));
    }

    #[test]
    fn next_open_skips_evenings_weekends_and_holidays() {
        let cal = office();
        // Tuesday 18:00 local: Wednesday 09:00 local.
        assert_eq!(
            cal.next_open(ts("2025-12-23T23:00:00Z")),
            Some(ts("2025-12-24T14:00:00Z"))
        );
        // Wednesday evening: Thursday is Christmas, so Friday 09:00.
        assert_eq!(
            cal.next_open(ts("2025-12-24T22:30:00Z")),
            Some(ts("2025-12-26T14:00:00Z"))
        );
        // Saturday: Monday.
        assert_eq!(
            cal.next_open(ts("2025-12-27T15:00:00Z")),
            Some(ts("2025-12-29T14:00:00Z"))
        );
    }

    #[test]
    fn windows_that_wrap_midnight_open_at_midnight_the_next_day() {
        let cal = calendar(
            r#"{"name": "night", "windows": [{"days": "fri", "start": "22:00", "end": "06:00"}]}"#,
        );
        // Saturday 2025-03-08 01:00 UTC is inside Friday's window.
        assert!(cal.allows(ts("2025-03-08T01:00:00Z")));
        // From Saturday 07:00, the next opening is Friday 22:00.
        assert_eq!(
            cal.next_open(ts("2025-03-08T07:00:00Z")),
            Some(ts("2025-03-14T22:00:00Z"))
        );
        // Friday 21:00: an hour later.
        assert_eq!(
            cal.next_open(ts("2025-03-14T21:00:00Z")),
            Some(ts("2025-03-14T22:00:00Z"))
        );
    }

    #[test]
    fn a_window_inside_a_dst_gap_moves_to_the_next_day() {
        // 02:00-02:30 in New York does not exist on 2025-03-09.
        let cal = calendar(
            r#"{"name": "gap", "timezone": "America/New_York",
                "windows": [{"start": "02:00", "end": "02:30"}]}"#,
        );
        let saturday_evening = ts("2025-03-09T00:00:00Z");
        // Monday 02:00 EDT is 06:00Z; Sunday has no such minutes. The
        // Sunday reading resolves to 03:00 EDT, which the window rejects.
        assert_eq!(
            cal.next_open(saturday_evening),
            Some(ts("2025-03-10T06:00:00Z"))
        );
    }

    #[test]
    fn a_calendar_that_never_opens_gives_up() {
        let mut spec = CalendarSpec::new("closed");
        spec.exclude_weekdays = "mon-sat".parse().unwrap();
        spec.windows.push(TimeWindow::new(
            "mon".parse().unwrap(),
            "09:00".parse().unwrap(),
            "10:00".parse().unwrap(),
        ));
        let cal = CompiledCalendar::compile(spec).unwrap();
        assert_eq!(cal.next_open(ts("2025-01-01T00:00:00Z")), None);
    }

    #[test]
    fn unknown_zones_are_reported() {
        let mut spec = CalendarSpec::new("x");
        spec.timezone = "America/Bogata".into();
        let err = CompiledCalendar::compile(spec).unwrap_err();
        assert_eq!(
            err,
            r#"unknown time zone "America/Bogata" (did you mean "America/Bogota"?)"#
        );
    }
}
