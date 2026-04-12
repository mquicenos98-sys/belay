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

pub(crate) fn run(ctx: &mut Ctx<'_>, m: &Matches, action: &str) -> Result<()> {
    match action {
        "add" => add(ctx, m),
        "list" => list(ctx),
        _ => {
            let id: ScheduleId = m.require("id")?;
            let client = ctx.client()?;
            let schedules = client.schedules();
            match action {
                "get" => {
                    let schedule = schedules.get(&id)?;
                    if ctx.out.json {
                        ctx.out.json(&schedule)?;
                    } else {
                        let fields = schedule_fields(&schedule, ctx.now());
                        ctx.out.fields(&fields)?;
                    }
                }
                "pause" | "resume" => {
                    if action == "pause" {
                        schedules.pause(&id)?;
                    } else {
                        schedules.resume(&id)?;
                    }
                    let state = if action == "pause" {
                        "paused"
                    } else {
                        "active"
                    };
                    if ctx.out.json {
                        ctx.out
                            .json_value(&json!({"schedule": id.as_str(), "state": state}))?;
                    }
                    ctx.out
                        .done(format!("schedule {id} is now {}", ctx.out.state(state)))?;
                }
                "rm" => {
                    schedules.delete(&id)?;
                    if ctx.out.json {
                        ctx.out
                            .json_value(&json!({"schedule": id.as_str(), "deleted": true}))?;
                    }
                    ctx.out.done(format!("deleted schedule {id}"))?;
                }
                "trigger" => {
                    let job = schedules.trigger(&id)?;
                    if ctx.out.json {
                        ctx.out.json_value(
                            &json!({"schedule": id.as_str(), "job": job.to_string()}),
                        )?;
                    } else if ctx.out.quiet {
                        ctx.out.line(job)?;
                    } else {
                        ctx.out.line(format!("fired {id}: enqueued {job}"))?;
                    }
                }
                "preview" => {
                    let n: u32 = m.get_or("count", 10)?;
                    let times = schedules.preview(&id, n)?;
                    if ctx.out.json {
                        let items = times
                            .iter()
                            .map(|t| Value::String(t.to_rfc3339()))
                            .collect();
                        ctx.out.json_value(&Value::Array(items))?;
                    } else {
                        let tz = Tz::parse(&schedules.get(&id)?.spec.timezone).unwrap_or(Tz::Utc);
                        print_times(ctx, &times, &tz)?;
                    }
                }
                other => unreachable!("schedule {other} is not declared"),
            }
            Ok(())
        }
    }
}

/// Fire times in the schedule's zone with their distance from now.
fn print_times(ctx: &mut Ctx<'_>, times: &[Timestamp], tz: &Tz) -> Result<()> {
    if times.is_empty() {
        return Ok(ctx.out.note("no upcoming fire times")?);
    }
    let now = ctx.now();
    let mut grid = Grid::new(&["TIME", "UTC", "IN"]);
    for t in times {
        grid.push([
            tz.format_rfc3339(t.as_secs()),
            t.to_rfc3339(),
            relative(*t, now),
        ]);
    }
    Ok(ctx.out.grid(&grid, "")?)
}

fn add(ctx: &mut Ctx<'_>, m: &Matches) -> Result<()> {
    let spec = if let Some(path) = m.value("file") {
        if let Some(flag) = INLINE_FLAGS.iter().find(|f| m.given(f)) {
            return Err(m
                .constraint(format!(
                    "--file cannot be combined with --{flag}; put it in the file"
                ))
                .into());
        }
        let spec: ScheduleSpec = input::spec_file(ctx, path)?;
        if let Some(id) = m.get::<ScheduleId>("id")? {
            if id != spec.id {
                return Err(m
                    .constraint(format!("the file defines schedule {}, not {id}", spec.id))
                    .into());
            }
        }
        spec
    } else {
        inline_spec(ctx, m)?
    };
    check_expression(m, &spec)?;

    let client = ctx.client()?;
    let schedule = if m.flag("replace") {
        client.schedules().put(&spec)?
    } else {
        client.schedules().create(&spec)?
    };
    if ctx.out.json {
        ctx.out.json(&schedule)?;
    } else if ctx.out.quiet {
        ctx.out.line(schedule.id())?;
    } else {
        let verb = if m.flag("replace") {
            "saved"
        } else {
            "created"
        };
        let next = match schedule.next_fire_at {
            Some(t) => format!("next fire {}", when(t, ctx.now())),
            None => "it will not fire again".to_owned(),
        };
        ctx.out
            .line(format!("{verb} schedule {}; {next}", schedule.id()))?;
    }
    Ok(())
}

fn inline_spec(ctx: &Ctx<'_>, m: &Matches) -> Result<ScheduleSpec> {
    let Some(id) = m.get::<ScheduleId>("id")? else {
        return Err(m
            .constraint("give the schedule's <ID>, or load a spec with --file")
            .into());
    };
    let cron: String = m.require("cron")?;
    let queue: QueueName = m.require("queue")?;
    let kind: String = m.require("kind")?;
    let template = apply_job_options(ctx, m, JobSpec::new(queue, kind))?;
    let mut spec = ScheduleSpec::new(id, cron, template)
        .timezone(m.value("tz").unwrap_or(ScheduleSpec::DEFAULT_TIMEZONE));
    if let Some(policy) = m.get::<MisfirePolicy>("misfire")? {
        spec = spec.misfire(policy);
    }
    if let Some(policy) = m.get::<OverlapPolicy>("overlap")? {
        spec = spec.overlap(policy);
    }
    if let Some(jitter) = m.get::<Duration>("jitter")? {
        spec = spec.jitter(jitter);
    }
    if let Some(calendar) = m.get::<String>("calendar")? {
        spec = spec.calendar(calendar);
    }
    if let Some(text) = m.get::<String>("description")? {
        spec = spec.description(text);
    }
    spec = spec.between(m.get("starts-at")?, m.get("ends-at")?);
    spec.paused = m.flag("paused");
    spec.validate()
        .map_err(|e| m.constraint(format!("invalid schedule: {e}")))?;
    Ok(spec)
}
