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

pub(crate) fn dev(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let mut config = Config::dev();
    config.server.listen = m.require("listen")?;
    if !config.server.is_loopback() {
        return Err(m
            .constraint(format!(
                "dev serves without authentication, so it only listens on loopback addresses, \
                 not {}",
                config.server.listen
            ))
            .with_hint("use `belay server` with [[auth.tokens]] to serve other hosts")
            .into());
    }
    if let Some(dir) = m.get::<PathBuf>("data")? {
        config.storage.in_memory = false;
        config.storage.dir = dir;
    }
    config.server.shutdown_grace = Duration::from_secs(5);
    let slots: usize = m.get_or("workers", 4)?;
    let queues: Vec<QueueName> = m.get_list("queues")?;
    if queues.is_empty() && !m.flag("no-worker") {
        return Err(m.constraint("--queues needs at least one queue").into());
    }
    let storage = if config.storage.in_memory {
        "memory (lost on exit)".to_owned()
    } else {
        format!("{} (kept)", config.storage.dir.display())
    };
    belay_common::log::init_stderr(
        Filter::new(service_log_level(ctx.verbose, ctx.out.quiet)),
        LogFormat::Logfmt,
    );

    let engine = Arc::new(Engine::start(config)?);
    let handle = belay_server::serve(Arc::clone(&engine))?;
    let url = format!("http://{}", handle.local_addr());
    let worker = if m.flag("no-worker") {
        None
    } else {
        let registry = worker::registry(true, m.get_all("allow")?);
        let worker_config = WorkerConfig::new(WorkerId::from_static("dev-worker"), queues.clone())
            .concurrency(slots)
            .poll_wait(Duration::from_secs(1))
            .shutdown_grace(Duration::from_secs(5));
        let kinds: Vec<String> = registry.kinds().map(str::to_owned).collect();
        let started = engine.embedded_worker(registry, worker_config)?;
        Some((started, kinds))
    };

    if ctx.out.json {
        ctx.out
            .json_line(&json!({"url": url.as_str(), "node": engine.node_id()}))?;
    } else if !ctx.out.quiet {
        let worker_line = match &worker {
            Some((_, kinds)) => {
                let names: Vec<&str> = queues.iter().map(QueueName::as_str).collect();
                format!(
                    "{slots} slots on {}; kinds {}",
                    names.join(", "),
                    kinds.join(", ")
                )
            }
            None => "off".to_owned(),
        };
        let url_flag = if url == belay_client::DEFAULT_URL {
            String::new()
        } else {
            format!("--url {url} ")
        };
        let first_queue = queues.first().map_or("default", QueueName::as_str);
        ctx.out.text(&format!(
            "belay {} dev server\n  api      {url}\n  storage  {storage}\n  worker   {worker_line}\n\
             try:  belay {url_flag}enqueue {first_queue} echo --payload '{{\"hello\": \"world\"}}' --wait\n\
             stop: belay {url_flag}admin shutdown (or Ctrl-C)\n",
            env!("CARGO_PKG_VERSION"),
        ))?;
    } else {
        ctx.out.line(&url)?;
    }
    ctx.out.flush()?;
    drop(engine);

    // Stop the worker as soon as the drain starts, while the engine still
    // takes its results; the server stops the engine once it has drained.
    while !handle.is_shutting_down() {
        std::thread::sleep(SHUTDOWN_POLL);
    }
    if let Some((worker, _)) = worker {
        let report = worker.stop();
        belay_common::info!("dev worker stopped"; stats = %report.stats);
    }
    handle.wait();
    ctx.out.done("dev server stopped")?;
    Ok(())
}
