//! The `belay` command: runs the server (`server`, `dev`) and workers
//! (`worker`), manages jobs, queues, schedules, calendars and workflows
//! over the HTTP API, and checks cron expressions, configuration files and
//! data directories offline.
//!
//! The binary is a thin `main` around [`run`]; everything else lives in
//! this library so it can be tested in-process with a [`Runner`] that
//! controls the environment, stdin and terminal detection.
//!
//! # Commands
//!
//! ```text
//! belay server [--config PATH]                 run from a configuration file
//! belay dev [--listen] [--data DIR|--in-memory] [--workers N] [--no-worker]
//! belay enqueue <QUEUE> <KIND> [--payload JSON|@FILE|-] [--priority N] [--delay D]
//!               [--run-at T] [--unique KEY[:SCOPE]] [--tag T]... [--meta K=V]... [--wait]
//! belay job      get | list | cancel | retry | promote | delete | wait
//! belay queue    list | info | pause | resume | purge | config [--set K=V] | delete
//! belay schedule add | list | get | pause | resume | rm | trigger | preview
//! belay calendar list | get | put | rm
//! belay workflow submit <FILE> | get | list | cancel
//! belay worker --queues a,b [--concurrency N] [--shell] [--allow PROGRAM]...
//! belay workers | members | info | version
//! belay events tail [--types PATTERNS] [--queue Q] [--from SEQ] [-n N]
//! belay cron     explain | next "<expr>" [--tz ZONE] [-n N] | zones
//! belay config   check [PATH] | print [PATH] | default
//! belay admin    checkpoint | store | verify | shutdown | store-inspect <DIR>
//! ```
//!
//! Global options: `--url` (`BELAY_URL`, default `http://127.0.0.1:7700`),
//! `--token` (`BELAY_TOKEN`), `--json`, `-q/--quiet`, `--no-color`
//! (also `NO_COLOR`), `-v/--verbose`. `belay help <command>` or
//! `<command> --help` explains each command with examples.
//!
//! # Output
//!
//! Human output is aligned tables and `label  value` blocks with rounded
//! times ("3m ago"); `--json` prints the API's JSON, pretty, one document
//! per command (one line per event for `events tail`); `--quiet` prints
//! only identifiers. Data goes to stdout, errors, warnings and notes to
//! stderr, so pipelines see only data.
//!
//! # Exit status
//!
//! See [`Exit`]: 0 success, 1 failure, 2 usage error, 3 not found, 4 server
//! unreachable or unavailable.
//!
//! # Limitations
//!
//! - The standard library cannot catch signals: Ctrl-C ends `server`,
//!   `dev` and `worker` at once. That is safe (the store is crash-consistent
//!   and leases expire), but jobs running in a killed worker only return to
//!   their queues after their lease runs out. `belay admin shutdown` stops
//!   a server gracefully.
//! - `job wait` and `--wait` poll the job rather than following the event
//!   stream, so they see a job's outcome up to half a second late.
//! - Plain `http://` only, like the client; put a TLS-terminating proxy in
//!   front of servers reached over untrusted networks.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod args;
mod commands;
mod error;
mod input;
mod output;
mod runner;

pub use commands::tree;
pub use error::{Error, Exit, Result};
pub use runner::{run, Runner};
