//! The shell handler against real processes. Commands go through
//! `cmd /C` on Windows and `sh -c` elsewhere, and stick to what both
//! shells spell the same way (echo, exit codes, redirection to stderr).

mod support;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_common::fs::TempDir;
use belay_common::Timestamp;
use belay_json::{ToJson, Value};
use belay_proto::{Job, JobId, JobSpec};
use belay_worker::builtin::{ShellCommand, ShellHandler, SHELL};
use belay_worker::testing::{JobOutcome, MemorySource};
use belay_worker::{failure_kind, Handler, HandlerError, JobContext, Registry};

use support::{config, spec, start, PATIENCE};

/// `script` run by the platform's shell.
fn shell(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), script.into()]
    } else {
        vec!["sh".into(), "-c".into(), script.into()]
    }
}

/// A command that runs for half a minute unless killed.
fn long_running() -> Vec<String> {
    if cfg!(windows) {
        ["ping", "-n", "30", "127.0.0.1"].map(String::from).to_vec()
    } else {
        ["sleep", "30"].map(String::from).to_vec()
    }
}

fn context_for(command: &ShellCommand) -> JobContext {
    let spec = JobSpec::new("jobs", SHELL).payload(command.to_json());
    JobContext::detached(Job::new(JobId::generate(), spec, Timestamp::now()))
}

fn run(handler: &ShellHandler, command: ShellCommand) -> Result<Value, HandlerError> {
    let mut ctx = context_for(&command);
    handler
        .handle(&mut ctx)
        .map(|outcome| outcome.into_result().expect("shell results carry a value"))
}

fn text(result: &Value, field: &str) -> String {
    result[field]
        .as_str()
        .expect("text field")
        .trim()
        .to_owned()
}

#[test]
fn successful_commands_report_their_output() {
    let result = run(&ShellHandler::new(), ShellCommand::new(shell("echo hello"))).unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(text(&result, "stdout"), "hello");
    assert_eq!(text(&result, "stderr"), "");
    assert!(result.get("stdout_truncated").is_none());
}

#[test]
fn stderr_is_captured_separately() {
    let result = run(
        &ShellHandler::new(),
        ShellCommand::new(shell("echo oops 1>&2")),
    )
    .unwrap();
    assert_eq!(text(&result, "stderr"), "oops");
    assert_eq!(text(&result, "stdout"), "");
}

#[test]
fn nonzero_exits_fail_permanently_by_default() {
    let err = run(&ShellHandler::new(), ShellCommand::new(shell("exit 3"))).unwrap_err();
    assert_eq!(err.message, "command exited with status 3");
    assert_eq!(err.kind.as_deref(), Some("exit:3"));
    assert!(!err.retryable);
}

#[test]
fn failure_messages_quote_stderr() {
    let err = run(
        &ShellHandler::new(),
        ShellCommand::new(shell("echo disk full 1>&2 && exit 1")),
    )
    .unwrap_err();
    assert_eq!(err.message, "command exited with status 1: disk full");
}

#[test]
fn payloads_and_handlers_choose_which_statuses_to_retry() {
    let from_payload = ShellCommand::new(shell("exit 3")).retry_on_exit(vec![3]);
    assert!(
        run(&ShellHandler::new(), from_payload)
            .unwrap_err()
            .retryable
    );
    let handler = ShellHandler::new().retry_on_exit(vec![3, 4]);
    assert!(
        run(&handler, ShellCommand::new(shell("exit 4")))
            .unwrap_err()
            .retryable
    );
    assert!(
        !run(&handler, ShellCommand::new(shell("exit 75")))
            .unwrap_err()
            .retryable
    );
}

#[test]
fn environment_and_working_directory_are_applied() {
    let dir = TempDir::new().expect("temp dir");
    let name = dir
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .expect("utf-8 dir name")
        .to_lowercase();
    let (print_var, print_dir) = if cfg!(windows) {
        ("echo %GREETING%", "cd")
    } else {
        ("echo $GREETING", "pwd")
    };
    let command = ShellCommand::new(shell(print_var)).env("GREETING", "hi there");
    assert_eq!(
        text(&run(&ShellHandler::new(), command).unwrap(), "stdout"),
        "hi there"
    );

    let cwd = dir.path().to_str().expect("utf-8 path");
    let command = ShellCommand::new(shell(print_dir)).cwd(cwd);
    let printed = text(&run(&ShellHandler::new(), command).unwrap(), "stdout").to_lowercase();
    assert!(printed.ends_with(&name), "{printed} should end with {name}");
}

