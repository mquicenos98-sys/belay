//! Commands that need no server: cron expressions, configuration files and
//! store directories.

mod common;

use std::fs;
use std::io::{Seek, SeekFrom, Write};

use belay_cli::Exit;
use belay_common::fs::TempDir;
use belay_json::json;
use belay_store::{Kv, LogKv, Options, WriteBatch};
use common::{belay, run_with, runner};

// ---- cron -----------------------------------------------------------------

#[test]
fn cron_next_prints_one_time_per_line_in_the_zone() {
    let out = belay(
        "cron next '30 9 * * MON-FRI' --tz America/New_York -n 3 --from 2025-03-07T15:00:00Z",
    )
    .ok();
    // The US switches to daylight time on Sunday 2025-03-09.
    assert_eq!(
        out.stdout,
        "2025-03-10T09:30:00-04:00\n2025-03-11T09:30:00-04:00\n2025-03-12T09:30:00-04:00\n"
    );
}

#[test]
fn cron_next_defaults_to_the_runner_clock_and_five_times() {
    let out = belay("cron next @daily").ok();
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0], "2025-03-05T00:00:00Z");
    let json = belay("cron next @hourly -n 2 --json").ok().json();
    assert_eq!(
        json,
        json!(["2025-03-04T06:00:00Z", "2025-03-04T07:00:00Z"])
    );
}

#[test]
fn cron_explain_describes_and_warns_about_clock_changes() {
    let out =
        belay("cron explain '0 30 2 * * *' --tz Europe/Berlin --from 2025-03-28T12:00:00Z -n 3")
            .ok();
    let text = &out.stdout;
    assert!(text.starts_with("expression  0 30 2 * * *\n"), "{text}");
    assert!(text.contains("meaning     at 02:30 every day"), "{text}");
    assert!(
        text.contains("time zone   Europe/Berlin (CET +01:00)"),
        "{text}"
    );
    // 2025-03-30 02:30 does not exist in Berlin: it fires at 03:00 CEST.
    assert!(text.contains("2025-03-30T03:00:00+02:00  Sun"), "{text}");
    assert!(
        text.contains(
            "note        clocks go forward at 2025-03-30T01:00:00.000Z (+01:00 to +02:00)"
        ),
        "{text}"
    );
}

#[test]
fn cron_explain_json_is_structured() {
    let json = belay("cron explain '*/15 * * * *' -n 2 --json --from 2025-03-04T05:00:00Z")
        .ok()
        .json();
    assert_eq!(json["description"], "every 15 minutes");
    assert_eq!(json["timezone"], "UTC");
    assert_eq!(
        json["next"],
        json!(["2025-03-04T05:15:00Z", "2025-03-04T05:30:00Z"])
    );
}
