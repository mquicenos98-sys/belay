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
