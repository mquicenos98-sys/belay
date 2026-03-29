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

impl Flag {
    /// A switch without a value, e.g. `--wait`.
    #[must_use]
    pub fn switch(long: &'static str, help: &'static str) -> Flag {
        Flag {
            long,
            short: None,
            value: None,
            help,
            repeated: false,
            default: None,
            env: None,
            conflicts: Vec::new(),
        }
    }

    /// An option taking a value shown as `<VALUE_NAME>` in help, e.g.
    /// `--priority <N>`.
    #[must_use]
    pub fn option(long: &'static str, value_name: &'static str, help: &'static str) -> Flag {
        Flag {
            value: Some(value_name),
            ..Flag::switch(long, help)
        }
    }

    /// Adds a one-letter form, `-p`.
    #[must_use]
    pub fn short(mut self, short: char) -> Flag {
        self.short = Some(short);
        self
    }

    /// Allows the flag more than once: options collect every value,
    /// switches count (`-vv`).
    #[must_use]
    pub fn repeated(mut self) -> Flag {
        self.repeated = true;
        self
    }

    /// The value used when the flag is absent (and no environment variable
    /// supplies one), shown in help.
    #[must_use]
    pub fn default(mut self, value: &'static str) -> Flag {
        self.default = Some(value);
        self
    }

    /// An environment variable that supplies the value when the flag is
    /// absent, shown in help.
    #[must_use]
    pub fn env(mut self, var: &'static str) -> Flag {
        self.env = Some(var);
        self
    }

    /// Makes giving both this flag and `other` (a long name) an error.
    #[must_use]
    pub fn conflicts_with(mut self, other: &'static str) -> Flag {
        self.conflicts.push(other);
        self
    }

    /// The long name, without dashes.
    #[must_use]
    pub fn long(&self) -> &'static str {
        self.long
    }

    /// Whether the flag takes a value.
    #[must_use]
    pub fn takes_value(&self) -> bool {
        self.value.is_some()
    }

    /// `--long`, as used in messages.
    #[must_use]
    pub fn display_name(&self) -> String {
        format!("--{}", self.long)
    }
}

/// A positional argument.
#[derive(Clone, Debug)]
pub struct Arg {
    pub(crate) name: &'static str,
    pub(crate) help: &'static str,
    pub(crate) required: bool,
    pub(crate) many: bool,
}

impl Arg {
    /// An argument that must be given.
    #[must_use]
    pub fn required(name: &'static str, help: &'static str) -> Arg {
        Arg {
            name,
            help,
            required: true,
            many: false,
        }
    }

    /// An argument that may be left out.
    #[must_use]
    pub fn optional(name: &'static str, help: &'static str) -> Arg {
        Arg {
            required: false,
            ..Arg::required(name, help)
        }
    }

    /// Takes every remaining positional token; only valid last.
    #[must_use]
    pub fn many(mut self) -> Arg {
        self.many = true;
        self
    }

    /// `<NAME>`, `[NAME]`, `<NAME>...` or `[NAME]...`.
    #[must_use]
    pub fn display_name(&self) -> String {
        let upper = self.name.to_ascii_uppercase().replace('-', "_");
        let base = if self.required {
            format!("<{upper}>")
        } else {
            format!("[{upper}]")
        };
        if self.many {
            base + "..."
        } else {
            base
        }
    }
}

/// A command: a leaf that runs, or a group of subcommands.
#[derive(Clone, Debug)]
pub struct Command {
    pub(crate) name: &'static str,
    pub(crate) about: &'static str,
    pub(crate) details: Option<&'static str>,
    pub(crate) aliases: Vec<&'static str>,
    pub(crate) args: Vec<Arg>,
    pub(crate) flags: Vec<Flag>,
    pub(crate) globals: Vec<Flag>,
    pub(crate) subcommands: Vec<Command>,
    pub(crate) examples: Vec<&'static str>,
}

impl Command {
    /// A command named `name`, described in one line by `about`.
    #[must_use]
    pub fn new(name: &'static str, about: &'static str) -> Command {
        Command {
            name,
            about,
            details: None,
            aliases: Vec::new(),
            args: Vec::new(),
            flags: Vec::new(),
            globals: Vec::new(),
            subcommands: Vec::new(),
            examples: Vec::new(),
        }
    }

    /// A longer description, printed in help under the one-line summary.
    #[must_use]
    pub fn details(mut self, text: &'static str) -> Command {
        self.details = Some(text);
        self
    }

