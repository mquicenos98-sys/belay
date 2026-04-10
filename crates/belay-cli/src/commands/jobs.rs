//! `belay enqueue` and `belay job ...`: creating, inspecting and managing
//! jobs, and waiting for them to finish.

use std::time::{Duration, Instant};

use belay_client::Client;
use belay_common::{format_duration, BackoffPolicy, Timestamp};
use belay_json::ToJson;
use belay_proto::api::{EnqueueStatus, JobFilter, SortOrder};
use belay_proto::{
    Job, JobId, JobSpec, JobState, OnConflict, QueueName, TenantId, Unique, UniqueScope, Validate,
};

use super::{for_each, Ctx};
use crate::args::{Arg, Command, Flag, FromArg, KeyValue, Matches};
use crate::error::{Error, Result};
use crate::input;
use crate::output::human::{approx, percent, relative, when};
use crate::output::{state_style, Fields, Grid};

/// Longest pause between two polls of a job being waited for.
const MAX_POLL: Duration = Duration::from_millis(500);

pub(crate) fn enqueue_spec() -> Command {
    let command = Command::new("enqueue", "enqueue a job")
        .details(
            "Adds a job to QUEUE for workers that handle KIND. Unless --delay or --run-at\n\
             say otherwise the job is ready at once. With --wait the command polls until\n\
             the job finishes, prints its result and fails (status 1) if it died.",
        )
        .arg(Arg::required("queue", "Queue to put the job on"))
        .arg(Arg::required(
            "kind",
            "Job kind: the handler name workers dispatch on",
        ));
    job_options(command)
        .flag(
            Flag::option("delay", "DURATION", "Run no earlier than this from now")
                .conflicts_with("run-at"),
        )
        .flag(Flag::option(
            "run-at",
            "TIME",
            "Run no earlier than this RFC 3339 time",
        ))
        .flag(Flag::option(
            "unique",
            "KEY[:SCOPE]",
            "Deduplicate on KEY; SCOPE is until_started, until_finished or for:DURATION",
        ))
        .flag(Flag::option(
            "on-conflict",
            "POLICY",
            "When KEY is taken: reject, return_existing or replace",
        ))
        .flag(Flag::switch("wait", "Wait for the job to finish and print its result").short('w'))
        .flag(Flag::option(
            "wait-timeout",
            "DURATION",
            "Stop waiting after this long",
        ))
        .example("belay enqueue emails send_welcome --payload '{\"user\": 42}'")
        .example("belay enqueue reports build --delay 10m --unique daily-report:for:1h")
        .example("belay enqueue default echo -d @payload.json --wait")
}

/// The job-shaping flags shared by `enqueue` and `schedule add`.
pub(crate) fn job_options(command: Command) -> Command {
    command
        .flag(
            Flag::option(
                "payload",
                "JSON",
                "Payload: inline JSON, @FILE, or - for stdin",
            )
            .short('d'),
        )
        .flag(
            Flag::option(
                "priority",
                "N",
                "Priority from -1000 to 1000; higher runs first",
            )
            .short('p'),
        )
        .flag(Flag::option(
            "max-attempts",
            "N",
            "Attempts before the job is dead (default: the queue's)",
        ))
        .flag(Flag::option(
            "retry",
            "POLICY",
            "Retry backoff such as exp:1s*2..10m (default: the queue's)",
        ))
        .flag(Flag::option(
            "timeout",
            "DURATION",
            "Execution budget per attempt (default: the queue's)",
        ))
        .flag(
            Flag::option("tag", "TAG", "Tag the job")
                .short('t')
                .repeated(),
        )
        .flag(Flag::option(
            "tenant",
            "TENANT",
            "Tenant, for fair sharing within the queue",
        ))
        .flag(
            Flag::option("meta", "KEY=VALUE", "Free-form annotation")
                .short('m')
                .repeated(),
        )
}

