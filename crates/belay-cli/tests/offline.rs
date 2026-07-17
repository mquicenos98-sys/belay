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

#[test]
fn cron_errors_point_at_the_column() {
    let out = belay("cron next '0 61 * * *'").exits(Exit::Usage);
    assert!(
        out.stderr.starts_with(
            "error: invalid cron expression: invalid value 61 in hour field (0-23) at column 3\n  \
             0 61 * * *\n    ^\n"
        ),
        "{}",
        out.stderr
    );
    let out = belay("cron next '* * * * *' --tz Europe/Berln").exits(Exit::Usage);
    assert!(
        out.stderr.contains("did you mean \"Europe/Berlin\"?"),
        "{}",
        out.stderr
    );
    assert!(out
        .stderr
        .contains("hint: list known zones with `belay cron zones`"));
}

#[test]
fn cron_that_never_fires_fails_next_but_explains() {
    let out = belay("cron next '0 0 30 2 *'").exits(Exit::Failure);
    assert!(out.stderr.contains("never fires"), "{}", out.stderr);
    let out = belay("cron explain '0 0 30 2 *'").ok();
    assert!(
        out.stdout.contains("never: no future time matches"),
        "{}",
        out.stdout
    );
    let out = belay("cron next '* * * * *' -n 0").exits(Exit::Usage);
    assert!(out.stderr.contains("--count must be between 1 and 1000"));
}

#[test]
fn cron_zones_filters_case_insensitively() {
    assert_eq!(
        belay("cron zones new_york").ok().stdout,
        "America/New_York\n"
    );
    let out = belay("cron zones nowhere").ok();
    assert!(out.stdout.is_empty());
    assert!(out.stderr.contains("no zone matches"));
    assert!(belay("cron zones").ok().stdout.lines().count() > 50);
}

// ---- config ---------------------------------------------------------------

fn write(dir: &TempDir, name: &str, text: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, text).expect("write file");
    path.to_string_lossy().into_owned()
}

