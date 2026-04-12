//! The command tree and what each command does.
//!
//! [`tree`] declares every command, flag and argument; [`dispatch`] maps
//! the parsed path to the function that runs it. Each submodule owns one
//! area: its spec builder and its implementations.

pub(crate) mod admin;
pub(crate) mod calendars;
pub(crate) mod config;
pub(crate) mod cron;
pub(crate) mod events;
pub(crate) mod jobs;
pub(crate) mod queues;
pub(crate) mod schedules;
pub(crate) mod server;
pub(crate) mod system;
pub(crate) mod worker;
pub(crate) mod workflows;

use std::time::Duration;

use belay_client::{Client, ClientConfig};
use belay_common::{SharedClock, Timestamp};

use crate::args::{Command, Flag, Matches, UsageError};
use crate::error::Result;
use crate::output::Output;
use crate::runner::Runner;

/// What every command gets: the output streams, the connection settings
/// and access to the environment of this invocation.
pub(crate) struct Ctx<'a> {
    pub(crate) out: Output<'a>,
    /// `--url`, `BELAY_URL` or the default.
    pub(crate) url: String,
    /// `--token` or `BELAY_TOKEN`.
    pub(crate) token: Option<String>,
    /// `-v` count.
    pub(crate) verbose: usize,
    pub(crate) clock: SharedClock,
    pub(crate) runner: &'a Runner,
    path: Vec<&'static str>,
    client: Option<Client>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        out: Output<'a>,
        runner: &'a Runner,
        clock: SharedClock,
        m: &Matches,
    ) -> Ctx<'a> {
        Ctx {
            out,
            url: m
                .value("url")
                .unwrap_or(belay_client::DEFAULT_URL)
                .to_owned(),
            token: m.value("token").map(|t| t.trim().to_owned()),
            verbose: m.count("verbose"),
            clock,
            runner,
            path: m.path().to_vec(),
            client: None,
        }
    }

    /// The API client, built on first use.
    pub(crate) fn client(&mut self) -> Result<Client> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let mut config = ClientConfig::new(self.url.clone())
            .user_agent(concat!("belay-cli/", env!("CARGO_PKG_VERSION")));
        if let Some(token) = &self.token {
            config = config.token(token.clone());
        }
        let client = Client::new(config)?;
        self.client = Some(client.clone());
        Ok(client)
    }

    /// The current time, from the invocation's clock.
    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    pub(crate) fn read_stdin(&self) -> std::io::Result<Vec<u8>> {
        self.runner.read_stdin()
    }

    /// A usage error about the running command.
    pub(crate) fn usage(&self, message: impl Into<String>) -> UsageError {
        UsageError::constraint(message, &self.path)
    }

    /// Sleeps between polls.
    pub(crate) fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Runs `op` for each item of a multi-item command (`job cancel A B C`).