/// Applies the flags of [`job_options`] to `spec`.
pub(crate) fn apply_job_options(ctx: &Ctx<'_>, m: &Matches, mut spec: JobSpec) -> Result<JobSpec> {
    spec = spec.payload(input::payload(ctx, m.value("payload"))?);
    if let Some(priority) = m.get::<i16>("priority")? {
        spec = spec.priority(priority);
    }
    if let Some(n) = m.get::<u32>("max-attempts")? {
        spec = spec.max_attempts(n);
    }
    if let Some(policy) = m.get::<BackoffPolicy>("retry")? {
        spec = spec.retry(policy);
    }
    if let Some(timeout) = m.get::<Duration>("timeout")? {
        spec = spec.timeout(timeout);
    }
    for tag in m.get_all::<String>("tag")? {
        spec = spec.tag(tag);
    }
    if let Some(tenant) = m.get::<TenantId>("tenant")? {
        spec = spec.tenant(tenant);
    }
    for pair in m.get_all::<KeyValue>("meta")? {
        spec = spec.meta(pair.key, pair.value);
    }
    Ok(spec)
}

/// `--unique KEY[:SCOPE]`. Keys often contain colons themselves
/// (`user:42`), so only a recognisable scope at the end is split off.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UniqueArg {
    key: String,
    scope: Option<UniqueScope>,
}

impl FromArg for UniqueArg {
    fn from_arg(text: &str) -> std::result::Result<UniqueArg, String> {
        let (key, scope) = if let Some((key, period)) = text.rsplit_once(":for:") {
            let scope = UniqueScope::from_arg(&format!("for:{period}"))?;
            (key, Some(scope))
        } else {
            match text.split_once(':') {
                Some((key, scope @ ("until_started" | "until_finished"))) => {
                    (key, Some(UniqueScope::from_arg(scope)?))
                }
                _ => (text, None),
            }
        };
        if key.is_empty() {
            return Err("the key must not be empty".to_owned());
        }
        Ok(UniqueArg {
            key: key.to_owned(),
            scope,
        })
    }
}

pub(crate) fn enqueue(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let queue: QueueName = m.require("queue")?;
    let kind: String = m.require("kind")?;
    let mut spec = apply_job_options(ctx, m, JobSpec::new(queue, kind))?;
    if let Some(delay) = m.get::<Duration>("delay")? {
        spec = spec.delay(delay);
    }
    if let Some(at) = m.get::<Timestamp>("run-at")? {
        spec = spec.run_at(at);
    }
    if let Some(unique) = m.get::<UniqueArg>("unique")? {
        let mut rule = Unique::new(unique.key, unique.scope.unwrap_or_default());
        if let Some(policy) = m.get::<OnConflict>("on-conflict")? {
            rule = rule.on_conflict(policy);
        }
        spec = spec.unique_with(rule);
    } else if m.given("on-conflict") {
        return Err(m.constraint("--on-conflict needs --unique").into());
    }
    if m.given("wait-timeout") && !m.flag("wait") {
        return Err(m.constraint("--wait-timeout needs --wait").into());
    }
    spec.validate()
        .map_err(|e| m.constraint(format!("invalid job: {e}")))?;

    let client = ctx.client()?;
    let result = client.jobs().enqueue(&spec)?;
    if m.flag("wait") {
        let timeout = m.get::<Duration>("wait-timeout")?;
        let job = wait_for(ctx, &client, result.id, timeout)?;
        return report_finished(ctx, &job);
    }
    if ctx.out.json {
        ctx.out.json(&result)?;
    } else if ctx.out.quiet {
        ctx.out.line(result.id)?;
    } else {
        let state = ctx.out.state(result.state.as_str());
        let line = match result.status {
            EnqueueStatus::Created => format!("enqueued {} on {} ({state})", result.id, spec.queue),
            EnqueueStatus::Duplicate => format!(
                "job {} already holds the unique key; nothing enqueued ({state})",
                result.id
            ),
            EnqueueStatus::Replaced => format!(
                "enqueued {} on {}, replacing the previous holder of the key ({state})",
                result.id, spec.queue
            ),
        };
        ctx.out.line(line)?;
    }
    Ok(())
}

