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
