//! Help output, version, and how command-line mistakes are reported.

mod common;

use belay_cli::Exit;
use common::{belay, run_with, runner, words};

#[test]
fn enqueue_help_snapshot() {
    let out = belay("enqueue --help").ok();
    assert_eq!(
        out.stdout,
        r#"Enqueue a job

Adds a job to QUEUE for workers that handle KIND. Unless --delay or --run-at
say otherwise the job is ready at once. With --wait the command polls until
the job finishes, prints its result and fails (status 1) if it died.

Usage: belay enqueue [OPTIONS] <QUEUE> <KIND>

Arguments:
  <QUEUE>  Queue to put the job on
  <KIND>   Job kind: the handler name workers dispatch on

Options:
  -d, --payload <JSON>           Payload: inline JSON, @FILE, or - for stdin
  -p, --priority <N>             Priority from -1000 to 1000; higher runs first
      --max-attempts <N>         Attempts before the job is dead (default: the queue's)
      --retry <POLICY>           Retry backoff such as exp:1s*2..10m (default: the queue's)
      --timeout <DURATION>       Execution budget per attempt (default: the queue's)
  -t, --tag <TAG>                Tag the job (repeatable)
      --tenant <TENANT>          Tenant, for fair sharing within the queue
  -m, --meta <KEY=VALUE>         Free-form annotation (repeatable)
      --delay <DURATION>         Run no earlier than this from now
      --run-at <TIME>            Run no earlier than this RFC 3339 time
      --unique <KEY[:SCOPE]>     Deduplicate on KEY; SCOPE is until_started, until_finished or for:DURATION
      --on-conflict <POLICY>     When KEY is taken: reject, return_existing or replace
  -w, --wait                     Wait for the job to finish and print its result
      --wait-timeout <DURATION>  Stop waiting after this long
  -h, --help                     Print help (see more with 'belay help <COMMAND>')

Global options:
      --url <URL>      Server address [env: BELAY_URL] [default: http://127.0.0.1:7700]
      --token <TOKEN>  Bearer token [env: BELAY_TOKEN]
      --json           Print JSON instead of tables
  -q, --quiet          Print only identifiers, no confirmations
      --no-color       Never color output (also NO_COLOR)
  -v, --verbose        Log more; repeat for even more (-vv)

Examples:
  belay enqueue emails send_welcome --payload '{"user": 42}'
  belay enqueue reports build --delay 10m --unique daily-report:for:1h
  belay enqueue default echo -d @payload.json --wait
"#
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn group_help_snapshot() {
    let out = belay("queue --help").ok();
    assert_eq!(
        out.stdout,
        "Inspect and manage queues

Usage: belay queue <COMMAND>

Commands:
  list    List queues with their job counts (alias: ls)
  info    Show a queue's counts, throughput and configuration (alias: get)
  pause   Stop handing out jobs from a queue (enqueueing continues)
  resume  Hand out jobs from a paused queue again
  purge   Remove a queue's jobs, or only those in one state
  config  Show or change a queue's configuration
  delete  Delete a queue and its configuration (alias: rm)

Options:
  -h, --help  Print help (see more with 'belay help <COMMAND>')

Global options:
      --url <URL>      Server address [env: BELAY_URL] [default: http://127.0.0.1:7700]
      --token <TOKEN>  Bearer token [env: BELAY_TOKEN]
      --json           Print JSON instead of tables
  -q, --quiet          Print only identifiers, no confirmations
      --no-color       Never color output (also NO_COLOR)
  -v, --verbose        Log more; repeat for even more (-vv)

Run 'belay queue <COMMAND> --help' for more on a command.
"
    );
}

#[test]
fn root_help_lists_every_top_level_command_and_exit_codes() {
    for line in ["", "--help", "help"] {
        let out = belay(line).ok();
        assert!(
            out.stdout
                .starts_with("Durable job queue and cron scheduler\n"),
            "{}",
            out.stdout
        );
        for command in [
            "server", "dev", "enqueue", "job", "queue", "schedule", "calendar", "workflow",
            "worker", "workers", "events", "cron", "config", "admin", "members", "info", "version",
        ] {
            let listed = format!("\n  {command} ");
            assert!(
                out.stdout.contains(&listed),
                "{command} missing from root help"
            );
        }
        assert!(out.stdout.contains("3 not found"), "{}", out.stdout);
    }
}

#[test]
fn help_subcommand_and_flag_agree() {
    let a = belay("help schedule add").ok();
    let b = belay("schedule add --help").ok();
    let c = belay("schedule add -h --bogus").ok();
    assert_eq!(a.stdout, b.stdout);
    assert_eq!(b.stdout, c.stdout);
    assert!(a
        .stdout
        .contains("Usage: belay schedule add [OPTIONS] [ID]"));
}

#[test]
fn version_flag_and_command() {
    let version = env!("CARGO_PKG_VERSION");
    for line in ["--version", "-V", "version"] {
        assert_eq!(belay(line).ok().stdout, format!("belay {version}\n"));
    }
    assert_eq!(belay("version -q").ok().stdout, format!("{version}\n"));
    assert_eq!(belay("version --json").ok().json()["version"], version);
}
