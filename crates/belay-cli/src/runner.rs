//! Running one invocation: [`Runner`] holds what a command may read from
//! its surroundings (environment, stdin, whether stdout is a terminal, the
//! clock), so tests can run commands in isolation and in parallel.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};

use belay_common::log::{Filter, Level, LogFormat};
use belay_common::{system_clock, SharedClock};

use crate::args::{self, Parsed, UsageError};
use crate::commands::{self, Ctx};
use crate::error::{Error, Exit};
use crate::output::{style, Output};

/// Where stdin comes from.
#[derive(Clone, Debug)]
enum Stdin {
    Process,
    Bytes(Vec<u8>),
}

/// The surroundings of one `belay` invocation.
///
/// [`Runner::new`] is the real process: its environment, stdin and
/// terminal. [`Runner::isolated`] sees no environment variables, reads an
/// empty stdin and never colors, which is what tests want; both can be
/// adjusted with the builder methods.
///
/// ```
/// use belay_cli::{Exit, Runner};
///
/// let (mut out, mut err) = (Vec::new(), Vec::new());
/// let exit = Runner::isolated().run(["cron", "next", "0 9 * * MON", "-n", "1", "--from", "2025-03-04T00:00:00Z"], &mut out, &mut err);
/// assert_eq!(exit, Exit::Success);
/// assert_eq!(String::from_utf8(out).unwrap(), "2025-03-10T09:00:00Z\n");
/// ```
#[derive(Clone)]
pub struct Runner {
    inherit_env: bool,
    vars: BTreeMap<String, String>,
    stdin: Stdin,
    terminal: Option<bool>,
    clock: Option<SharedClock>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("inherit_env", &self.inherit_env)
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .field("stdin", &self.stdin)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl Default for Runner {
    fn default() -> Runner {
        Runner::new()
    }
}

impl Runner {
    /// The real process environment, stdin and terminal.
    #[must_use]
    pub fn new() -> Runner {
        Runner {
            inherit_env: true,
            vars: BTreeMap::new(),
            stdin: Stdin::Process,
            terminal: None,
            clock: None,
        }
    }

    /// No environment, empty stdin, not a terminal.
    #[must_use]
    pub fn isolated() -> Runner {
        Runner {
            inherit_env: false,
            vars: BTreeMap::new(),
            stdin: Stdin::Bytes(Vec::new()),
            terminal: Some(false),
            clock: None,
        }
    }

    /// Sets an environment variable for this invocation (on top of the
    /// process environment for [`Runner::new`]).
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Runner {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Makes stdin read `bytes`.
    #[must_use]
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Runner {
        self.stdin = Stdin::Bytes(bytes.into());
        self
    }

    /// Declares whether stdout is a terminal, which decides coloring.
    #[must_use]
    pub fn terminal(mut self, is_terminal: bool) -> Runner {
        self.terminal = Some(is_terminal);
        self
    }

    /// The clock used for "3m ago" and default start times.
    #[must_use]
    pub fn clock(mut self, clock: SharedClock) -> Runner {
        self.clock = Some(clock);
        self
    }

    /// An environment variable.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<String> {
        if let Some(value) = self.vars.get(name) {
            return Some(value.clone());
        }
        if self.inherit_env {
            return std::env::var(name).ok();
        }
        None
    }

    /// Every environment variable, for configuration overlays.
    #[must_use]
    pub fn vars(&self) -> Vec<(String, String)> {
        let mut all: BTreeMap<String, String> = if self.inherit_env {
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect()
        } else {
            BTreeMap::new()
        };
        all.extend(self.vars.clone());
        all.into_iter().collect()
    }

    pub(crate) fn read_stdin(&self) -> io::Result<Vec<u8>> {
        match &self.stdin {
            Stdin::Bytes(bytes) => Ok(bytes.clone()),
            Stdin::Process => {
                let mut bytes = Vec::new();
                io::stdin().lock().read_to_end(&mut bytes)?;
                Ok(bytes)
            }
        }
    }

    fn is_terminal(&self) -> bool {
        self.terminal.unwrap_or_else(|| io::stdout().is_terminal())
    }

    /// Runs `belay` with `args` (without the program name), writing to
    /// `out` and `err`, and returns the exit status.
    pub fn run<I, S>(&self, args: I, out: &mut dyn Write, err: &mut dyn Write) -> Exit
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let root = commands::tree();
        let lookup = |name: &str| self.var(name);
        let parsed = match args::parse(&root, args, &lookup) {
            Ok(parsed) => parsed,
            Err(e) => {
                // The flags did not parse, so there is no --no-color to go by.
                let color = self.is_terminal();
                let mut output = Output::new(out, err);
                output.color = color;
                report_usage(&mut output, &root, &e);
                return Exit::Usage;
            }
        };
        let m = match parsed {
            Parsed::Help(path) => {
                let text = args::render_help(&root, &path);
                return finish(out.write_all(text.as_bytes()));
            }
            Parsed::Version => {
                return finish(writeln!(out, "belay {}", env!("CARGO_PKG_VERSION")));
            }
            Parsed::Run(m) => m,
        };

        let mut output = Output::new(out, err);
        output.json = m.flag("json");
        output.quiet = m.flag("quiet");
        output.color = style::color_enabled(m.flag("no-color"), &lookup, self.is_terminal());
        let clock = self.clock.clone().unwrap_or_else(system_clock);
        let mut ctx = Ctx::new(output, self, clock, &m);
        install_logging(ctx.verbose, m.path());

        let result = commands::dispatch(&mut ctx, &m);
        let flushed = ctx.out.flush();
        match result.and(flushed.map_err(Error::from)) {
            Ok(()) => Exit::Success,
            Err(e) if e.is_broken_pipe() => Exit::Success,
            Err(Error::Usage(e)) => {
                report_usage(&mut ctx.out, &root, &e);
                Exit::Usage
            }
            Err(e) => {
                report(&mut ctx.out, &e, &ctx.url);
                if ctx.verbose > 0 {
                    if let Some((code, details)) = e.api_details() {
                        let pretty = belay_json::to_string_pretty(details);
                        let _ = ctx.out.err_text(&format!("details ({code}): {pretty}\n"));
                    }
                }
                e.exit()
            }
        }
    }
}

/// Runs `belay` in the real process environment; see [`Runner::run`].
pub fn run<I, S>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> Exit
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    Runner::new().run(args, out, err)
}