#[test]
fn config_check_accepts_a_valid_file_and_summarizes_it() {
    let dir = TempDir::new().unwrap();
    let path = write(
        &dir,
        "belay.toml",
        "[server]\nlisten = \"127.0.0.1:9100\"\n\n[queues.emails]\nmax_concurrency = 4\n\n\
         [[schedules]]\nid = \"nightly\"\ncron = \"0 2 * * *\"\n\
         template = { queue = \"reports\", kind = \"build\" }\n",
    );
    let out = belay(&format!("config check '{path}'")).ok();
    assert!(
        out.stdout.starts_with(&format!("{path}: ok\n")),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("listen     127.0.0.1:9100"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("queues     emails"), "{}", out.stdout);
    assert!(out.stdout.contains("schedules  nightly"), "{}", out.stdout);
    assert!(out.stderr.is_empty(), "{}", out.stderr);
}

#[test]
fn config_check_warns_about_typos_and_strict_fails_on_them() {
    let dir = TempDir::new().unwrap();
    let path = write(&dir, "belay.toml", "[sever]\nlisten = \"127.0.0.1:9100\"\n");
    let out = belay(&format!("config check '{path}'")).ok();
    assert!(
        out.stderr.contains("warning: unknown key `sever"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("did you mean `server"),
        "{}",
        out.stderr
    );
    let out = belay(&format!("config check '{path}' --strict")).exits(Exit::Failure);
    assert!(
        out.stderr.contains("has 1 warning (--strict)"),
        "{}",
        out.stderr
    );
    let json = belay(&format!("config check '{path}' --json")).ok().json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["warnings"].as_array().map(Vec::len), Some(1));
}

#[test]
fn config_check_reports_errors_as_diagnostics() {
    let dir = TempDir::new().unwrap();
    let path = write(
        &dir,
        "bad.toml",
        "[server]\nlisten = \"127.0.0.1:9100\"\n\n[queues.emails]\nmax_concurrency = 0\n",
    );
    let out = belay(&format!("config check '{path}'")).exits(Exit::Failure);
    assert!(
        out.stderr.starts_with(
            "error: invalid value for queues.emails.max_concurrency: must be at least 1"
        ),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("max_concurrency = 0"), "{}", out.stderr);
    assert!(out.stderr.contains('^'), "{}", out.stderr);
    assert!(out.stdout.is_empty());
}

#[test]
fn config_applies_environment_overrides_like_the_server() {
    let dir = TempDir::new().unwrap();
    let path = write(
        &dir,
        "belay.toml",
        "[server]\nlisten = \"127.0.0.1:9100\"\n",
    );
    let runner = runner().env("BELAY_SERVER__THREADS", "3");
    let out = run_with(&runner, &format!("config print '{path}'")).ok();
    assert!(out.stdout.contains("threads = 3"), "{}", out.stdout);
    let runner = runner.env("BELAY_SERVER__THREADS", "many");
    let out = run_with(&runner, &format!("config check '{path}'")).exits(Exit::Failure);
    assert!(
        out.stderr.contains("BELAY_SERVER__THREADS"),
        "{}",
        out.stderr
    );
}

#[test]
fn config_print_output_loads_back() {
    let dir = TempDir::new().unwrap();
    let out = belay("config print").ok();
    assert!(out.stdout.contains("[server]"), "{}", out.stdout);
    let path = write(&dir, "printed.toml", &out.stdout);
    assert_eq!(
        belay(&format!("config print '{path}'")).ok().stdout,
        out.stdout
    );
}

#[test]
fn config_default_is_a_valid_configuration() {
    let dir = TempDir::new().unwrap();
    let out = belay("config default").ok();
    assert!(out.stdout.contains("[server]"));
    let path = write(&dir, "example.toml", &out.stdout);
    belay(&format!("config check '{path}' --strict")).ok();
    belay("config default --json").exits(Exit::Usage);
}

#[test]
fn config_names_missing_files() {
    let out = belay("config check no/such/belay.toml").exits(Exit::Failure);
    assert!(
        out.stderr
            .starts_with("error: reading no/such/belay.toml: "),
        "{}",
        out.stderr
    );
    let runner = runner().env("BELAY_CONFIG", "also/missing.toml");
    let out = run_with(&runner, "config check").exits(Exit::Failure);
    assert!(out.stderr.contains("also/missing.toml"), "{}", out.stderr);
}

// ---- store-inspect --------------------------------------------------------

/// A closed store with a checkpoint and a few batches after it.
fn populated_store() -> TempDir {
    let dir = TempDir::new().unwrap();
    let kv = LogKv::open(dir.path(), Options::new().segment_size(4096)).expect("open store");
    for i in 0..40u32 {
        let mut batch = WriteBatch::new();
        batch.put(format!("job/{i:03}"), vec![b'x'; 200]);
        kv.write(batch).expect("write");
        if i == 19 {
            kv.checkpoint().expect("checkpoint");
        }
    }
    kv.close().expect("close");
    dir
}

#[test]
fn store_inspect_lists_files_and_verifies_them() {
    let dir = populated_store();
    let path = dir.path().to_string_lossy().into_owned();
    let out = belay(&format!("admin store-inspect '{path}'")).ok();
    let text = &out.stdout;
    assert!(text.starts_with("FILE"), "{text}");
    assert!(text.contains("snap-") && text.contains("wal-"), "{text}");
    assert!(
        text.contains("snapshot seq 20, recoverable through seq 40"),
        "{text}"
    );
    assert!(text.contains("no problems found"), "{text}");

    let json = belay(&format!("admin store-inspect '{path}' --json"))
        .ok()
        .json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["last_seq"], 40);
    assert!(json["files"].as_array().is_some_and(|f| f.len() >= 2));

    let out = belay(&format!("admin store-inspect '{path}' --records")).ok();
    assert!(
        out.stdout.contains("put \"job/039\" (200 bytes)"),
        "{}",
        out.stdout
    );
}

#[test]
fn store_inspect_fails_on_corruption_in_the_middle_of_the_log() {
    let dir = populated_store();
    let mut segments: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("wal-"))
        })
        .collect();
    segments.sort();
    assert!(
        segments.len() >= 2,
        "the test needs a segment that is not the last"
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&segments[0])
        .unwrap();
    file.seek(SeekFrom::Start(200)).unwrap();
    file.write_all(b"garbage!").unwrap();
    drop(file);

    let path = dir.path().to_string_lossy().into_owned();
    let out = belay(&format!("admin store-inspect '{path}'")).exits(Exit::Failure);
    assert!(out.stdout.contains("corrupt"), "{}", out.stdout);
    assert!(
        out.stderr.contains("that opening it would not survive"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("skip-corrupt"), "{}", out.stderr);
}

#[test]
fn store_inspect_needs_a_directory() {
    let out = belay("admin store-inspect no/such/dir").exits(Exit::Failure);
    assert!(out.stderr.contains("is not a directory"), "{}", out.stderr);
}
