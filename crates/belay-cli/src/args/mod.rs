//! A small command-line parser: a tree of [`Command`]s, each with
//! positional [`Arg`]s and [`Flag`]s, parsed into [`Matches`] with typed
//! accessors, plus generated help and "did you mean" suggestions.
//!
//! The grammar is the usual GNU-style one:
//!
//! - long flags `--priority 5` or `--priority=5`, short flags `-p 5`,
//!   `-p5`, and clusters of short switches `-qv`;
//! - `--` ends flag parsing, so later tokens are positional even when they
//!   start with `-`; a lone `-` is positional (it means stdin);
//! - a value-taking flag consumes the next token even when it starts with a
//!   single dash (`--priority -5`), but not when it is another long flag,
//!   which reports the missing value instead of swallowing `--wait`;
//! - global flags (declared on the root) are accepted before or after any
//!   subcommand;
//! - `-h`/`--help` anywhere shows the help of the command reached so far,
//!   and `help <command>...` does the same.
//!
//! ```
//! use belay_cli::args::{parse, Arg, Command, Flag, Parsed};
//!
//! let root = Command::new("belay", "job queue")
//!     .global(Flag::switch("json", "Print JSON"))
//!     .subcommand(
//!         Command::new("enqueue", "Enqueue a job")
//!             .arg(Arg::required("queue", "Queue name"))
//!             .flag(Flag::option("priority", "N", "Priority").short('p').default("0")),
//!     );
//! let no_env = |_: &str| None;
//! let Parsed::Run(m) = parse(&root, ["enqueue", "emails", "-p", "7", "--json"], &no_env)? else {
//!     unreachable!("not a help request");
//! };
//! assert_eq!(m.path(), ["enqueue"]);
//! assert_eq!(m.get::<i16>("priority")?, Some(7));
//! assert_eq!(m.arg("queue"), Some("emails"));
//! assert!(m.flag("json"));
//! # Ok::<(), belay_cli::args::UsageError>(())
//! ```

mod error;

mod help;

mod matches;

mod parse;

mod suggest;

mod value;

pub use error::{UsageError, UsageErrorKind};
pub use help::{render_help, usage_line};
pub use matches::Matches;
pub use parse::{parse, Parsed};
pub use suggest::{edit_distance, suggest};
pub use value::{FromArg, KeyValue};

/// A flag: `--long`, optionally with a short form, either a switch or an
/// option that takes a value.
#[derive(Clone, Debug)]
pub struct Flag {
    pub(crate) long: &'static str,
    pub(crate) short: Option<char>,
    pub(crate) value: Option<&'static str>,
    pub(crate) help: &'static str,
    pub(crate) repeated: bool,
    pub(crate) default: Option<&'static str>,
    pub(crate) env: Option<&'static str>,
    pub(crate) conflicts: Vec<&'static str>,
}
