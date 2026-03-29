//! What can make a `belay` command fail, how each failure is shown, and
//! which exit status it gives.

use std::fmt;
use std::io;

use belay_proto::api::ErrorCode;

use crate::args::UsageError;

/// The process exit status of a `belay` invocation.
///
/// | status | meaning |
/// |---|---|
/// | 0 | success |
/// | 1 | the command failed (server refused, invalid input file, job died while waiting, verification problems) |
/// | 2 | the command line is wrong (unknown flag, bad value, missing argument) |
/// | 3 | the named job, queue, schedule, calendar or workflow does not exist |
/// | 4 | the server is unreachable, unavailable or shedding load; retrying later may help |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Exit {
    /// 0.
    Success,
    /// 1.
    Failure,
    /// 2.
    Usage,
    /// 3.
    NotFound,
    /// 4.
    Unavailable,
}

impl Exit {
    /// The numeric status.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Failure => 1,
            Exit::Usage => 2,
            Exit::NotFound => 3,
            Exit::Unavailable => 4,
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(exit: Exit) -> std::process::ExitCode {
        std::process::ExitCode::from(exit.code())
    }
}

/// A failed command.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The command line is wrong.
    Usage(UsageError),
    /// A call to the server failed.
    Client(belay_client::Error),
    /// A configuration file or override is invalid. Its `Display` is a
    /// complete multi-line diagnostic.
    Config(belay_core::config::ConfigError),
    /// The engine could not start.
    Engine(belay_core::Error),
    /// The HTTP server could not start.
    Server(belay_server::Error),
    /// The worker could not start.
    Worker(belay_worker::Error),
    /// A store directory could not be inspected.
    Store(belay_store::StoreError),
    /// Reading or writing a file or stream failed.
    Io {
        /// What was being done, e.g. `reading spec.json`.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// Input content (a file, stdin) is not what the command needs.
    Input(String),
    /// The command ran but the outcome is a failure, e.g. a waited-for job
    /// died or verification found problems.
    Failed {
        /// What happened.
        message: String,
        /// What to do about it.
        hint: Option<String>,
    },
    /// Some items of a multi-item command failed; each failure has already
    /// been reported.
    Partial {
        /// E.g. `2 of 3 jobs could not be cancelled`.
        message: String,
        /// The status of the first failure.
        exit: Exit,
    },
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn failed(message: impl Into<String>) -> Error {
        Error::Failed {
            message: message.into(),
            hint: None,
        }
    }

    /// The exit status this failure gives.
    #[must_use]
    pub fn exit(&self) -> Exit {
        match self {
            Error::Usage(_) => Exit::Usage,
            Error::Client(e) => match e {
                belay_client::Error::Api(api) => match api.code {
                    ErrorCode::NotFound => Exit::NotFound,
                    ErrorCode::Unavailable => Exit::Unavailable,
                    _ => Exit::Failure,
                },
                belay_client::Error::Transport(t) if t.is_transient() || refused(t) => {
                    Exit::Unavailable
                }
                _ => Exit::Failure,
            },
            Error::Partial { exit, .. } => *exit,
            _ => Exit::Failure,
        }
    }

    /// A suggestion for what to do next, given the server URL in use.
    #[must_use]
    pub fn hint(&self, url: &str) -> Option<String> {
        match self {
            Error::Usage(e) => e.hint(),
            Error::Client(belay_client::Error::Transport(t)) if refused(t) || t.is_timeout() => {
                Some(format!(
                    "is a belay server running at {url}? Start one with `belay dev`, \
                     or point --url (BELAY_URL) at yours"
                ))
            }
            Error::Client(belay_client::Error::Api(api)) => match api.code {
                ErrorCode::Unauthenticated => {
                    Some("pass a token with --token or set BELAY_TOKEN".to_owned())
                }
                ErrorCode::PermissionDenied => Some(
                    "the token lacks the scope this call needs (admin, enqueue, work or read)"
                        .to_owned(),
                ),
                _ => None,
            },
            Error::Client(belay_client::Error::Config(_)) => {
                Some("--url takes an address like http://127.0.0.1:7700".to_owned())
            }
            Error::Server(belay_server::Error::Bind { .. }) => Some(
                "another process may be using the address; pick another with --listen \
                 (dev) or server.listen"
                    .to_owned(),
            ),
            Error::Failed { hint, .. } => hint.clone(),
            _ => None,
        }
    }

    /// The server's error code and structured details, when the server
    /// sent details (shown with `-v`).
    #[must_use]
    pub fn api_details(&self) -> Option<(ErrorCode, &belay_json::Value)> {
        match self {
            Error::Client(belay_client::Error::Api(api)) => {
                api.details.as_ref().map(|details| (api.code, details))
            }
            _ => None,
        }
    }

    /// Whether this is a closed stdout (`belay job list | head`), which is
    /// not worth reporting.
    #[must_use]
    pub fn is_broken_pipe(&self) -> bool {
        matches!(self, Error::Io { source, .. } if source.kind() == io::ErrorKind::BrokenPipe)
    }
}

/// Whether the TCP connection was refused, i.e. nothing listens there.
fn refused(e: &belay_http::Error) -> bool {
    matches!(e, belay_http::Error::Io(io) if matches!(
        io.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::AddrNotAvailable
    ))
}