pub(crate) fn spec() -> Command {
    let id = || Arg::required("id", "Job id");
    let ids = || Arg::required("id", "Job ids").many();
    Command::new("job", "inspect and manage jobs")
        .subcommand(
            Command::new("get", "show a job: state, attempts, errors, result")
                .alias("show")
                .arg(id()),
        )
        .subcommand(
            Command::new("list", "list jobs, newest first")
                .alias("ls")
                .flag(Flag::option("queue", "QUEUE", "Only this queue"))
                .flag(
                    Flag::option("state", "STATE", "Only these states (comma-separated)")
                        .short('s')
                        .repeated(),
                )
                .flag(Flag::option("kind", "KIND", "Only this kind"))
                .flag(Flag::option("tag", "TAG", "Only jobs with this tag"))
                .flag(Flag::option("tenant", "TENANT", "Only this tenant"))
                .flag(Flag::option(
                    "created-after",
                    "TIME",
                    "Only jobs created after this time",
                ))
                .flag(Flag::option(
                    "created-before",
                    "TIME",
                    "Only jobs created before this time",
                ))
                .flag(Flag::option("order", "ORDER", "asc (oldest first) or desc").default("desc"))
                .flag(Flag::option("limit", "N", "Jobs per page (server maximum 1000)").short('n'))
                .flag(
                    Flag::option("cursor", "CURSOR", "Continue after a previous page")
                        .conflicts_with("all"),
                )
                .flag(Flag::switch("all", "Follow every page to the end"))
                .example("belay job list --state dead,retrying --queue emails")
                .example("belay job list -q --state dead | xargs belay job retry"),
        )
        .subcommand(
            Command::new(
                "cancel",
                "cancel jobs; running ones stop at their next heartbeat",
            )
            .arg(ids()),
        )
        .subcommand(Command::new("retry", "send dead or cancelled jobs back to ready").arg(ids()))
        .subcommand(Command::new("promote", "make scheduled or retrying jobs ready now").arg(ids()))
        .subcommand(
            Command::new("delete", "delete finished jobs")
                .alias("rm")
                .arg(ids()),
        )
        .subcommand(
            Command::new("wait", "wait for a job to finish and print its result")
                .arg(id())
                .flag(Flag::option(
                    "timeout",
                    "DURATION",
                    "Give up after this long",
                )),
        )
}

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    match action {
        "get" => get(ctx, m),
        "list" => list(ctx, m),
        "cancel" | "retry" | "promote" | "delete" => act(ctx, m, action),
        "wait" => {
            let id: JobId = m.require("id")?;
            let client = ctx.client()?;
            let job = wait_for(ctx, &client, id, m.get("timeout")?)?;
            report_finished(ctx, &job)
        }
        other => unreachable!("job {other} is not declared"),
    }
}

fn get(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let id: JobId = m.require("id")?;
    let job = ctx.client()?.jobs().get(id)?;
    if ctx.out.json {
        ctx.out.json(&job)?;
    } else {
        let fields = job_fields(&job, ctx.now());
        ctx.out.fields(&fields)?;
    }
    Ok(())
}

