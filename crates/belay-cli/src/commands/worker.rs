//! `belay worker`: a worker process that leases jobs from a server over
//! HTTP and runs them with the built-in handlers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_common::log::{Filter, LogFormat};
use belay_json::json;
use belay_proto::{JobSource, QueueName, WorkerId};
use belay_worker::builtin::{self, ShellHandler};
use belay_worker::{Registry, Worker, WorkerConfig, WorkerHandle, WorkerStats};

use super::Ctx;
use crate::args::{Command, Flag, Matches};
use crate::error::Result;
use crate::runner::service_log_level;

/// How often `--burst` and `--max-jobs` look at the worker's counters.
const WATCH_EVERY: Duration = Duration::from_millis(20);

pub(crate) fn spec() -> Command {
    Command::new("worker", "run a worker that leases jobs from the server")
        .details(
            "Runs the built-in handlers: echo (returns its payload), sleep and fail, plus\n\
             shell with --shell or --allow. Shell jobs run the program in their payload\n\
             ({\"cmd\": [\"prog\", \"arg\"]}), so enable it deliberately and prefer --allow lists.\n\
             \n\
             Without --burst or --max-jobs the worker runs until the process is stopped;\n\
             jobs it was running then come back after their lease expires.",
        )
        .flag(
            Flag::option("queues", "LIST", "Queues to serve (comma-separated)")
                .repeated()
                .default("default"),
        )
        .flag(
            Flag::option("concurrency", "N", "Jobs run at once")
                .short('c')
                .default("4"),
        )
        .flag(Flag::option("id", "ID", "Worker id (default: host:pid)"))
        .flag(Flag::switch("shell", "Also run shell jobs, any program"))
        .flag(Flag::option("allow", "PROGRAM", "Run shell jobs, only for this program").repeated())
        .flag(Flag::option("lease-for", "DURATION", "Lease length; heartbeats renew it").default("30s"))
        .flag(Flag::option("poll-wait", "DURATION", "Long-poll wait when idle").default("10s"))
        .flag(Flag::switch("burst", "Exit once the queues stay empty for one poll wait"))
        .flag(Flag::option("max-jobs", "N", "Exit after this many jobs finished"))
        .example("belay worker --queues emails,reports --concurrency 8")
        .example("belay worker --queues media --allow convert --allow ffmpeg")
        .example("belay worker --queues default --burst --poll-wait 1s")
}

/// The handlers a worker runs: the harmless built-ins, plus the shell
/// handler when asked for (`allow` empty means any program).
pub(crate) fn registry(shell: bool, allow: Vec<String>) -> Registry {
    let registry = builtin::defaults();
    if !shell && allow.is_empty() {
        return registry;
    }
    let handler = if allow.is_empty() {
        ShellHandler::new()
    } else {
        ShellHandler::new().allow(allow)
    };
    registry.register(builtin::SHELL, handler)
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let queues: Vec<QueueName> = m.get_list("queues")?;
    if queues.is_empty() {
        return Err(m.constraint("--queues needs at least one queue").into());
    }
    let mut config = WorkerConfig::new(belay_worker::default_worker_id(), queues)
        .concurrency(m.get_or("concurrency", 4)?)
        .lease_for(m.require("lease-for")?)
        .poll_wait(m.require("poll-wait")?);
    if let Some(id) = m.get::<WorkerId>("id")? {
        config = config.id(id);
    }
    let max_jobs = m.get::<u64>("max-jobs")?;
    let allow: Vec<String> = m.get_all("allow")?;
    if m.flag("shell") && allow.is_empty() {
        ctx.out.warn(
            "shell jobs can run any program for anyone who can enqueue; limit them with --allow",
        )?;
    }
    let registry = registry(m.flag("shell"), allow);
    let kinds: Vec<String> = registry.kinds().map(str::to_owned).collect();

    belay_common::log::init_stderr(
        Filter::new(service_log_level(ctx.verbose, ctx.out.quiet)),
        LogFormat::Logfmt,
    );
    let source: Arc<dyn JobSource> = Arc::new(ctx.client()?);
    let poll_wait = config.poll_wait;
    let worker = Worker::new(source, registry, config)?;
    let (id, queues, slots) = (
        worker.id().clone(),
        worker.config().queues.clone(),
        worker.config().concurrency,
    );
    let handle = worker.spawn()?;
    if !ctx.out.json {
        let queues: Vec<&str> = queues.iter().map(QueueName::as_str).collect();
        ctx.out.done(format!(
            "worker {id} serving {} from {} with {slots} slots; kinds: {}",
            queues.join(", "),
            ctx.url,
            kinds.join(", ")
        ))?;
        ctx.out.flush()?;
    }

    let stats = if m.flag("burst") || max_jobs.is_some() {
        let limit = max_jobs.unwrap_or(u64::MAX);
        let idle_limit = m.flag("burst").then_some(poll_wait);
        watch(&handle, limit, idle_limit);
        handle.stop().stats
    } else {
        handle.join().stats
    };
    summary(ctx, &id, &stats)
}

/// Blocks until `limit` jobs have finished or, with `idle_limit`, until the
/// worker has held no job for that long, i.e. a whole long poll came back
/// empty.
fn watch(handle: &WorkerHandle, limit: u64, idle_limit: Option<Duration>) {
    let mut idle_since = Instant::now();
    let mut seen = handle.stats().leased;
    loop {
        std::thread::sleep(WATCH_EVERY);
        let stats = handle.stats();
        if stats.finished() >= limit || handle.is_finished() {
            return;
        }
        if stats.leased != seen || stats.running > 0 {
            seen = stats.leased;
            idle_since = Instant::now();
            continue;
        }
        if idle_limit.is_some_and(|limit| idle_since.elapsed() >= limit + WATCH_EVERY) {
            return;
        }
    }
}

fn summary(ctx: &mut Ctx<'_>, id: &WorkerId, stats: &WorkerStats) -> Result<()> {
    if ctx.out.json {
        ctx.out.json_value(&json!({
            "worker": id.to_string(),
            "leased": stats.leased,
            "succeeded": stats.succeeded,
            "failed": stats.failed,
            "retried": stats.retried,
            "abandoned": stats.abandoned,
            "handed_back": stats.handed_back,
        }))?;
        return Ok(());
    }
    ctx.out.done(format!(
        "worker {id} stopped: {} succeeded, {} failed, {} abandoned",
        stats.succeeded, stats.failed, stats.abandoned
    ))?;
    if let Some(error) = &stats.last_error {
        ctx.out.note(format!("last error: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_is_only_registered_on_request() {
        let kinds = |r: Registry| r.kinds().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            kinds(registry(false, Vec::new())),
            ["echo", "fail", "sleep"]
        );
        assert_eq!(
            kinds(registry(true, Vec::new())),
            ["echo", "fail", "shell", "sleep"]
        );
        assert!(registry(false, vec!["convert".into()]).contains("shell"));
    }
}
