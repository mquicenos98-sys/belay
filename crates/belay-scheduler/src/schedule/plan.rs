//! When a schedule fires: its cron expression evaluated in its time zone,
//! clipped to its `[starts_at, ends_at)` bounds and filtered by its
//! calendar.
//!
//! belay-cron works in Unix seconds; everything here speaks
//! [`Timestamp`]s (milliseconds) and converts at the edge. Fire times are
//! always whole seconds.
//!
//! # Calendar vetoes
//!
//! A cron match that the calendar does not allow is *vetoed*: it is not a
//! fire time. The search walks past vetoed matches one by one, counting
//! them, so that the scheduler can report a closed stretch as a single
//! skip. After [`COUNT_LIMIT`] vetoes in a row it stops counting and jumps
//! straight to the next instant the calendar opens, so an every-second
//! expression under a weekday calendar costs no more than an hourly one.

use std::sync::Arc;
use std::time::Duration;

use belay_common::hash::hash64;
use belay_common::Timestamp;
use belay_cron::{CronExpr, Tz};
use belay_proto::limits::{MAX_CRON_LEN, MAX_TIMEZONE_LEN};
use belay_proto::{ScheduleId, ScheduleSpec};

use super::calendar::CompiledCalendar;

/// Counts of skipped or missed fire times stop at this value.
pub(crate) const COUNT_LIMIT: u32 = 10_000;

/// Calendar jumps one search may make before concluding that the cron
/// expression and the calendar never meet again.
const MAX_JUMPS: u32 = 2_000;

/// Seed for jitter offsets; changing it would move every jittered job.
const JITTER_SEED: u64 = 0x6265_6c61_795f_6a74;

/// Parses a cron expression, with messages fit for a `cron` violation.
pub(crate) fn parse_cron(text: &str) -> Result<CronExpr, String> {
    if text.len() > MAX_CRON_LEN {
        return Err(format!(
            "must be at most {MAX_CRON_LEN} bytes, got {}",
            text.len()
        ));
    }
    let expr = CronExpr::parse(text).map_err(|e| e.to_string())?;
    if !expr.is_satisfiable() {
        return Err(format!(
            "{text:?} can never match (no date satisfies its day and month fields)"
        ));
    }
    Ok(expr)
}

/// Parses a time zone (IANA name, fixed offset or POSIX rule), with
/// messages fit for a `timezone` violation.
pub(crate) fn parse_timezone(text: &str) -> Result<Tz, String> {
    if text.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if text.len() > MAX_TIMEZONE_LEN {
        return Err(format!(
            "must be at most {MAX_TIMEZONE_LEN} bytes, got {}",
            text.len()
        ));
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must not contain spaces or control characters".to_owned());
    }
    Tz::parse(text).map_err(|e| e.to_string())
}

/// A run of consecutive times: vetoed matches, missed fires, fires that
/// came due while paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    /// The first time in the run.
    pub first: Timestamp,
    /// The last time in the run.
    pub last: Timestamp,
    /// Times in the run, saturating at [`COUNT_LIMIT`].
    pub count: u32,
}

impl Span {
    pub(crate) fn new(t: Timestamp) -> Span {
        Span {
            first: t,
            last: t,
            count: 1,
        }
    }

    /// Extends the run to `t`, returning the new count.
    pub(crate) fn push(&mut self, t: Timestamp) -> u32 {
        self.last = t;
        self.count = (self.count + 1).min(COUNT_LIMIT);
        self.count
    }

    /// Extends `span` to `t`, starting it if there is none.
    pub(crate) fn extend(span: &mut Option<Span>, t: Timestamp) -> u32 {
        match span {
            Some(s) => s.push(t),
            None => {
                *span = Some(Span::new(t));
                1
            }
        }
    }
}

/// The result of a fire-time search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Next {
    /// The fire time, or `None` if the schedule never fires again.
    pub at: Option<Timestamp>,
    /// Matches the calendar vetoed on the way there. After a jump the
    /// count stops growing, but `last` is still the final vetoed match.
    pub vetoed: Option<Span>,
}

