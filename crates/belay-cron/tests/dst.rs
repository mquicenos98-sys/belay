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
fn seconds_level_rules_follow_the_same_periodicity_test() {
    // One fixed second: first pass only.
    assert_eq!(
        next_fires("0 0 1 * * *", NY, "2025-11-02T04:30:00", 2),
        ["2025-11-02T01:00:00-04:00", "2025-11-03T01:00:00-05:00"]
    );
    // Several seconds per minute: periodic, both passes.
    assert_eq!(
        next_fires("*/30 0 1 * * *", NY, "2025-11-02T04:30:00", 4),
        [
            "2025-11-02T01:00:00-04:00",
            "2025-11-02T01:00:30-04:00",
            "2025-11-02T01:00:00-05:00",
            "2025-11-02T01:00:30-05:00"
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

#[test]
fn thirty_minute_dst_shift_on_lord_howe() {
    assert_eq!(
        next_fires(
            "15 2 * * *",
            "Australia/Lord_Howe",
            "2025-10-04T00:00:00",
            2
        ),
        ["2025-10-05T02:30:00+11:00", "2025-10-06T02:15:00+11:00"]
    );
    // Clocks go back from 02:00 to 01:30 on 2025-04-06.
    assert_eq!(
        next_fires(
            "45 1 * * *",
            "Australia/Lord_Howe",
            "2025-04-05T00:00:00",
            2
        ),
        ["2025-04-06T01:45:00+11:00", "2025-04-07T01:45:00+10:30"]
    );
}

#[test]
fn midnight_transitions_in_santiago() {
    let scl = "America/Santiago";
    // 2025-09-07: midnight does not exist, clocks jump to 01:00.
    assert_eq!(
        next_fires("0 0 * * *", scl, "2025-09-06T12:00:00", 2),
        ["2025-09-07T01:00:00-03:00", "2025-09-08T00:00:00-03:00"]
    );
    // 2025-04-06: at midnight clocks go back to 23:00 on the 5th.
    assert_eq!(
        next_fires("30 23 * * *", scl, "2025-04-05T12:00:00", 2),
        ["2025-04-05T23:30:00-03:00", "2025-04-06T23:30:00-04:00"]
    );
}

#[test]
fn negative_dst_in_dublin_behaves_like_london() {
    assert_eq!(
        next_fires("30 1 * * *", "Europe/Dublin", "2025-10-25T12:00:00", 2),
        ["2025-10-26T01:30:00+01:00", "2025-10-27T01:30:00+00:00"]
    );
    assert_eq!(
        next_fires("30 1 * * *", "Europe/Dublin", "2025-03-29T12:00:00", 1),
        ["2025-03-30T02:00:00+01:00"]
    );
}

#[test]
fn two_hour_shift_skips_and_repeats_two_hours() {
    // A made-up zone with double summer time: 01:00-02:59 is skipped in
    // March and repeated in October.
    let tz = "AAA0BBB-2,M3.5.0/1,M10.5.0/3";
    assert_eq!(
        next_fires("30 1,2 * * *", tz, "2025-03-29T12:00:00", 3),
        [
            "2025-03-30T03:00:00+02:00",
            "2025-03-31T01:30:00+02:00",
            "2025-03-31T02:30:00+02:00"
        ]
    );
    assert_eq!(
        next_fires("30 1,2 * * *", tz, "2025-10-25T12:00:00", 3),
        [
            "2025-10-26T01:30:00+02:00",
            "2025-10-26T02:30:00+02:00",
            "2025-10-27T01:30:00+00:00"
        ]
    );
}

#[test]
fn prev_before_mirrors_gap_and_overlap_rules() {
    assert_eq!(
        prev_fires("30 2 * * *", NY, "2025-03-09T12:00:00", 2),
        ["2025-03-09T03:00:00-04:00", "2025-03-08T02:30:00-05:00"]
    );
    assert_eq!(
        prev_fires("30 1 * * *", NY, "2025-11-02T12:00:00", 2),
        ["2025-11-02T01:30:00-04:00", "2025-11-01T01:30:00-04:00"]
    );
    assert_eq!(
        prev_fires("*/30 * * * *", NY, "2025-11-02T07:15:00", 6),
        [
            "2025-11-02T02:00:00-05:00",
            "2025-11-02T01:30:00-05:00",
            "2025-11-02T01:00:00-05:00",
            "2025-11-02T01:30:00-04:00",
            "2025-11-02T01:00:00-04:00",
            "2025-11-02T00:30:00-04:00"
        ]
    );
}

#[test]
fn matches_reports_the_gap_fire_instant() {
    let tz = zone(NY);
    let expr = CronExpr::parse("30 2 * * *").unwrap();
    assert!(expr.matches(utc("2025-03-09T07:00:00"), &tz));
    assert!(!expr.matches(utc("2025-03-09T07:30:00"), &tz));
    assert!(expr.matches(utc("2025-03-10T06:30:00"), &tz));
}

#[test]
fn weekly_schedule_keeps_local_time_across_the_change() {
    let tz = zone(NY);
    let expr = CronExpr::parse("0 9 * * MON").unwrap();
    let fires: Vec<i64> = expr
        .upcoming(utc("2025-02-18T00:00:00"), &tz)
        .take(3)
        .collect();
    // 09:00 EST, then 09:00 EDT: one week less one hour apart.
    assert_eq!(fires[1] - fires[0], 7 * 86_400);
    assert_eq!(fires[2] - fires[1], 7 * 86_400 - 3600);
    assert_eq!(tz.format_rfc3339(fires[2]), "2025-03-10T09:00:00-04:00");
}

#[test]
fn a_year_of_fires_is_strictly_increasing_in_every_test_zone() {
    for name in [
        NY,
        "Europe/London",
        "Australia/Sydney",
        "Asia/Kolkata",
        "America/Bogota",
        "Pacific/Chatham",
    ] {
        let tz = zone(name);
        for text in ["*/20 * * * *", "30 1,2,3 * * *", "0 * * * *", "59 23 L * *"] {
            let expr = CronExpr::parse(text).unwrap();
            let start = utc("2025-01-01T00:00:00");
            let end = utc("2026-01-01T00:00:00");
            let fires: Vec<i64> = expr.upcoming(start, &tz).take_while(|&t| t < end).collect();
            assert!(fires.windows(2).all(|w| w[0] < w[1]), "{name} {text}");
            assert!(!fires.is_empty(), "{name} {text}");
            let mut back: Vec<i64> = expr
                .preceding(end, &tz)
                .take_while(|&t| t > start)
                .collect();
            back.reverse();
            assert_eq!(back, fires, "{name} {text}");
        }
    }
}

#[test]
fn second_pass_is_found_when_the_next_other_match_is_a_year_away() {
    // Periodic (several minutes) but restricted to one day a year: from
    // 01:45 EDT the next reading at or past 01:45 is a year away, yet the
    // repeated 01:00 and 01:30 EST come first. Skipping ahead to the far
    // match must not jump over them.
    assert_eq!(
        next_fires("*/30 1 2 11 *", NY, "2025-11-02T05:45:00", 3),
        [
            "2025-11-02T01:00:00-05:00",
            "2025-11-02T01:30:00-05:00",
            "2026-11-02T01:00:00-05:00"
        ]
    );
    // Backwards from 01:10 EST the latest earlier reading is a year back,
    // but the first pass of 01:30-01:55 EDT happened minutes ago.
    assert_eq!(
        prev_fires("30-59/5 1 2 11 *", NY, "2025-11-02T06:10:00", 2),
        ["2025-11-02T01:55:00-04:00", "2025-11-02T01:50:00-04:00"]
    );
}

#[test]
fn distant_matches_are_found_across_many_transitions() {
    assert_eq!(
        next_fires("0 0 0 1 1 * 2190", NY, "2025-01-01T00:00:00", 2),
        ["2190-01-01T00:00:00-05:00"]
    );
    assert_eq!(
        prev_fires("0 30 2 * 3 SUN 2030", NY, "2199-01-01T00:00:00", 2),
        ["2030-03-31T02:30:00-04:00", "2030-03-24T02:30:00-04:00"]
    );
    // 2030-03-10 02:30 was skipped; its fire is the transition instant.
    assert_eq!(
        next_fires("0 30 2 * 3 SUN 2030", NY, "1990-01-01T00:00:00", 2),
        ["2030-03-03T02:30:00-05:00", "2030-03-10T03:00:00-04:00"]
    );
}
