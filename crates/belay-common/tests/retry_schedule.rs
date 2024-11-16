//! A job's retry schedule computed the way the queue engine does it: a
//! backoff policy from configuration text, times from a manual clock, ids
//! from a clock-driven generator, and a table for the CLI.

use std::time::Duration;

use belay_common::duration::{format_duration, humanize};
use belay_common::id::SyncUlidGenerator;
use belay_common::rand::Rng;
use belay_common::text::{Align, Table};
use belay_common::{parse_duration, BackoffPolicy, Clock, ManualClock, Timestamp};

#[test]
fn retry_times_follow_the_configured_policy() {
    let policy: BackoffPolicy = "exp:30s*2..10m".parse().expect("valid policy");
    let clock = ManualClock::new("2025-01-31T23:58:00Z".parse().expect("valid timestamp"));
    let ids = SyncUlidGenerator::with_rng(clock.shared(), Rng::seed_from_u64(1));
    let mut rng = Rng::seed_from_u64(2);

    let mut table = Table::new(["ATTEMPT", "ID", "RUN AT", "DELAY"]).align(0, Align::Right);
    let mut run_at = Vec::new();
    for attempt in 1..=6u32 {
        let delay = policy.delay_for(attempt, &mut rng);
        let at = clock.now() + delay;
        let id = ids.generate().expect("id space");
        table.push_row([
            attempt.to_string(),
            id.to_string(),
            at.to_string(),
            format_duration(delay),
        ]);
        run_at.push(at.to_rfc3339());
        clock.set(at);
    }

    assert_eq!(
        run_at,
        [
            "2025-01-31T23:58:30.000Z",
            "2025-01-31T23:59:30.000Z",
            "2025-02-01T00:01:30.000Z",
            "2025-02-01T00:05:30.000Z",
            "2025-02-01T00:13:30.000Z",
            "2025-02-01T00:23:30.000Z",
        ]
    );
    let rendered = table.to_string();
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 7);
    assert!(lines[0].starts_with("ATTEMPT  ID"));
    assert!(lines[6].starts_with("      6  "));
    assert!(lines[6].ends_with("2025-02-01T00:23:30.000Z  10m"));
}

#[test]
fn jittered_policies_never_exceed_their_bounds_over_many_jobs() {
    let policy: BackoffPolicy = "linear:1s+1s..5s~equal".parse().expect("valid policy");
    let mut rng = Rng::seed_from_u64(9);
    for attempt in 1..=10 {
        let base = policy.backoff.base_delay(attempt);
        for _ in 0..200 {
            let d = policy.delay_for(attempt, &mut rng);
            assert!(d >= base / 2 && d <= base, "attempt {attempt}: {d:?}");
        }
    }
}
