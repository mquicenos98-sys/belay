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

#[test]
fn unknown_commands_suggest_the_closest_one() {
    let out = belay("schedul list").exits(Exit::Usage);
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        "error: unknown command \"schedul\" for \"belay\"\n\
         hint: did you mean \"schedule\"?\n\
         \n\
         Usage: belay [OPTIONS] <COMMAND>\n\
         For more information, try 'belay --help'.\n"
    );
}

#[test]
fn unknown_flags_suggest_and_show_the_command_usage() {
    let out = belay("enqueue emails send --prioirty 5").exits(Exit::Usage);
    assert_eq!(
        out.stderr,
        "error: unknown option --prioirty for \"belay enqueue\"\n\
         hint: did you mean --priority?\n\
         \n\
         Usage: belay enqueue [OPTIONS] <QUEUE> <KIND>\n\
         For more information, try 'belay enqueue --help'.\n"
    );
}

#[test]
fn typed_values_are_checked_before_anything_is_sent() {
    // No server is configured: a request would fail with status 4, not 2.
    let cases = [
        (
            "enqueue q k --priority abc",
            r#"invalid value "abc" for --priority: expected an integer"#,
        ),
        (
            "enqueue q k --priority 40000",
            r#"invalid value "40000" for --priority: expected an integer between -32768 and 32767"#,
        ),
        (
            "enqueue q k --priority 5000",
            "invalid job: $.priority: must be between -1000 and 1000, got 5000",
        ),
        ("enqueue Emails k", r#"invalid value "Emails" for <QUEUE>:"#),
        (
            "job list --state dead,zombie",
            r#"invalid value "zombie" for --state: unknown job state "zombie""#,
        ),
        (
            "enqueue q k --delay soon",
            r#"invalid value "soon" for --delay:"#,
        ),
        (
            "enqueue q k --payload {oops",
            "invalid value for --payload: invalid JSON:",
        ),
        (
            "enqueue q k --meta novalue",
            r#"invalid value "novalue" for --meta: expected KEY=VALUE"#,
        ),
        (
            "job get not-an-id",
            r#"invalid value "not-an-id" for <ID>:"#,
        ),
        (
            "enqueue q k --delay 1m --run-at 2025-01-01T00:00:00Z",
            "--delay cannot be used together with --run-at",
        ),
        (
            "enqueue q k --on-conflict reject",
            "--on-conflict needs --unique",
        ),
        ("job", "\"belay job\" needs a command"),
        ("job get", "missing argument <ID> for \"belay job get\""),
        (
            "queue info a b",
            "unexpected argument \"b\" for \"belay queue info\"",
        ),
        (
            "enqueue q k --wait=yes",
            "option --wait does not take a value, got \"yes\"",
        ),
        (
            "enqueue q k --payload",
            "option --payload needs a value (--payload <JSON>)",
        ),
        (
            "enqueue q k -p 1 -p 2",
            "option --priority was given more than once",
        ),
    ];
    for (line, message) in cases {
        let out = belay(line).exits(Exit::Usage);
        assert!(
            out.stderr.starts_with(&format!("error: {message}")),
            "{line}: {}",
            out.stderr
        );
        assert!(out.stdout.is_empty(), "{line}");
    }
}

#[test]
fn invalid_environment_values_name_their_variable() {
    let out = run_with(&runner().env("BELAY_URL", "ftp://nowhere"), "info").exits(Exit::Failure);
    assert!(
        out.stderr.contains("base_url must use http://"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("hint: --url takes an address"),
        "{}",
        out.stderr
    );
}

#[test]
fn test_command_lines_split_like_a_shell() {
    assert_eq!(
        words(r#"enqueue q k -d '{"a": 1}' --cron "0 9 * * MON"  x''"#),
        [
            "enqueue",
            "q",
            "k",
            "-d",
            r#"{"a": 1}"#,
            "--cron",
            "0 9 * * MON",
            "x"
        ]
    );
    assert_eq!(words("a '' b"), ["a", "", "b"]);
    assert!(words("  ").is_empty());
}