#[test]
fn stdin_is_fed_to_the_command() {
    let reader: Vec<String> = if cfg!(windows) {
        vec!["findstr".into(), "^".into()]
    } else {
        vec!["cat".into()]
    };
    let command = ShellCommand::new(reader).stdin("line one\nline two\n");
    let result = run(&ShellHandler::new(), command).unwrap();
    let lines: Vec<String> = text(&result, "stdout")
        .lines()
        .map(|l| l.trim().to_owned())
        .collect();
    assert_eq!(lines, ["line one", "line two"]);
}

#[test]
fn output_is_cut_to_the_configured_tail() {
    let handler = ShellHandler::new().max_output(8);
    let result = run(&handler, ShellCommand::new(shell("echo 0123456789abcdef"))).unwrap();
    let stdout = result["stdout"].as_str().expect("stdout");
    assert!(stdout.len() <= 8, "{stdout:?}");
    assert!(stdout.trim_end().ends_with("cdef"), "{stdout:?}");
    assert_eq!(result["stdout_truncated"], true);
}

#[test]
fn commands_past_their_timeout_are_killed() {
    let started = Instant::now();
    let command = ShellCommand::new(long_running()).timeout(Duration::from_millis(100));
    let err = run(&ShellHandler::new(), command).unwrap_err();
    assert_eq!(err.message, "command timed out after 100ms");
    assert_eq!(err.kind.as_deref(), Some(failure_kind::TIMEOUT));
    assert!(err.retryable);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the child was killed"
    );
}

#[test]
fn the_handler_default_timeout_applies_when_the_payload_has_none() {
    let handler = ShellHandler::new().default_timeout(Duration::from_millis(50));
    let err = run(&handler, ShellCommand::new(long_running())).unwrap_err();
    assert_eq!(err.message, "command timed out after 50ms");
}

#[test]
fn cancellation_kills_the_command() {
    let command = ShellCommand::new(long_running());
    let mut ctx = context_for(&command);
    let cancel = ctx.cancel_handle();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        cancel.cancel();
    });
    let started = Instant::now();
    let err = ShellHandler::new().handle(&mut ctx).unwrap_err();
    assert!(err.is_cancellation());
    assert!(started.elapsed() < Duration::from_secs(10));
    canceller.join().expect("canceller thread");
}

#[test]
fn missing_programs_fail_permanently() {
    let err = run(
        &ShellHandler::new(),
        ShellCommand::new(["belay-no-such-program-4f2a"]),
    )
    .unwrap_err();
    assert_eq!(
        err.message,
        r#"cannot run "belay-no-such-program-4f2a": program not found"#
    );
    assert!(!err.retryable);
}

#[test]
fn the_allow_list_blocks_other_programs() {
    let handler = ShellHandler::new().allow(["convert"]);
    let err = run(&handler, ShellCommand::new(shell("echo hi"))).unwrap_err();
    assert_eq!(err.kind.as_deref(), Some("not_allowed"));
    assert!(!err.retryable);
    let program = shell("echo hi").remove(0);
    let allowed = ShellHandler::new().allow([program]);
    assert!(run(&allowed, ShellCommand::new(shell("echo hi"))).is_ok());
}

#[test]
fn bad_payloads_are_rejected_before_anything_runs() {
    let mut ctx = JobContext::detached(Job::new(
        JobId::generate(),
        JobSpec::new("jobs", SHELL).payload(belay_json::json!({"cmd": []})),
        Timestamp::now(),
    ));
    let err = ShellHandler::new().handle(&mut ctx).unwrap_err();
    assert_eq!(err.message, "invalid payload: $.cmd: must name a program");
    assert_eq!(err.kind.as_deref(), Some(failure_kind::INVALID_PAYLOAD));
}

#[test]
fn shell_jobs_run_through_a_worker() {
    let source = Arc::new(MemorySource::new());
    let id =
        source.push(spec(SHELL).payload(ShellCommand::new(shell("echo from a worker")).to_json()));
    let failing = source.push(spec(SHELL).payload(ShellCommand::new(shell("exit 2")).to_json()));
    let registry = Registry::new().register(SHELL, ShellHandler::new());
    let worker = start(&source, registry, config(2));

    assert!(source.wait_until(PATIENCE, |v| v.reported() == 2));
    match source.outcome(&id) {
        Some(JobOutcome::Completed(Some(result))) => {
            assert_eq!(text(&result, "stdout"), "from a worker");
        }
        other => panic!("unexpected outcome {other:?}"),
    }
    let failure = source
        .outcome(&failing)
        .and_then(|o| o.failure().cloned())
        .expect("failed");
    assert_eq!(failure.kind.as_deref(), Some("exit:2"));
    drop(worker);
}
