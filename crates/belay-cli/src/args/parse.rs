//! The parser proper: walks the tokens once, descending into subcommands
//! and collecting flag and argument values, then checks arity, repeats and
//! conflicts, and fills in environment and default values.

use std::ffi::OsString;

use super::error::{UsageError, UsageErrorKind};
use super::matches::{Entry, Matches, Source};
use super::suggest::suggest;
use super::{Command, Flag};

/// What the command line asks for.
#[derive(Clone, Debug)]
pub enum Parsed {
    /// Run the command in [`Matches::path`].
    Run(Matches),
    /// Show the help of the command at this path.
    Help(Vec<&'static str>),
    /// Show the version (`--version` on the root).
    Version,
}

/// Parses `args` (without the program name) against the tree under
/// `root`. `env` looks up the environment variables flags fall back to;
/// empty values count as unset.
///
/// A help request wins over every other problem on the line, so
/// `belay enqueue --prio 5 --help` shows help rather than an error.
pub fn parse<I, S>(
    root: &Command,
    args: I,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Parsed, UsageError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut tokens = Vec::new();
    for arg in args {
        match arg.into().into_string() {
            Ok(s) => tokens.push(s),
            Err(raw) => {
                return Err(UsageError::new(
                    UsageErrorKind::NotUtf8 {
                        lossy: raw.to_string_lossy().into_owned(),
                    },
                    &[],
                ))
            }
        }
    }
    let mut parser = Parser {
        root,
        chain: vec![root],
        path: Vec::new(),
        entries: Vec::new(),
        positionals: Vec::new(),
        help: false,
        version: false,
        error: None,
    };
    parser.scan(&tokens);
    parser.finish(env)
}

struct Parser<'a> {
    root: &'a Command,
    chain: Vec<&'a Command>,
    path: Vec<&'static str>,
    entries: Vec<Entry>,
    positionals: Vec<String>,
    help: bool,
    version: bool,
    /// The first problem found; scanning goes on so that a later `--help`
    /// can still be honoured.
    error: Option<UsageError>,
}