///
/// With one item, its error is the command's error. With several, every
/// item is attempted: failures are reported as they happen, and the
/// command then fails with a summary and the status of the first failure.
/// `op` prints its own confirmation and returns the JSON shown for the item
/// with `--json` (one object for one item, an array otherwise).
pub(crate) fn for_each<T, F>(
    ctx: &mut Ctx<'_>,
    items: &[T],
    noun: &str,
    verb: &str,
    mut op: F,
) -> Result<()>
where
    T: std::fmt::Display,
    F: FnMut(&mut Ctx<'_>, &T) -> Result<belay_json::Value>,
{
    let mut values = Vec::with_capacity(items.len());
    let mut failed = 0;
    let mut first_exit = None;
    for item in items {
        match op(ctx, item) {
            Ok(value) => values.push(value),
            Err(e) if items.len() == 1 => return Err(e),
            Err(e) => {
                ctx.out.error(&format!("{item}: {e}"), None)?;
                failed += 1;
                first_exit.get_or_insert(e.exit());
            }
        }
    }
    if ctx.out.json {
        match values.len() {
            1 if items.len() == 1 => ctx.out.json_value(&values[0])?,
            _ => ctx.out.json_value(&belay_json::Value::Array(values))?,
        }
    }
    match first_exit {
        None => Ok(()),
        Some(exit) => Err(crate::error::Error::Partial {
            message: format!("{failed} of {} {noun} could not be {verb}", items.len()),
            exit,
        }),
    }
}

/// The whole command tree.
#[must_use]
pub fn tree() -> Command {
    Command::new("belay", "durable job queue and cron scheduler")
        .details(
            "Talks to a belay server over HTTP (--url, BELAY_URL) or runs one (server, dev).\n\
             \n\
             Exit status: 0 success, 1 failure, 2 usage error, 3 not found,\n\
             4 server unreachable or unavailable.",
        )
        .global(
            Flag::option("url", "URL", "Server address")
                .env("BELAY_URL")
                .default(belay_client::DEFAULT_URL),
        )
        .global(Flag::option("token", "TOKEN", "Bearer token").env("BELAY_TOKEN"))
        .global(Flag::switch("json", "Print JSON instead of tables"))
        .global(Flag::switch("quiet", "Print only identifiers, no confirmations").short('q'))
        .global(Flag::switch(
            "no-color",
            "Never color output (also NO_COLOR)",
        ))
        .global(
            Flag::switch("verbose", "Log more; repeat for even more (-vv)")
                .short('v')
                .repeated(),
        )
        .subcommand(server::server_spec())
        .subcommand(server::dev_spec())
        .subcommand(jobs::enqueue_spec())
        .subcommand(jobs::spec())
        .subcommand(queues::spec())
        .subcommand(schedules::spec())
        .subcommand(calendars::spec())
        .subcommand(workflows::spec())
        .subcommand(worker::spec())
        .subcommand(system::workers_spec())
        .subcommand(events::spec())
        .subcommand(cron::spec())
        .subcommand(config::spec())
        .subcommand(admin::spec())
        .subcommand(system::members_spec())
        .subcommand(system::info_spec())
        .subcommand(system::version_spec())
}

/// Runs the command `m` selects.
pub(crate) fn dispatch(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    match m.path() {
        ["server"] => server::server(ctx, m),
        ["dev"] => server::dev(ctx, m),
        ["enqueue"] => jobs::enqueue(ctx, m),
        ["job", action] => jobs::run(ctx, m, action),
        ["queue", action] => queues::run(ctx, m, action),
        ["schedule", action] => schedules::run(ctx, m, action),
        ["calendar", action] => calendars::run(ctx, m, action),
        ["workflow", action] => workflows::run(ctx, m, action),
        ["worker"] => worker::run(ctx, m),
        ["workers"] => system::workers(ctx),
        ["events", "tail"] => events::tail(ctx, m),
        ["cron", action] => cron::run(ctx, m, action),
        ["config", action] => config::run(ctx, m, action),
        ["admin", action] => admin::run(ctx, m, action),
        ["members"] => system::members(ctx),
        ["info"] => system::info(ctx),
        ["version"] => system::version(ctx),
        other => unreachable!("the parser only yields declared commands, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_tree_is_consistent() {
        assert_eq!(tree().lint(), Vec::<String>::new());
    }

    #[test]
    fn every_leaf_is_dispatched() {
        // Walks the tree and checks each leaf path against the dispatch
        // table's shape, so a new command cannot be declared and forgotten.
        fn leaves(command: &Command, path: Vec<&'static str>, out: &mut Vec<Vec<&'static str>>) {
            if command.is_group() {
                for sub in command.subcommands() {
                    let mut next = path.clone();
                    next.push(sub.name());
                    leaves(sub, next, out);
                }
            } else {
                out.push(path);
            }
        }
        let mut all = Vec::new();
        leaves(&tree(), Vec::new(), &mut all);
        assert!(all.len() > 40, "{}", all.len());
        for path in all {
            let known = matches!(
                path.as_slice(),
                ["server"
                    | "dev"
                    | "enqueue"
                    | "worker"
                    | "workers"
                    | "members"
                    | "info"
                    | "version"]
                    | [
                        "job"
                            | "queue"
                            | "schedule"
                            | "calendar"
                            | "workflow"
                            | "cron"
                            | "config"
                            | "admin",
                        _
                    ]
                    | ["events", "tail"]
            );
            assert!(known, "{path:?} has no dispatch arm");
        }
    }

    #[test]
    fn every_command_has_help_text() {
        fn check(command: &Command) {
            assert!(
                !command.about().is_empty(),
                "{} has no summary",
                command.name()
            );
            for flag in command.flags() {
                assert!(!flag.long().is_empty());
            }
            for sub in command.subcommands() {
                check(sub);
            }
        }
        check(&tree());
    }
}
