//! `belay server` and `belay dev`: running the engine and its HTTP API in
//! this process.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use belay_common::log::{Filter, LogFormat};
use belay_core::config::Config;
use belay_core::Engine;
use belay_json::json;
use belay_proto::{QueueName, WorkerId};
use belay_worker::WorkerConfig;

use super::{config, worker, Ctx};
use crate::args::{Command, Flag, Matches};
use crate::error::Result;
use crate::runner::service_log_level;

/// How often `dev` checks whether a shutdown was requested, so it can stop
/// its worker before the engine goes away.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

pub(crate) fn server_spec() -> Command {
    Command::new("server", "run the belay server from a configuration file")
        .details(
            "Loads the configuration (--config, else BELAY_CONFIG, else built-in defaults)\n\
             with BELAY_<SECTION>__<KEY> environment overrides, starts the engine and\n\
             serves the HTTP API until `belay admin shutdown` drains it. Ctrl-C ends the\n\
             process at once, which is safe: the store is crash-consistent and leased\n\
             jobs return to their queues when their leases expire.",
        )
        .flag(
            Flag::option("config", "PATH", "Configuration file")
                .short('c')
                .env(config::ENV_CONFIG),
        )
        .example("belay server --config /etc/belay/belay.toml")
        .example("BELAY_SERVER__LISTEN=0.0.0.0:7700 belay server -c belay.toml")
}
