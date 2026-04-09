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
