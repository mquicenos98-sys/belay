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
