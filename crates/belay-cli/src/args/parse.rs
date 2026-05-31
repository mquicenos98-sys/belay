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
            Some(next) => {
                next.starts_with("--")
                    || next
                        .strip_prefix('-')
                        .and_then(|rest| rest.chars().next())
                        .is_some_and(|c| !c.is_ascii_digit() && self.find_short(c).is_some())
            }
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

/// `-5`, `-1.5`: negative numbers, which are values rather than flags.
fn is_number(token: &str) -> bool {
    token.len() > 1
        && token[1..].parse::<f64>().is_ok()
        && token[1..].starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Arg;

    fn tree() -> Command {
        Command::new("belay", "test tree")
            .global(Flag::switch("json", "JSON output"))
            .global(Flag::switch("verbose", "More logs").short('v').repeated())
            .global(Flag::switch("quiet", "Less output").short('q'))
            .global(
                Flag::option("url", "URL", "Server")
                    .env("BELAY_URL")
                    .default("http://127.0.0.1:7700"),
            )
            .subcommand(
                Command::new("enqueue", "Enqueue")
                    .arg(Arg::required("queue", "Queue"))
                    .arg(Arg::required("kind", "Kind"))
                    .flag(Flag::option("priority", "N", "Priority").short('p'))
                    .flag(Flag::option("payload", "JSON", "Payload").short('d'))
                    .flag(Flag::option("tag", "TAG", "Tag").short('t').repeated())
                    .flag(Flag::option("delay", "DURATION", "Delay").conflicts_with("run-at"))
                    .flag(Flag::option("run-at", "TIME", "When"))
                    .flag(Flag::switch("wait", "Wait").short('w')),
            )
            .subcommand(
                Command::new("job", "Jobs")
                    .subcommand(Command::new("get", "Get").arg(Arg::required("id", "Job")))
                    .subcommand(
                        Command::new("list", "List")
                            .alias("ls")
                            .flag(Flag::option("state", "STATE", "State").repeated()),
                    )
                    .subcommand(
                        Command::new("cancel", "Cancel").arg(Arg::required("id", "Jobs").many()),
                    ),
            )
            .subcommand(
                Command::new("config", "Config")
                    .subcommand(Command::new("check", "Check").arg(Arg::optional("path", "File"))),
            )
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn run(args: &[&str]) -> Matches {
        match parse(&tree(), args, &no_env) {
            Ok(Parsed::Run(m)) => m,
            other => panic!("expected a command for {args:?}, got {other:?}"),
        }
    }

    fn fail(args: &[&str]) -> UsageError {
        match parse(&tree(), args, &no_env) {
            Err(e) => e,
            other => panic!("expected an error for {args:?}, got {other:?}"),
        }
    }

    #[test]
    fn the_tree_is_consistent() {
        assert_eq!(tree().lint(), Vec::<String>::new());
    }

    #[test]
    fn long_flags_take_values_inline_or_from_the_next_token() {
        let m = run(&[
            "enqueue",
            "emails",
            "send",
            "--priority=5",
            "--payload",
            "{}",
        ]);
        assert_eq!(m.path(), ["enqueue"]);
        assert_eq!(m.arg("queue"), Some("emails"));
        assert_eq!(m.arg("kind"), Some("send"));
        assert_eq!(m.get::<i16>("priority").unwrap(), Some(5));
        assert_eq!(m.value("payload"), Some("{}"));
    }

    #[test]
    fn short_flags_cluster_and_attach_values() {
        let m = run(&["enqueue", "-qvvw", "-p7", "-t", "a", "-t=b", "q", "k"]);
        assert!(m.flag("quiet") && m.flag("wait"));
        assert_eq!(m.count("verbose"), 2);
        assert_eq!(m.get::<i16>("priority").unwrap(), Some(7));
        assert_eq!(m.values("tag"), ["a", "b"]);
    }

    #[test]
    fn negative_numbers_are_values_not_flags() {
        let m = run(&["enqueue", "q", "k", "--priority", "-5"]);
        assert_eq!(m.get::<i16>("priority").unwrap(), Some(-5));
        let m = run(&["enqueue", "q", "k", "-p", "-10"]);
        assert_eq!(m.get::<i16>("priority").unwrap(), Some(-10));
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        let m = run(&["enqueue", "--", "-q", "--wait"]);
        assert_eq!(m.arg("queue"), Some("-q"));
        assert_eq!(m.arg("kind"), Some("--wait"));
        assert!(!m.flag("quiet"));
    }

    #[test]
    fn a_lone_dash_is_a_positional_value() {
        let m = run(&["enqueue", "q", "k", "--payload", "-"]);
        assert_eq!(m.value("payload"), Some("-"));
        let m = run(&["config", "check", "-"]);
        assert_eq!(m.arg("path"), Some("-"));
    }

    #[test]
    fn global_flags_work_before_and_after_subcommands() {
        let m = run(&["--json", "job", "-q", "get", "01ABC", "--url", "http://h:1"]);
        assert_eq!(m.path(), ["job", "get"]);
        assert!(m.flag("json") && m.flag("quiet"));
        assert_eq!(m.value("url"), Some("http://h:1"));
    }

    #[test]
    fn aliases_resolve_to_the_real_name() {
        assert_eq!(run(&["job", "ls"]).path(), ["job", "list"]);
    }

    #[test]
    fn many_arguments_collect_the_rest() {
        let m = run(&["job", "cancel", "a", "b", "c"]);
        assert_eq!(m.values("id"), ["a", "b", "c"]);
        assert!(matches!(
            fail(&["job", "cancel"]).kind(),
            UsageErrorKind::MissingArgument { name } if name == "<ID>..."
        ));
    }

    #[test]
    fn environment_and_defaults_fill_absent_options() {
        let env = |var: &str| (var == "BELAY_URL").then(|| "http://env:1".to_owned());
        let Parsed::Run(m) = parse(&tree(), ["job", "ls"], &env).unwrap() else {
            panic!("expected a command");
        };
        assert_eq!(m.value("url"), Some("http://env:1"));
        assert_eq!(
            run(&["job", "ls"]).value("url"),
            Some("http://127.0.0.1:7700")
        );
        let blank = |_: &str| Some("  ".to_owned());
        let Parsed::Run(m) = parse(&tree(), ["job", "ls"], &blank).unwrap() else {
            panic!("expected a command");
        };
        assert_eq!(m.value("url"), Some("http://127.0.0.1:7700"));
    }

    #[test]
    fn help_is_found_anywhere_and_beats_errors() {
        let help = |args: &[&str]| match parse(&tree(), args, &no_env) {
            Ok(Parsed::Help(path)) => path,
            other => panic!("expected help for {args:?}, got {other:?}"),
        };
        assert!(help(&[]).is_empty());
        assert_eq!(help(&["job", "--help"]), ["job"]);
        assert_eq!(help(&["enqueue", "--prio", "x", "-h"]), ["enqueue"]);
        assert_eq!(help(&["help", "job", "list"]), ["job", "list"]);
        assert_eq!(help(&["job", "help", "cancel"]), ["job", "cancel"]);
    }

    #[test]
    fn version_is_a_root_flag() {
        assert!(matches!(
            parse(&tree(), ["--version"], &no_env),
            Ok(Parsed::Version)
        ));
        assert!(matches!(
            fail(&["job", "list", "-V"]).kind(),
            UsageErrorKind::UnknownFlag { flag, .. } if flag == "-V"
        ));
    }

    #[test]
    fn unknown_commands_and_flags_get_suggestions() {
        let e = fail(&["jbo", "list"]);
        assert_eq!(e.to_string(), r#"unknown command "jbo" for "belay""#);
        assert_eq!(e.hint().as_deref(), Some(r#"did you mean "job"?"#));
        let e = fail(&["job", "lsit"]);
        assert_eq!(e.hint().as_deref(), Some(r#"did you mean "list"?"#));
        assert_eq!(e.path(), ["job"]);
        let e = fail(&["enqueue", "q", "k", "--prioriy", "1"]);
        assert_eq!(
            e.to_string(),
            r#"unknown option --prioriy for "belay enqueue""#
        );
        assert_eq!(e.hint().as_deref(), Some("did you mean --priority?"));
        let e = fail(&["enqueue", "q", "k", "-x"]);
        assert_eq!(e.to_string(), r#"unknown option -x for "belay enqueue""#);
    }

    #[test]
    fn missing_values_are_reported_instead_of_swallowing_flags() {
        for args in [
            &["enqueue", "q", "k", "--payload"][..],
            &["enqueue", "q", "k", "--payload", "--wait"],
            &["enqueue", "q", "k", "--payload", "-w"],
        ] {
            assert_eq!(
                fail(args).to_string(),
                "option --payload needs a value (--payload <JSON>)"
            );
        }
    }

    #[test]
    fn switches_reject_inline_values() {
        assert_eq!(
            fail(&["enqueue", "q", "k", "--wait=yes"]).to_string(),
            r#"option --wait does not take a value, got "yes""#
        );
    }

    #[test]
    fn arity_is_checked() {
        assert_eq!(
            fail(&["enqueue", "q"]).to_string(),
            r#"missing argument <KIND> for "belay enqueue""#
        );
        assert_eq!(
            fail(&["enqueue", "q", "k", "extra"]).to_string(),
            r#"unexpected argument "extra" for "belay enqueue""#
        );
        assert_eq!(run(&["config", "check"]).arg("path"), None);
    }

    #[test]
    fn groups_need_a_subcommand() {
        let e = fail(&["job"]);
        assert_eq!(e.to_string(), r#""belay job" needs a command"#);
        assert_eq!(
            e.hint().as_deref(),
            Some("available commands: get, list, cancel")
        );
    }

    #[test]
    fn single_flags_may_not_repeat_and_conflicts_are_enforced() {
        assert_eq!(
            fail(&["enqueue", "q", "k", "-p", "1", "-p", "2"]).to_string(),
            "option --priority was given more than once"
        );
        assert_eq!(
            fail(&["enqueue", "q", "k", "--delay", "1s", "--run-at", "x"]).to_string(),
            "--delay cannot be used together with --run-at"
        );
        let m = run(&["job", "list", "--state", "dead", "--state", "ready"]);
        assert_eq!(m.values("state"), ["dead", "ready"]);
    }

    #[test]
    fn non_utf8_arguments_are_rejected() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = OsString::from_vec(vec![b'j', 0xff]);
            let e = parse(&tree(), [bad], &no_env).unwrap_err();
            assert!(matches!(e.kind(), UsageErrorKind::NotUtf8 { .. }));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let bad = OsString::from_wide(&[0xD800]);
            let e = parse(&tree(), [bad], &no_env).unwrap_err();
            assert!(matches!(e.kind(), UsageErrorKind::NotUtf8 { .. }));
        }
    }

    #[test]
    fn numbers_are_recognised_conservatively() {
        assert!(is_number("-5"));
        assert!(is_number("-0.5"));
        assert!(!is_number("-inf"));
        assert!(!is_number("-v"));
        assert!(!is_number("-"));
    }
}