/// A compiled schedule: everything needed to compute its fire times.
#[derive(Debug)]
pub(crate) struct Plan {
    expr: CronExpr,
    tz: Tz,
    calendar: Option<Arc<CompiledCalendar>>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
}

impl Plan {
    /// Compiles `spec`; `calendar` must be the calendar it names, if any.
    pub(crate) fn compile(
        spec: &ScheduleSpec,
        calendar: Option<Arc<CompiledCalendar>>,
    ) -> Result<Plan, String> {
        let expr = parse_cron(&spec.cron).map_err(|e| format!("cron: {e}"))?;
        let tz = parse_timezone(&spec.timezone).map_err(|e| format!("timezone: {e}"))?;
        Ok(Plan::new(expr, tz, calendar, spec.starts_at, spec.ends_at))
    }

    pub(crate) fn new(
        expr: CronExpr,
        tz: Tz,
        calendar: Option<Arc<CompiledCalendar>>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
    ) -> Plan {
        Plan {
            expr,
            tz,
            calendar,
            starts_at,
            ends_at,
        }
    }

    /// The calendar this plan was compiled against.
    pub(crate) fn calendar(&self) -> Option<&Arc<CompiledCalendar>> {
        self.calendar.as_ref()
    }

    /// Whether the calendar (if any) allows a fire at `t`. Fire times from
    /// [`next_after`](Self::next_after) always pass; this re-checks times
    /// computed against an older version of the calendar.
    pub(crate) fn allows(&self, t: Timestamp) -> bool {
        self.calendar.as_ref().map_or(true, |c| c.allows(t))
    }

    /// The first fire time strictly after `after`, counting vetoed matches
    /// on the way.
    pub(crate) fn next_after(&self, after: Timestamp) -> Next {
        self.search(after, COUNT_LIMIT)
    }

    /// The first fire time strictly after `after`, jumping over closed
    /// calendar stretches without counting them.
    pub(crate) fn next_fire(&self, after: Timestamp) -> Option<Timestamp> {
        self.search(after, 1).at
    }

    /// The next `n` fire times strictly after `after`.
    pub(crate) fn upcoming(&self, after: Timestamp, n: usize) -> Vec<Timestamp> {
        let mut out = Vec::with_capacity(n.min(64));
        let mut cursor = after;
        while out.len() < n {
            match self.next_fire(cursor) {
                Some(t) => {
                    out.push(t);
                    cursor = t;
                }
                None => break,
            }
        }
        out
    }

    /// The last fire time at or before `t`, looking back over at most
    /// [`COUNT_LIMIT`] cron matches.
    pub(crate) fn last_fire_until(&self, t: Timestamp) -> Option<Timestamp> {
        let mut cursor = t.as_secs().checked_add(1)?;
        for _ in 0..COUNT_LIMIT {
            let secs = self.expr.prev_before(cursor, &self.tz)?;
            let at = Timestamp::from_secs(secs);
            if self.starts_at.map_or(false, |start| at < start) {
                return None;
            }
            if self.ends_at.map_or(true, |end| at < end) && self.allows(at) {
                return Some(at);
            }
            cursor = secs;
        }
        None
    }

    /// Whether the cron expression alone (ignoring the calendar) matches
    /// again after `after` within the bounds.
    pub(crate) fn cron_matches_after(&self, after: Timestamp) -> bool {
        let after = self.lower_bound(after);
        match self.expr.next_after(after.as_secs(), &self.tz) {
            Some(secs) => self
                .ends_at
                .map_or(true, |end| Timestamp::from_secs(secs) < end),
            None => false,
        }
    }

    /// Fire times are at or after `starts_at`, so a search from before it
    /// starts just before it.
    fn lower_bound(&self, after: Timestamp) -> Timestamp {
        match self.starts_at {
            Some(start) if after < start => start - Duration::from_millis(1),
            _ => after,
        }
    }