impl<'a> Parser<'a> {
    fn current(&self) -> &'a Command {
        self.chain[self.chain.len() - 1]
    }

    fn fail(&mut self, kind: UsageErrorKind) {
        if self.error.is_none() {
            self.error = Some(UsageError::new(kind, &self.path));
        }
    }

    /// Flags visible at the current command: its own, then the globals.
    fn visible_flags(&self) -> impl Iterator<Item = &'a Flag> + 'a {
        let current = self.current();
        current.flags.iter().chain(&self.root.globals)
    }

    fn find_long(&self, name: &str) -> Option<&'a Flag> {
        self.visible_flags().find(|f| f.long == name)
    }

    fn find_short(&self, short: char) -> Option<&'a Flag> {
        self.visible_flags().find(|f| f.short == Some(short))
    }

    fn scan(&mut self, tokens: &[String]) {
        let mut i = 0;
        let mut only_positional = false;
        while i < tokens.len() {
            let token = tokens[i].as_str();
            i += 1;
            if only_positional {
                self.positional(token);
            } else if token == "--" {
                only_positional = true;
            } else if token == "--help" || token == "-h" {
                self.help = true;
            } else if (token == "--version" || token == "-V") && self.chain.len() == 1 {
                self.version = true;
            } else if let Some(long) = token.strip_prefix("--") {
                i += self.long_flag(long, tokens.get(i).map(String::as_str));
            } else if token.len() > 1 && token.starts_with('-') && !is_number(token) {
                i += self.short_flags(&token[1..], tokens.get(i).map(String::as_str));
            } else {
                self.positional(token);
            }
        }
    }

    /// Handles `--name[=value]`; returns how many following tokens it used.
    fn long_flag(&mut self, text: &str, next: Option<&str>) -> usize {
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (text, None),
        };
        let Some(flag) = self.find_long(name) else {
            let candidates: Vec<&str> = self
                .visible_flags()
                .map(|f| f.long)
                .chain(["help"])
                .collect();
            let suggestion = suggest(name, candidates).map(|s| format!("--{s}"));
            self.fail(UsageErrorKind::UnknownFlag {
                flag: format!("--{name}"),
                suggestion,
            });
            return 0;
        };
        match (flag.takes_value(), inline) {
            (true, Some(value)) => {
                self.record(flag, Some(value.to_owned()));
                0
            }
            (true, None) => self.take_value(flag, next),
            (false, None) => {
                self.record(flag, None);
                0
            }
            (false, Some(value)) => {
                self.fail(UsageErrorKind::UnexpectedValue {
                    flag: flag.display_name(),
                    value: value.to_owned(),
                });
                0
            }
        }
    }

    /// Handles `-abc`, `-n5` and `-n 5`; returns how many following tokens
    /// it used.
    fn short_flags(&mut self, cluster: &str, next: Option<&str>) -> usize {
        for (at, short) in cluster.char_indices() {
            let Some(flag) = self.find_short(short) else {
                self.fail(UsageErrorKind::UnknownFlag {
                    flag: format!("-{short}"),
                    suggestion: None,
                });
                return 0;
            };
            if flag.takes_value() {
                let rest = &cluster[at + short.len_utf8()..];
                if rest.is_empty() {
                    return self.take_value(flag, next);
                }
                let value = rest.strip_prefix('=').unwrap_or(rest);
                self.record(flag, Some(value.to_owned()));
                return 0;
            }
            self.record(flag, None);
        }
        0
    }

    /// Takes the next token as `flag`'s value unless it is clearly another
    /// flag, which means the value was forgotten.
    fn take_value(&mut self, flag: &'a Flag, next: Option<&str>) -> usize {
        let forgotten = match next {
            None => true,
            Some(next) => next.starts_with('-') && next != "-",
        };
        match next {
            Some(value) if !forgotten => {
                self.record(flag, Some(value.to_owned()));
                1
            }
            _ => {
                self.fail(UsageErrorKind::MissingValue {
                    flag: flag.display_name(),
                    value_name: flag.value.unwrap_or("VALUE").to_owned(),
                });
                0
            }
        }
    }

    fn record(&mut self, flag: &Flag, value: Option<String>) {
        self.entries.push(Entry {
            name: flag.long,
            source: Source::Flag,
            value,
        });
    }

    fn positional(&mut self, token: &str) {
        let current = self.current();
        if !current.is_group() {
            self.positionals.push(token.to_owned());
            return;
        }
        if token == "help" && current.find("help").is_none() {
            // `belay help job list`: the rest names the command to explain.
            self.help = true;
            return;
        }
        match current.find(token) {
            Some(sub) => {
                self.chain.push(sub);
                self.path.push(sub.name);
            }
            None => {
                let names = current
                    .subcommands
                    .iter()
                    .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()));
                let suggestion = suggest(token, names).map(str::to_owned);
                self.fail(UsageErrorKind::UnknownCommand {
                    name: token.to_owned(),
                    suggestion,
                });
            }
        }
    }

    fn finish(mut self, env: &dyn Fn(&str) -> Option<String>) -> Result<Parsed, UsageError> {
        if self.help {
            return Ok(Parsed::Help(self.path));
        }
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        if self.version {
            return Ok(Parsed::Version);
        }
        let current = self.current();
        if current.is_group() {
            if self.chain.len() == 1 {
                return Ok(Parsed::Help(self.path));
            }
            let available = current
                .subcommands
                .iter()
                .map(|c| c.name.to_owned())
                .collect();
            return Err(self.error(UsageErrorKind::MissingCommand { available }));
        }
        self.assign_positionals()?;
        self.check_repeats_and_conflicts()?;
        self.fill_fallbacks(env);
        Ok(Parsed::Run(Matches {
            path: self.path,
            entries: self.entries,
        }))
    }

    fn error(&self, kind: UsageErrorKind) -> UsageError {
        UsageError::new(kind, &self.path)
    }

    fn assign_positionals(&mut self) -> Result<(), UsageError> {
        let current = self.current();
        let mut values = std::mem::take(&mut self.positionals).into_iter();
        for arg in &current.args {
            if arg.many {
                let rest: Vec<String> = values.by_ref().collect();
                if rest.is_empty() && arg.required {
                    return Err(self.error(UsageErrorKind::MissingArgument {
                        name: arg.display_name(),
                    }));
                }
                for value in rest {
                    self.push_arg(arg.name, value);
                }
                continue;
            }
            match values.next() {
                Some(value) => self.push_arg(arg.name, value),
                None if arg.required => {
                    return Err(self.error(UsageErrorKind::MissingArgument {
                        name: arg.display_name(),
                    }))
                }
                None => {}
            }
        }
        match values.next() {
            Some(value) => Err(self.error(UsageErrorKind::UnexpectedArgument { value })),
            None => Ok(()),
        }
    }

    fn push_arg(&mut self, name: &'static str, value: String) {
        self.entries.push(Entry {
            name,
            source: Source::Arg,
            value: Some(value),
        });
    }

    fn check_repeats_and_conflicts(&self) -> Result<(), UsageError> {
        let given = |name: &str| {
            self.entries
                .iter()
                .filter(|e| e.name == name && e.source == Source::Flag)
                .count()
        };
        for flag in self.visible_flags() {
            if !flag.repeated && given(flag.long) > 1 {
                return Err(self.error(UsageErrorKind::RepeatedFlag {
                    flag: flag.display_name(),
                }));
            }
            for other in &flag.conflicts {
                if given(flag.long) > 0 && given(other) > 0 {
                    return Err(self.error(UsageErrorKind::Conflict {
                        first: flag.display_name(),
                        second: format!("--{other}"),
                    }));
                }
            }
        }
        Ok(())
    }

    fn fill_fallbacks(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let flags: Vec<&Flag> = self.visible_flags().filter(|f| f.takes_value()).collect();
        for flag in flags {
            if let Some(var) = flag.env {
                if let Some(value) = env(var).filter(|v| !v.trim().is_empty()) {
                    self.entries.push(Entry {
                        name: flag.long,
                        source: Source::Env(var),
                        value: Some(value),
                    });
                }
            }
            if let Some(default) = flag.default {
                self.entries.push(Entry {
                    name: flag.long,
                    source: Source::Default,
                    value: Some(default.to_owned()),
                });
            }
        }
    }
}