    /// Another name the command answers to, e.g. `ls` for `list`.
    #[must_use]
    pub fn alias(mut self, alias: &'static str) -> Command {
        self.aliases.push(alias);
        self
    }

    /// Appends a positional argument.
    #[must_use]
    pub fn arg(mut self, arg: Arg) -> Command {
        self.args.push(arg);
        self
    }

    /// Appends a flag.
    #[must_use]
    pub fn flag(mut self, flag: Flag) -> Command {
        self.flags.push(flag);
        self
    }

    /// Appends a flag accepted by this command and every command below it.
    /// Only meaningful on the root.
    #[must_use]
    pub fn global(mut self, flag: Flag) -> Command {
        self.globals.push(flag);
        self
    }

    /// Appends a subcommand.
    #[must_use]
    pub fn subcommand(mut self, command: Command) -> Command {
        self.subcommands.push(command);
        self
    }

    /// Appends an example invocation, shown in help.
    #[must_use]
    pub fn example(mut self, line: &'static str) -> Command {
        self.examples.push(line);
        self
    }

    /// The command's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The one-line summary.
    #[must_use]
    pub fn about(&self) -> &'static str {
        self.about
    }

    /// The subcommands, in declaration order.
    #[must_use]
    pub fn subcommands(&self) -> &[Command] {
        &self.subcommands
    }

    /// The flags declared on this command (not the globals).
    #[must_use]
    pub fn flags(&self) -> &[Flag] {
        &self.flags
    }

    /// The positional arguments.
    #[must_use]
    pub fn args(&self) -> &[Arg] {
        &self.args
    }

    /// Whether this command only groups subcommands.
    #[must_use]
    pub fn is_group(&self) -> bool {
        !self.subcommands.is_empty()
    }

    /// The subcommand called `name` or aliased so.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Command> {
        self.subcommands
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
    }

    /// The chain of commands from `self` down along `path`; stops early at
    /// a name that does not exist.
    #[must_use]
    pub fn walk<'a>(&'a self, path: &[&str]) -> Vec<&'a Command> {
        let mut chain = vec![self];
        for name in path {
            let current = chain[chain.len() - 1];
            match current.find(name) {
                Some(next) => chain.push(next),
                None => break,
            }
        }
        chain
    }

    /// Checks the tree for mistakes that would make parsing ambiguous:
    /// duplicate names, clashing short flags, a `many` argument that is not
    /// last, or a required argument after an optional one. Returns one line
    /// per problem.
    #[must_use]
    pub fn lint(&self) -> Vec<String> {
        let mut problems = Vec::new();
        self.lint_into(&self.globals, &self.name.to_owned(), &mut problems);
        problems
    }

    fn lint_into(&self, globals: &[Flag], path: &str, problems: &mut Vec<String>) {
        let mut names: Vec<&str> = Vec::new();
        let mut shorts: Vec<char> = vec!['h'];
        for flag in globals.iter().chain(&self.flags) {
            if names.contains(&flag.long) || flag.long == "help" {
                problems.push(format!("{path}: duplicate flag --{}", flag.long));
            }
            names.push(flag.long);
            if let Some(short) = flag.short {
                if shorts.contains(&short) {
                    problems.push(format!("{path}: duplicate short flag -{short}"));
                }
                shorts.push(short);
            }
        }
        for flag in &self.flags {
            for other in &flag.conflicts {
                if !names.contains(other) {
                    problems.push(format!(
                        "{path}: --{} conflicts with unknown flag --{other}",
                        flag.long
                    ));
                }
            }
        }
        let mut seen_optional = false;
        for (i, arg) in self.args.iter().enumerate() {
            if names.contains(&arg.name) {
                problems.push(format!("{path}: argument {} shadows a flag", arg.name));
            }
            if arg.many && i + 1 != self.args.len() {
                problems.push(format!(
                    "{path}: {} takes many values but is not last",
                    arg.name
                ));
            }
            if arg.required && seen_optional {
                problems.push(format!(
                    "{path}: required {} follows an optional argument",
                    arg.name
                ));
            }
            seen_optional |= !arg.required;
        }
        if self.is_group() && !self.args.is_empty() {
            problems.push(format!("{path}: a group cannot take arguments"));
        }
        let mut commands: Vec<&str> = Vec::new();
        for sub in &self.subcommands {
            for name in std::iter::once(&sub.name).chain(&sub.aliases) {
                if commands.contains(name) {
                    problems.push(format!("{path}: duplicate command {name}"));
                }
                commands.push(name);
            }
            sub.lint_into(globals, &format!("{path} {}", sub.name), problems);
        }
    }
}
