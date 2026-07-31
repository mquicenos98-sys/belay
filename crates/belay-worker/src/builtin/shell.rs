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
            retry_on_exit: vec![EX_TEMPFAIL],
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

/// Starts the stdin feeder and the two output readers. Returns how many
/// readers will report on `done`.
fn start_io(
    child: &mut Child,
    command: &ShellCommand,
    stdout: &Arc<Mutex<Tail>>,
    stderr: &Arc<Mutex<Tail>>,
    done: &Sender<()>,
) -> io::Result<usize> {
    if let (Some(input), Some(mut pipe)) = (command.stdin.clone(), child.stdin.take()) {
        // A separate thread, so a command that does not read its input
        // cannot block us on a full pipe. Errors (the command exited or
        // closed stdin early) are its business, not ours.
        thread::Builder::new()
            .name("belay-shell-stdin".to_owned())
            .spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            })?;
    }
    let mut readers = 0;
    if let Some(pipe) = child.stdout.take() {
        spawn_reader("belay-shell-stdout", pipe, Arc::clone(stdout), done.clone())?;
        readers += 1;
    }
    if let Some(pipe) = child.stderr.take() {
        spawn_reader("belay-shell-stderr", pipe, Arc::clone(stderr), done.clone())?;
        readers += 1;
    }
    Ok(readers)
}

fn spawn_reader(
    name: &str,
    mut pipe: impl Read + Send + 'static,
    tail: Arc<Mutex<Tail>>,
    done: Sender<()>,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => lock(&tail).push(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = done.send(());
        })?;
    Ok(())
}

/// Waits for `readers` end-of-file notices, for at most `limit` in total.
fn drain(done: &Receiver<()>, readers: usize, limit: Duration) {
    let deadline = Instant::now() + limit;
    for _ in 0..readers {
        let left = deadline.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// Polls the child until it exits, the timeout passes or the job is
/// cancelled; kills it in the latter two cases.
fn wait(
    ctx: &JobContext,
    child: &mut Child,
    timeout: Option<Duration>,
) -> Result<ExitStatus, HandlerError> {
    let limit = timeout.and_then(|t| Instant::now().checked_add(t));
    let mut pause = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                kill(child);
                return Err(
                    HandlerError::retry(format!("waiting for the command failed: {e}"))
                        .with_kind(failure_kind::IO),
                );
            }
        }
        let mut nap = pause;
        if let (Some(limit), Some(timeout)) = (limit, timeout) {
            let left = limit.saturating_duration_since(Instant::now());
            if left.is_zero() {
                kill(child);
                return Err(HandlerError::retry(format!(
                    "command timed out after {}",
                    format_duration(timeout)
                ))
                .with_kind(failure_kind::TIMEOUT));
            }
            nap = nap.min(left);
        }
        if !ctx.sleep_or_cancel(nap) {
            kill(child);
            return Err(HandlerError::cancelled());
        }
        pause = (pause * 2).min(MAX_POLL);
    }
}

