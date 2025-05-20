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
/// Kind name [`defaults`] uses for [`FailHandler`].
pub const FAIL: &str = "fail";
/// Conventional kind name for [`ShellHandler`].
pub const SHELL: &str = "shell";

/// A registry with `echo`, `sleep` and `fail`.
///
/// ```
/// use belay_worker::builtin::{self, ShellHandler};
///
/// let registry = builtin::defaults()
///     .register(builtin::SHELL, ShellHandler::new().allow(["convert"]));
/// assert_eq!(registry.kinds().collect::<Vec<_>>(), ["echo", "fail", "shell", "sleep"]);
/// ```
#[must_use]
pub fn defaults() -> Registry {
    Registry::new()
        .register(ECHO, EchoHandler)
        .register(SLEEP, SleepHandler::new())
        .register(FAIL, FailHandler::new("failed on purpose"))
}
