//! [`ShellHandler`]: runs the command a job's payload describes.
//!
//! Output is captured from pipes by one thread per stream, keeping only
//! the last [`DEFAULT_MAX_OUTPUT`] bytes of each, so a chatty command
//! cannot exhaust memory or produce a result the source would reject. The
//! handler polls the child (std has no wait-with-timeout) with a delay
//! that grows from 1ms to 50ms, which keeps short commands fast and long
//! ones cheap, and kills the child when the payload's timeout passes or
//! the job is cancelled.
//!
//! `Child::kill` ends the direct child only. A command that starts its own
//! children (`sh -c "a | b"`, `cmd /C ...`) can leave them running after a
//! timeout; std offers no portable process groups. Such grandchildren may
//! also keep the output pipes open, so after the child exits the handler
//! waits at most a quarter of a second for end-of-file and then reports
//! what it has.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use belay_common::format_duration;
use belay_common::sync::lock;
use belay_json::{FromJson, FromJsonError, ObjectBuilder, ObjectReader, ToJson, Value};
use belay_proto::wire::{BuilderExt, ReaderExt};

use crate::context::JobContext;
use crate::handler::{failure_kind, Handler, HandlerError, Outcome};

/// Bytes of stdout and of stderr kept by default (the last 64 KiB).
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;

/// `EX_TEMPFAIL` from BSD `sysexits.h`: "temporary failure, try again".
/// The one exit status retried by default.
pub const EX_TEMPFAIL: i32 = 75;

/// How long to wait for the output pipes to close after the child exits.
const PIPE_DRAIN: Duration = Duration::from_millis(250);

/// Longest pause between two checks on the child.
const MAX_POLL: Duration = Duration::from_millis(50);

/// Output quoted in a failure message, at most.
const MAX_QUOTED_OUTPUT: usize = 1024;

/// The payload of a shell job.
///
/// ```
/// use std::time::Duration;
/// use belay_json::ToJson;
/// use belay_worker::builtin::ShellCommand;
///
/// let cmd = ShellCommand::new(["convert", "in.png", "-resize", "50%", "out.png"])
///     .env("MAGICK_THREAD_LIMIT", "1")
///     .timeout(Duration::from_secs(30));
/// assert_eq!(
///     cmd.to_json().to_string(),
///     r#"{"cmd":["convert","in.png","-resize","50%","out.png"],"env":{"MAGICK_THREAD_LIMIT":"1"},"timeout":"30s"}"#
/// );
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellCommand {
    /// Program and arguments; the program is looked up on `PATH`. No shell
    /// is involved unless the program is one (`["sh", "-c", "..."]`).
    pub cmd: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Working directory; the worker's own when absent.
    pub cwd: Option<String>,
    /// Text written to the command's standard input, which is otherwise
    /// empty.
    pub stdin: Option<String>,
    /// Kill the command after this long.
    pub timeout: Option<Duration>,
    /// Exit statuses worth retrying; the handler's default when absent
    /// (just [`EX_TEMPFAIL`] unless configured otherwise).
    pub retry_on_exit: Option<Vec<i32>>,
}

impl ShellCommand {
    /// Runs `cmd` (program first).
    #[must_use]
    pub fn new<I, S>(cmd: I) -> ShellCommand
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        ShellCommand {
            cmd: cmd.into_iter().map(Into::into).collect(),
            ..ShellCommand::default()
        }
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> ShellCommand {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<String>) -> ShellCommand {
        self.cwd = Some(dir.into());
        self
    }

    /// Sets the standard input.
    #[must_use]
    pub fn stdin(mut self, input: impl Into<String>) -> ShellCommand {
        self.stdin = Some(input.into());
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> ShellCommand {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the exit statuses to retry.
    #[must_use]
    pub fn retry_on_exit(mut self, codes: Vec<i32>) -> ShellCommand {
        self.retry_on_exit = Some(codes);
        self
    }
}

impl ToJson for ShellCommand {
    fn to_json(&self) -> Value {
        ObjectBuilder::new()
            .field("cmd", &self.cmd)
            .field_if(!self.env.is_empty(), "env", &self.env)
            .opt_field("cwd", &self.cwd)
            .opt_field("stdin", &self.stdin)
            .opt_duration("timeout", self.timeout)
            .opt_field("retry_on_exit", &self.retry_on_exit)
            .build()
    }
}

impl FromJson for ShellCommand {
    fn from_json(value: &Value) -> Result<ShellCommand, FromJsonError> {
        let r = ObjectReader::new(value, "ShellCommand")?;
        let cmd: Vec<String> = r.req("cmd")?;
        let command = ShellCommand {
            env: r.or_default("env")?,
            cwd: r.opt("cwd")?,
            stdin: r.opt("stdin")?,
            timeout: r.opt_duration("timeout")?,
            retry_on_exit: r.opt("retry_on_exit")?,
            cmd,
        };
        r.deny_unknown_fields()?;
        match command.cmd.first() {
            None => return Err(r.invalid("cmd", "must name a program")),
            Some(program) if program.is_empty() => {
                return Err(r.invalid("cmd", "program name must not be empty"))
            }
            Some(_) => {}
        }
        if command.cmd.iter().any(|a| a.contains('\0')) {
            return Err(r.invalid("cmd", "arguments must not contain NUL characters"));
        }
        for (key, value) in &command.env {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err(r.invalid(
                    "env",
                    format!("{key:?} is not a valid variable (names must be non-empty, without '=' or NUL)"),
                ));
            }
        }
        if command.cwd.as_deref().is_some_and(|d| d.contains('\0')) {
            return Err(r.invalid("cwd", "must not contain NUL characters"));
        }
        Ok(command)
    }
}

/// Runs the command in the payload (a [`ShellCommand`]).
///
/// Exit status 0 succeeds with `{"exit_code": 0, "stdout": "...",
/// "stderr": "..."}` (plus `"stdout_truncated": true` or
/// `"stderr_truncated": true` when output was cut). Any other status fails
/// with kind `exit:<status>` and the end of stderr in the message; the
/// failure is retryable only for the statuses in `retry_on_exit`. A
/// command killed by a signal fails retryably with kind `signal`.
///
/// Running what a payload says is running arbitrary code for anyone who
/// can enqueue: prefer an [`allow`](Self::allow) list.
#[derive(Clone, Debug)]
pub struct ShellHandler {
    allowed: Option<BTreeSet<String>>,
    max_output: usize,
    default_timeout: Option<Duration>,
    inherit_env: bool,
    retry_on_exit: Vec<i32>,
}

impl Default for ShellHandler {
    fn default() -> ShellHandler {
        ShellHandler::new()
    }
}

impl ShellHandler {
    /// Runs any program, keeps 64 KiB of each output stream, inherits the
    /// worker's environment and retries [`EX_TEMPFAIL`].
    #[must_use]
    pub fn new() -> ShellHandler {
        ShellHandler {
            allowed: None,
            max_output: DEFAULT_MAX_OUTPUT,
            default_timeout: None,
            inherit_env: true,
            retry_on_exit: Vec::new(),
        }
    }