fn kill(child: &mut Child) {
    // Fails only if the child already exited, which is what we want.
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_error(program: &str, e: &io::Error) -> HandlerError {
    let error = match e.kind() {
        io::ErrorKind::NotFound => {
            HandlerError::fatal(format!("cannot run {program:?}: program not found"))
        }
        io::ErrorKind::PermissionDenied => {
            HandlerError::fatal(format!("cannot run {program:?}: permission denied"))
        }
        _ => HandlerError::retry(format!("cannot run {program:?}: {e}")),
    };
    error.with_kind("spawn")
}

/// Appends the end of stderr (or of stdout, if stderr is empty) to a
/// failure message.
fn with_output(mut message: String, stderr: &Tail, stdout: &Tail) -> String {
    let source = if stderr.is_empty() { stdout } else { stderr };
    let text = source.text();
    let text = text.trim();
    if !text.is_empty() {
        let mut start = text.len().saturating_sub(MAX_QUOTED_OUTPUT);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        message.push_str(": ");
        message.push_str(&text[start..]);
    }
    message
}

/// The last `cap` bytes written to a stream.
#[derive(Debug)]
struct Tail {
    buf: VecDeque<u8>,
    cap: usize,
    total: u64,
}

impl Tail {
    fn new(cap: usize) -> Tail {
        Tail {
            buf: VecDeque::with_capacity(cap.min(8192)),
            cap,
            total: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(data);
    }

    fn truncated(&self) -> bool {
        self.total > self.buf.len() as u64
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The kept bytes as text. When the start was cut off it may fall in
    /// the middle of a UTF-8 sequence; those stray continuation bytes are
    /// skipped rather than shown as replacement characters.
    fn text(&self) -> String {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        let start = if self.truncated() {
            bytes
                .iter()
                .position(|b| b & 0xC0 != 0x80)
                .unwrap_or(bytes.len())
        } else {
            0
        };
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    #[test]
    fn tail_keeps_the_last_bytes() {
        let mut t = Tail::new(5);
        t.push(b"abc");
        assert_eq!((t.text().as_str(), t.truncated()), ("abc", false));
        t.push(b"defg");
        assert_eq!((t.text().as_str(), t.truncated()), ("cdefg", true));
        t.push(b"0123456789");
        assert_eq!(t.text(), "56789");
        assert_eq!(t.total, 17);
    }

    #[test]
    fn tail_skips_a_cut_utf8_sequence() {
        let mut t = Tail::new(3);
        t.push("xé€".as_bytes()); // 1 + 2 + 3 bytes; the tail is the euro sign
        assert_eq!(t.text(), "€");
        let mut t = Tail::new(4);
        t.push("é€".as_bytes()); // keeps the last byte of 'é' and all of '€'
        assert_eq!(t.text(), "€");
    }

    #[test]
    fn a_zero_capacity_tail_keeps_nothing() {
        let mut t = Tail::new(0);
        t.push(b"abc");
        assert!(t.is_empty() && t.truncated());
        assert_eq!(t.text(), "");
    }

    #[test]
    fn payload_round_trips() {
        let cmd = ShellCommand::new(["prog", "arg"])
            .env("A", "1")
            .cwd("/tmp")
            .stdin("input")
            .timeout(Duration::from_secs(30))
            .retry_on_exit(vec![75, 1]);
        let json = cmd.to_json();
        assert_eq!(
            json,
            json!({"cmd": ["prog", "arg"], "env": {"A": "1"}, "cwd": "/tmp", "stdin": "input", "timeout": "30s", "retry_on_exit": [75, 1]})
        );
        assert_eq!(ShellCommand::from_json(&json), Ok(cmd));
    }

    #[test]
    fn payload_validation_names_the_field() {
        let err = |v: Value| ShellCommand::from_json(&v).unwrap_err().to_string();
        assert_eq!(err(json!({"cmd": []})), "$.cmd: must name a program");
        assert_eq!(
            err(json!({"cmd": [""]})),
            "$.cmd: program name must not be empty"
        );
        assert_eq!(
            err(json!({})),
            r#"$: missing required field "cmd" in ShellCommand"#
        );
        assert!(err(json!({"cmd": ["a"], "env": {"A=B": "1"}}))
            .starts_with("$.env: \"A=B\" is not a valid variable"));
        assert!(err(json!({"cmd": ["a"], "timeout": 30}))
            .starts_with("$.timeout: expected duration string"));
        assert!(err(json!({"cmd": ["a"], "tmeout": "1s"})).contains(r#"did you mean "timeout"?"#));
    }

    #[test]
    fn failure_messages_quote_the_end_of_the_output() {
        let mut err = Tail::new(64);
        let mut out = Tail::new(64);
        out.push(b"progress\n");
        assert_eq!(with_output("failed".into(), &err, &out), "failed: progress");
        err.push(b"  disk full\n");
        assert_eq!(
            with_output("failed".into(), &err, &out),
            "failed: disk full"
        );
        let empty = Tail::new(64);
        assert_eq!(with_output("failed".into(), &empty, &empty), "failed");
    }

    #[test]
    fn spawn_errors_distinguish_permanent_problems() {
        let e = spawn_error("nope", &io::Error::new(io::ErrorKind::NotFound, "x"));
        assert_eq!(e.message, r#"cannot run "nope": program not found"#);
        assert!(!e.retryable);
        let e = spawn_error("p", &io::Error::new(io::ErrorKind::WouldBlock, "busy"));
        assert!(e.retryable);
        assert_eq!(e.kind.as_deref(), Some("spawn"));
    }
}
