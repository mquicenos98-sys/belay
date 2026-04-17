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

pub(crate) fn dev_spec() -> Command {
    Command::new("dev", "run a development server with a built-in worker")
        .details(
            "Serves the API without authentication on a loopback address, keeps data in\n\
             memory unless --data names a directory, and runs a worker in the same process\n\
             with the echo, sleep, fail and shell handlers. Stop it with `belay admin\n\
             shutdown` or Ctrl-C.",
        )
        .flag(Flag::option("listen", "ADDR", "Address to serve on").default("127.0.0.1:7700"))
        .flag(
            Flag::option("data", "DIR", "Keep data in this directory").conflicts_with("in-memory"),
        )
        .flag(Flag::switch(
            "in-memory",
            "Keep data in memory, lost on exit (the default)",
        ))
        .flag(Flag::option("workers", "N", "Slots of the built-in worker").default("4"))
        .flag(
            Flag::option("queues", "LIST", "Queues the built-in worker serves")
                .repeated()
                .default("default"),
        )
        .flag(Flag::option("allow", "PROGRAM", "Limit shell jobs to these programs").repeated())
        .flag(Flag::switch("no-worker", "Do not run the built-in worker"))
        .example("belay dev")
        .example("belay dev --listen 127.0.0.1:0 --data ./dev-data --queues default,emails")
}

pub(crate) fn server(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let (name, config) = config::load(ctx, m.value("config"))?;
    for warning in config.warnings() {
        ctx.out.err_text(&format!("{warning}\n"))?;
    }
    // -v and -q override the configured level; the format stays.
    if ctx.verbose > 0 || ctx.out.quiet {
        belay_common::log::init_stderr(
            Filter::new(service_log_level(ctx.verbose, ctx.out.quiet)),
            config.log.format,
        );
    } else {
        config.log.install();
    }
    let storage = if config.storage.in_memory {
        "memory".to_owned()
    } else {
        config.storage.dir.display().to_string()
    };
    let engine = Arc::new(Engine::start(config)?);
    let handle = belay_server::serve(Arc::clone(&engine))?;
    let url = format!("http://{}", handle.local_addr());
    if ctx.out.json {
        ctx.out.json_line(&json!({
            "url": url.as_str(),
            "node": engine.node_id(),
            "config": name.as_str(),
        }))?;
    } else if ctx.out.quiet {
        ctx.out.line(&url)?;
    } else {
        ctx.out.done(format!(
            "belay {} serving {url} (node {}, storage {storage}, config {name})",
            env!("CARGO_PKG_VERSION"),
            engine.node_id()
        ))?;
    }
    ctx.out.flush()?;
    drop(engine);
    handle.wait();
    ctx.out.done("server stopped")?;
    Ok(())
}
