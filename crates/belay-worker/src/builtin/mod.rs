//! Handlers that ship with belay: [`ShellHandler`] runs a command,
//! [`EchoHandler`], [`SleepHandler`] and [`FailHandler`] exist for trying
//! things out and for testing retries, timeouts and cancellation end to
//! end.
//!
//! [`defaults`] registers the harmless ones under their usual names.
//! The shell handler is never registered implicitly: it runs whatever a
//! job's payload says, so enabling it is a decision (ideally with
//! [`ShellHandler::allow`]).

mod shell;

mod simple;

pub use shell::{ShellCommand, ShellHandler, DEFAULT_MAX_OUTPUT, EX_TEMPFAIL};
pub use simple::{EchoHandler, FailHandler, SleepHandler};

use crate::registry::Registry;

/// Kind name [`defaults`] uses for [`EchoHandler`].
pub const ECHO: &str = "echo";

/// Kind name [`defaults`] uses for [`SleepHandler`].
pub const SLEEP: &str = "sleep";