    fn search(&self, after: Timestamp, veto_budget: u32) -> Next {
        // Fire times are whole seconds; a match `k` is after `after` exactly
        // when `k > floor(after / 1s)`, which is what next_after computes.
        let mut cursor = self.lower_bound(after).as_secs();
        let mut vetoed: Option<Span> = None;
        let mut jumps = 0;
        loop {
            let Some(secs) = self.expr.next_after(cursor, &self.tz) else {
                return Next { at: None, vetoed };
            };
            let t = Timestamp::from_secs(secs);
            if self.ends_at.map_or(false, |end| t >= end) {
                return Next { at: None, vetoed };
            }
            let Some(calendar) = &self.calendar else {
                return Next {
                    at: Some(t),
                    vetoed: None,
                };
            };
            if calendar.allows(t) {
                if let Some(v) = &mut vetoed {
                    // After a jump, matches between the last one examined and
                    // `t` were vetoed without being looked at; the one just
                    // before `t` is the true end of the run.
                    if let Some(prev) = self.expr.prev_before(secs, &self.tz) {
                        v.last = v.last.max(Timestamp::from_secs(prev));
                    }
                }
                return Next {
                    at: Some(t),
                    vetoed,
                };
            }
            let count = Span::extend(&mut vetoed, t);
            if count < veto_budget {
                cursor = secs;
                continue;
            }
            jumps += 1;
            if jumps > MAX_JUMPS {
                return Next { at: None, vetoed };
            }
            match calendar.next_open(t) {
                Some(open) if open > t => cursor = open.as_secs() - 1,
                _ => return Next { at: None, vetoed },
            }
        }
    }
}