    /// Only runs these programs, compared exactly with `cmd[0]` (list
    /// `/usr/bin/convert` separately from `convert` if both are wanted).
    /// Other programs fail permanently with kind `not_allowed`.
    #[must_use]
    pub fn allow<I, S>(mut self, programs: I) -> ShellHandler
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed
            .get_or_insert_with(BTreeSet::new)
            .extend(programs.into_iter().map(Into::into));
        self
    }

    /// Keeps the last `bytes` of stdout and of stderr.
    #[must_use]
    pub fn max_output(mut self, bytes: usize) -> ShellHandler {
        self.max_output = bytes;
        self
    }

    /// Timeout for payloads that do not set one.
    #[must_use]
    pub fn default_timeout(mut self, timeout: Duration) -> ShellHandler {
        self.default_timeout = Some(timeout);
        self
    }

    /// Starts commands with an empty environment plus the payload's `env`.
    #[must_use]
    pub fn clear_env(mut self) -> ShellHandler {
        self.inherit_env = false;
        self
    }

    /// Exit statuses retried when the payload does not say.
    #[must_use]
    pub fn retry_on_exit(mut self, codes: Vec<i32>) -> ShellHandler {
        self.retry_on_exit = codes;
        self
    }

    /// Runs `command` for the job in `ctx`.
    pub fn run(&self, ctx: &JobContext, command: &ShellCommand) -> Result<Outcome, HandlerError> {
        let program = command
            .cmd
            .first()
            .ok_or_else(|| HandlerError::invalid_payload("$.cmd: must name a program"))?;
        if let Some(allowed) = &self.allowed {
            if !allowed.contains(program) {
                return Err(HandlerError::fatal(format!(
                    "program {program:?} is not in the allowed list"
                ))
                .with_kind("not_allowed"));
            }
        }

        let mut cmd = Command::new(program);
        cmd.args(&command.cmd[1..])
            .stdin(if command.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !self.inherit_env {
            cmd.env_clear();
        }
        cmd.envs(&command.env);
        if let Some(dir) = &command.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| spawn_error(program, &e))?;

        let stdout = Arc::new(Mutex::new(Tail::new(self.max_output)));
        let stderr = Arc::new(Mutex::new(Tail::new(self.max_output)));
        let (done_tx, done_rx) = mpsc::channel();
        let started = start_io(&mut child, command, &stdout, &stderr, &done_tx);
        drop(done_tx);
        let readers = match started {
            Ok(readers) => readers,
            Err(e) => {
                kill(&mut child);
                return Err(
                    HandlerError::retry(format!("cannot start output readers: {e}"))
                        .with_kind(failure_kind::IO),
                );
            }
        };

        let timeout = command.timeout.or(self.default_timeout);
        let status = wait(ctx, &mut child, timeout);
        drain(&done_rx, readers, PIPE_DRAIN);
        let status = status?;

        let out = lock(&stdout);
        let err = lock(&stderr);
        match status.code() {
            Some(0) => Ok(Outcome::with(
                ObjectBuilder::new()
                    .field("exit_code", 0)
                    .field("stdout", out.text())
                    .field("stderr", err.text())
                    .field_if(out.truncated(), "stdout_truncated", true)
                    .field_if(err.truncated(), "stderr_truncated", true)
                    .build(),
            )),
            Some(code) => {
                let retry_on = command
                    .retry_on_exit
                    .as_deref()
                    .unwrap_or(&self.retry_on_exit);
                let message = with_output(format!("command exited with status {code}"), &err, &out);
                let error = if retry_on.contains(&code) {
                    HandlerError::retry(message)
                } else {
                    HandlerError::fatal(message)
                };
                Err(error.with_kind(format!("exit:{code}")))
            }
            None => Err(HandlerError::retry(with_output(
                format!("command was terminated by a signal ({status})"),
                &err,
                &out,
            ))
            .with_kind("signal")),
        }
    }
}

impl Handler for ShellHandler {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
        let command: ShellCommand = ctx.payload_as()?;
        self.run(ctx, &command)
    }
}
