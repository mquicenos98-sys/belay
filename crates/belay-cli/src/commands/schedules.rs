//! `belay schedule ...`: cron schedules that enqueue a job at every fire
//! time.

use std::time::Duration;

use belay_common::{format_duration, Timestamp};
use belay_cron::{CronExpr, Tz};
use belay_json::{json, ToJson, Value};
use belay_proto::{
    JobSpec, MisfirePolicy, OverlapPolicy, QueueName, Schedule, ScheduleId, ScheduleSpec, Validate,
};

use super::jobs::{apply_job_options, job_options};
use super::Ctx;
use crate::args::{Arg, Command, Flag, Matches};
use crate::error::{Error, Result};
use crate::input;
use crate::output::human::{relative, when};
use crate::output::{state_style, Fields, Grid};

/// Flags that describe a schedule inline, all incompatible with `--file`.
const INLINE_FLAGS: &[&str] = &[
    "cron",
    "queue",
    "kind",
    "tz",
    "misfire",
    "overlap",
    "jitter",
    "calendar",
    "description",
    "starts-at",
    "ends-at",
    "paused",
    "payload",
    "priority",
    "max-attempts",
    "retry",
    "timeout",
    "tag",
    "tenant",
    "meta",
];

pub(crate) fn spec() -> Command {
    let id = || Arg::required("id", "Schedule id");
    let add = Command::new("add", "create a schedule from flags or a file")
        .details(
            "Either give the schedule inline (ID, --cron, --queue and --kind, plus any job\n\
             and policy flags) or load a whole spec with --file (JSON or TOML, the same\n\
             fields as the API and the [[schedules]] configuration table). With --replace\n\
             an existing schedule of the same id is overwritten instead of refused.",
        )
        .arg(Arg::optional(
            "id",
            "Schedule id: lowercase letters, digits, '_', '.', '-'",
        ))
        .flag(Flag::option(
            "file",
            "PATH",
            "Load the spec from a JSON or TOML file",
        ))
        .flag(Flag::option(
            "cron",
            "EXPR",
            "Cron expression (5-7 fields or @daily and friends)",
        ))
        .flag(Flag::option(
            "queue",
            "QUEUE",
            "Queue for the jobs it enqueues",
        ))
        .flag(Flag::option("kind", "KIND", "Kind of the jobs it enqueues"))
        .flag(Flag::option("tz", "ZONE", "Time zone of the expression").default("UTC"))
        .flag(Flag::option(
            "misfire",
            "POLICY",
            "After downtime: skip, fire_once or fire_all[:N]",
        ))
        .flag(Flag::option(
            "overlap",
            "POLICY",
            "While the last job runs: allow, skip or cancel_previous",
        ))
        .flag(Flag::option(
            "jitter",
            "DURATION",
            "Random delay of up to this much per fire",
        ))
        .flag(Flag::option(
            "calendar",
            "NAME",
            "Only fire when this calendar allows",
        ))
        .flag(Flag::option("description", "TEXT", "Free text for humans"))
        .flag(Flag::option(
            "starts-at",
            "TIME",
            "Do not fire before this time",
        ))
        .flag(Flag::option(
            "ends-at",
            "TIME",
            "Do not fire at or after this time",
        ))
        .flag(Flag::switch("paused", "Create it paused"))
        .flag(Flag::switch(
            "replace",
            "Overwrite an existing schedule with this id",
        ));
    Command::new("schedule", "manage cron schedules")
        .subcommand(
            job_options(add)
                .example("belay schedule add nightly --cron '0 2 * * *' --tz America/Bogota --queue reports --kind build")
                .example("belay schedule add --file schedules/nightly.toml --replace"),
        )
        .subcommand(Command::new("list", "list schedules with their next fire time").alias("ls"))
        .subcommand(Command::new("get", "show a schedule").alias("show").arg(id()))
        .subcommand(Command::new("pause", "stop a schedule from firing").arg(id()))
        .subcommand(Command::new("resume", "let a paused schedule fire again").arg(id()))
        .subcommand(Command::new("rm", "delete a schedule (its jobs stay)").alias("delete").arg(id()))
        .subcommand(Command::new("trigger", "fire a schedule now, outside its expression").arg(id()))
        .subcommand(
            Command::new("preview", "show a schedule's next fire times")
                .arg(id())
                .flag(Flag::option("count", "N", "How many").short('n').default("10")),
        )
}