/// Everything about a job, for `job get` and `--wait`.
pub(crate) fn job_fields(job: &Job, now: Timestamp) -> Fields {
    let spec = &job.spec;
    let mut f = Fields::new();
    f.add("id", job.id)
        .add("queue", &spec.queue)
        .add("kind", &spec.kind)
        .state("state", job.state.as_str())
        .add(
            "attempt",
            format!("{} of {}", job.attempt, job.max_attempts()),
        )
        .add("priority", spec.priority)
        .add("created", when(job.created_at, now));
    if !job.state.is_terminal() {
        f.add("run at", when(job.run_at, now));
    }
    f.opt("started", job.started_at.map(|t| when(t, now)))
        .opt("finished", job.finished_at.map(|t| when(t, now)))
        .opt(
            "runtime",
            job.runtime().filter(|_| job.is_terminal()).map(approx),
        );
    if let Some(lease) = &job.lease {
        f.add(
            "lease",
            format!(
                "held by {}, expires {}",
                lease.worker,
                relative(lease.expires_at, now)
            ),
        );
    }
    if job.cancel_requested {
        f.add(
            "cancel",
            "requested; the worker stops at its next heartbeat",
        );
    }
    if let Some(progress) = &job.progress {
        let pct = progress.percent.map(percent);
        let text = match (pct, &progress.message) {
            (Some(p), Some(msg)) => format!("{p} {msg}"),
            (Some(p), None) => p,
            (None, Some(msg)) => msg.clone(),
            (None, None) => String::new(),
        };
        f.add("progress", text);
    }
    f.opt("timeout", spec.timeout.map(format_duration))
        .opt("retry", spec.retry.as_ref())
        .opt(
            "unique",
            spec.unique
                .as_ref()
                .map(|u| format!("{} ({}, {})", u.key, u.scope, u.on_conflict)),
        );
    if !spec.tags.is_empty() {
        f.add("tags", spec.tags.join(", "));
    }
    f.opt("tenant", spec.tenant.as_ref());
    if !spec.meta.is_empty() {
        let meta: Vec<String> = spec.meta.iter().map(|(k, v)| format!("{k}={v}")).collect();
        f.add("meta", meta.join("\n"));
    }
    f.opt(
        "workflow",
        spec.workflow
            .as_ref()
            .map(|w| format!("{} step {}", w.id, w.step)),
    );
    if !spec.payload.is_null() {
        f.add("payload", belay_json::to_string_pretty(&spec.payload));
    }
    if let Some(result) = &job.result {
        f.add("result", belay_json::to_string_pretty(result));
    }
    if !job.errors.is_empty() {
        let lines: Vec<String> = job
            .errors
            .iter()
            .map(|e| {
                let mut line = format!("#{} {}", e.attempt, relative(e.at, now));
                if let Some(worker) = &e.worker {
                    line.push_str(&format!(" on {worker}"));
                }
                line.push_str(&format!(": {}", e.message));
                if let Some(kind) = &e.kind {
                    line.push_str(&format!(" [{kind}]"));
                }
                if !e.retryable {
                    line.push_str(" (not retryable)");
                }
                line
            })
            .collect();
        f.add("errors", lines.join("\n"));
    }
    f
}

fn filter(m: &Matches) -> Result<JobFilter> {
    let mut filter = JobFilter::new();
    filter.queue = m.get::<QueueName>("queue")?;
    filter.states = m.get_list::<JobState>("state")?;
    filter.kind = m.get::<String>("kind")?;
    filter.tag = m.get::<String>("tag")?;
    filter.tenant = m.get::<TenantId>("tenant")?;
    filter.created_after = m.get::<Timestamp>("created-after")?;
    filter.created_before = m.get::<Timestamp>("created-before")?;
    filter.order = m.get_or("order", SortOrder::Desc)?;
    filter.limit = m.get::<u32>("limit")?;
    filter.cursor = match m.value("cursor") {
        Some(text) => Some(
            text.parse()
                .map_err(|e| m.constraint(format!("invalid value {text:?} for --cursor: {e}")))?,
        ),
        None => None,
    };
    Ok(filter)
}

fn list(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let filter = filter(m)?;
    let client = ctx.client()?;
    let (jobs, next) = if m.flag("all") {
        let jobs: Vec<Job> = client
            .jobs()
            .list_all(filter)
            .collect::<std::result::Result<_, _>>()?;
        (jobs, None)
    } else {
        let page = client.jobs().list(&filter)?;
        (page.items, page.next_cursor)
    };
    if ctx.out.json {
        let page = belay_proto::api::Page::new(jobs, next);
        return Ok(ctx.out.json(&page)?);
    }
    if ctx.out.quiet {
        for job in &jobs {
            ctx.out.line(job.id)?;
        }
        return Ok(());
    }
    let now = ctx.now();
    let mut grid = Grid::new(&["ID", "QUEUE", "KIND", "STATE", "TRIES", "AGE", "INFO"])
        .right("TRIES")
        .paint("STATE", state_style)
        .max_width("INFO", 60);
    for job in &jobs {
        grid.push([
            job.id.to_string(),
            job.queue().to_string(),
            job.kind().to_owned(),
            job.state.to_string(),
            job.attempt.to_string(),
            relative(job.created_at, now),
            info(job, now),
        ]);
    }
    ctx.out.grid(&grid, "no jobs match")?;
    if let Some(cursor) = next {
        ctx.out.note(format!(
            "showing {}; continue with --cursor {cursor}, or use --all",
            belay_common::text::plural(jobs.len() as u64, "job")
        ))?;
    }
    Ok(())
}
