//! Daylight saving time scenarios with hand-checked expectations.
//!
//! Instants are written as UTC wall-clock strings and results as RFC 3339
//! text in the zone, so each assertion reads like the timetable a user would
//! expect to see.

use belay_cron::{CronExpr, LocalDateTime, Tz};

fn utc(s: &str) -> i64 {
    let local = LocalDateTime::parse(s).unwrap();
    Tz::Utc.from_local(local).single().unwrap()
}

fn zone(name: &str) -> Tz {
    Tz::parse(name).unwrap()
}

/// The next `n` fires after the UTC instant `from`, formatted in the zone.
fn next_fires(expr: &str, tz: &str, from: &str, n: usize) -> Vec<String> {
    let tz = zone(tz);
    CronExpr::parse(expr)
        .unwrap()
        .upcoming(utc(from), &tz)
        .take(n)
        .map(|t| tz.format_rfc3339(t))
        .collect()
}

/// The previous `n` fires before the UTC instant `from`, newest first.
fn prev_fires(expr: &str, tz: &str, from: &str, n: usize) -> Vec<String> {
    let tz = zone(tz);
    CronExpr::parse(expr)
        .unwrap()
        .preceding(utc(from), &tz)
        .take(n)
        .map(|t| tz.format_rfc3339(t))
        .collect()
}

const NY: &str = "America/New_York";

#[test]
fn dst_gap_fires_at_first_valid_instant() {
    assert_eq!(
        next_fires("30 2 * * *", NY, "2025-03-08T12:00:00", 2),
        ["2025-03-09T03:00:00-04:00", "2025-03-10T02:30:00-04:00"]
    );
}

#[test]
fn several_skipped_matches_collapse_into_one_fire() {
    assert_eq!(
        next_fires("*/15 2 * * *", NY, "2025-03-08T12:00:00", 3),
        [
            "2025-03-09T03:00:00-04:00",
            "2025-03-10T02:00:00-04:00",
            "2025-03-10T02:15:00-04:00"
        ]
    );
}

#[test]
fn gap_fire_and_a_regular_match_at_the_same_instant_fire_once() {
    assert_eq!(
        next_fires("0 2,3 * * *", NY, "2025-03-08T12:00:00", 3),
        [
            "2025-03-09T03:00:00-04:00",
            "2025-03-10T02:00:00-04:00",
            "2025-03-10T03:00:00-04:00"
        ]
    );
}

#[test]
fn fixed_time_in_repeated_hour_fires_once() {
    assert_eq!(
        next_fires("30 1 * * *", NY, "2025-11-01T12:00:00", 2),
        ["2025-11-02T01:30:00-04:00", "2025-11-03T01:30:00-05:00"]
    );
    // Starting inside the repeated hour, after the first 01:30 has passed,
    // the second 01:30 is not a new fire.
    assert_eq!(
        next_fires("30 1 * * *", NY, "2025-11-02T05:45:00", 1),
        ["2025-11-03T01:30:00-05:00"]
    );
}

#[test]
fn periodic_schedule_fires_in_both_passes() {
    assert_eq!(
        next_fires("*/30 * * * *", NY, "2025-11-02T04:45:00", 5),
        [
            "2025-11-02T01:00:00-04:00",
            "2025-11-02T01:30:00-04:00",
            "2025-11-02T01:00:00-05:00",
            "2025-11-02T01:30:00-05:00",
            "2025-11-02T02:00:00-05:00"
        ]
    );
}

#[test]
fn hourly_schedule_fires_every_real_hour_when_clocks_go_back() {
    assert_eq!(
        next_fires("0 * * * *", NY, "2025-11-02T04:30:00", 3),
        [
            "2025-11-02T01:00:00-04:00",
            "2025-11-02T01:00:00-05:00",
            "2025-11-02T02:00:00-05:00"
        ]
    );
}

#[test]
fn hourly_schedule_loses_the_skipped_hour_when_clocks_go_forward() {
    // 02:00 does not exist; its fire moves to 03:00, which is also a match.
    assert_eq!(
        next_fires("0 * * * *", NY, "2025-03-09T05:30:00", 3),
        [
            "2025-03-09T01:00:00-05:00",
            "2025-03-09T03:00:00-04:00",
            "2025-03-09T04:00:00-04:00"
        ]
    );
    // At half past, the skipped 02:30 fires at the transition instead.
    assert_eq!(
        next_fires("30 * * * *", NY, "2025-03-09T05:45:00", 3),
        [
            "2025-03-09T01:30:00-05:00",
            "2025-03-09T03:00:00-04:00",
            "2025-03-09T03:30:00-04:00"
        ]
    );
}

#[test]
fn london_transitions_at_one_utc() {
    assert_eq!(
        next_fires("30 1 * * *", "Europe/London", "2025-03-29T12:00:00", 2),
        ["2025-03-30T02:00:00+01:00", "2025-03-31T01:30:00+01:00"]
    );
    assert_eq!(
        next_fires("30 1 * * *", "Europe/London", "2025-10-25T12:00:00", 2),
        ["2025-10-26T01:30:00+01:00", "2025-10-27T01:30:00+00:00"]
    );
}

#[test]
fn southern_hemisphere_springs_forward_in_october() {
    let sydney = "Australia/Sydney";
    assert_eq!(
        next_fires("30 2 * * *", sydney, "2025-10-04T00:00:00", 2),
        ["2025-10-05T03:00:00+11:00", "2025-10-06T02:30:00+11:00"]
    );
    assert_eq!(
        next_fires("30 2 * * *", sydney, "2025-04-05T00:00:00", 2),
        ["2025-04-06T02:30:00+11:00", "2025-04-07T02:30:00+10:00"]
    );
    // A southern summer spans New Year: January is on daylight time.
    assert_eq!(
        next_fires("0 0 1 1 *", sydney, "2025-06-01T00:00:00", 1),
        ["2026-01-01T00:00:00+11:00"]
    );
}

#[test]
fn half_hour_offset_without_dst() {
    assert_eq!(
        next_fires("0 9 * * *", "Asia/Kolkata", "2025-03-08T12:00:00", 2),
        ["2025-03-09T09:00:00+05:30", "2025-03-10T09:00:00+05:30"]
    );
    assert_eq!(
        next_fires("0 9 * * *", "Asia/Kolkata", "2025-11-01T12:00:00", 2),
        ["2025-11-02T09:00:00+05:30", "2025-11-03T09:00:00+05:30"]
    );
}

#[test]
fn zone_without_dst_ignores_neighbouring_transitions() {
    assert_eq!(
        next_fires("30 1 * * *", "America/Bogota", "2025-11-01T12:00:00", 2),
        ["2025-11-02T01:30:00-05:00", "2025-11-03T01:30:00-05:00"]
    );
    assert_eq!(
        next_fires("30 2 * * *", "America/Bogota", "2025-03-08T12:00:00", 1),
        ["2025-03-09T02:30:00-05:00"]
    );
}