/// The deterministic delay added to the job of schedule `id`'s fire at
/// `fire_time`: uniform-ish in `[0, jitter)` at millisecond resolution, and
/// the same every time it is computed, so a retried fire runs at the same
/// moment as the original would have.
///
/// ```
/// # use std::time::Duration;
/// # use belay_common::Timestamp;
/// # use belay_proto::ScheduleId;
/// use belay_scheduler::jitter_offset;
///
/// let id = ScheduleId::from_static("nightly");
/// let fire = Timestamp::from_secs(1_750_000_000);
/// let offset = jitter_offset(&id, fire, Duration::from_secs(30));
/// assert!(offset < Duration::from_secs(30));
/// assert_eq!(offset, jitter_offset(&id, fire, Duration::from_secs(30)));
/// assert_eq!(jitter_offset(&id, fire, Duration::ZERO), Duration::ZERO);
/// ```
#[must_use]
pub fn jitter_offset(id: &ScheduleId, fire_time: Timestamp, jitter: Duration) -> Duration {
    let bound = jitter.as_secs();
    if bound == 0 {
        return Duration::ZERO;
    }
    let mut bytes = Vec::with_capacity(id.as_str().len() + 9);
    bytes.extend_from_slice(id.as_str().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&fire_time.as_millis().to_le_bytes());
    Duration::from_millis(hash64(&bytes, JITTER_SEED) % bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn plan(cron: &str, tz: &str) -> Plan {
        Plan::new(
            parse_cron(cron).unwrap(),
            parse_timezone(tz).unwrap(),
            None,
            None,
            None,
        )
    }

    fn weekday_office() -> Arc<CompiledCalendar> {
        let spec = belay_json::from_str(
            r#"{"name": "office", "timezone": "UTC",
                "windows": [{"days": "mon-fri", "start": "09:00", "end": "17:00"}]}"#,
        )
        .unwrap();
        Arc::new(CompiledCalendar::compile(spec).unwrap())
    }

    #[test]
    fn fire_times_are_strictly_after_the_given_instant() {
        let p = plan("0 * * * *", "UTC");
        let on_the_hour = ts("2025-06-02T10:00:00Z");
        assert_eq!(
            p.next_after(on_the_hour).at,
            Some(ts("2025-06-02T11:00:00Z"))
        );
        assert_eq!(
            p.next_after(ts("2025-06-02T09:59:59.999Z")).at,
            Some(on_the_hour)
        );
        assert_eq!(
            p.upcoming(on_the_hour, 3),
            [
                ts("2025-06-02T11:00:00Z"),
                ts("2025-06-02T12:00:00Z"),
                ts("2025-06-02T13:00:00Z")
            ]
        );
    }

    #[test]
    fn bounds_clip_fire_times() {
        let mut p = plan("*/10 * * * *", "UTC");
        p.starts_at = Some(ts("2025-06-02T10:00:00Z"));
        p.ends_at = Some(ts("2025-06-02T10:30:00Z"));
        // starts_at itself is a fire time.
        assert_eq!(
            p.upcoming(ts("2025-01-01T00:00:00Z"), 5),
            [
                ts("2025-06-02T10:00:00Z"),
                ts("2025-06-02T10:10:00Z"),
                ts("2025-06-02T10:20:00Z")
            ]
        );
        assert!(p.cron_matches_after(ts("2025-06-02T10:15:00Z")));
        assert!(!p.cron_matches_after(ts("2025-06-02T10:20:00Z")));
    }

    #[test]
    fn vetoed_runs_are_counted_and_bounded() {
        let mut p = plan("0 * * * *", "UTC");
        p.calendar = Some(weekday_office());
        // Friday 2025-06-06 16:00 fires; then 17:00 Friday through 08:00
        // Monday are vetoed: 7 + 24 + 24 + 9 = 64 hourly matches.
        let next = p.next_after(ts("2025-06-06T16:00:00Z"));
        assert_eq!(next.at, Some(ts("2025-06-09T09:00:00Z")));
        assert_eq!(
            next.vetoed,
            Some(Span {
                first: ts("2025-06-06T17:00:00Z"),
                last: ts("2025-06-09T08:00:00Z"),
                count: 64
            })
        );
        // The fast search agrees on the time.
        assert_eq!(
            p.next_fire(ts("2025-06-06T16:00:00Z")),
            Some(ts("2025-06-09T09:00:00Z"))
        );
        // Inside the window there is nothing to veto.
        assert_eq!(p.next_after(ts("2025-06-09T09:00:00Z")).vetoed, None);
    }

    #[test]
    fn long_closed_stretches_saturate_the_count_but_keep_exact_edges() {
        let mut p = plan("* * * * * *", "UTC");
        p.calendar = Some(weekday_office());
        let next = p.next_after(ts("2025-06-06T16:59:59Z"));
        assert_eq!(next.at, Some(ts("2025-06-09T09:00:00Z")));
        let veto = next.vetoed.unwrap();
        assert_eq!(veto.count, COUNT_LIMIT);
        assert_eq!(veto.first, ts("2025-06-06T17:00:00Z"));
        assert_eq!(veto.last, ts("2025-06-09T08:59:59Z"));
    }

    #[test]
    fn a_cron_that_never_meets_its_calendar_never_fires() {
        let mut p = plan("0 3 * * *", "UTC");
        p.calendar = Some(weekday_office());
        let next = p.next_after(ts("2025-06-02T00:00:00Z"));
        assert_eq!(next.at, None);
        assert!(p.cron_matches_after(ts("2025-06-02T00:00:00Z")));
    }

    #[test]
    fn parse_errors_read_well() {
        assert_eq!(
            parse_cron("61 * * * *").unwrap_err(),
            "invalid value 61 in minute field (0-59) at column 1"
        );
        assert_eq!(
            parse_cron("0 0 30 2 *").unwrap_err(),
            r#""0 0 30 2 *" can never match (no date satisfies its day and month fields)"#
        );
        assert_eq!(
            parse_timezone("Europe/Berln").unwrap_err(),
            r#"unknown time zone "Europe/Berln" (did you mean "Europe/Berlin"?)"#
        );
        assert_eq!(
            parse_timezone("Europe/ Berlin").unwrap_err(),
            "must not contain spaces or control characters"
        );
        assert!(parse_timezone("+05:30").is_ok());
        assert!(parse_timezone("EST5EDT,M3.2.0,M11.1.0").is_ok());
    }
}
